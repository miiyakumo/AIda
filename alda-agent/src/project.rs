use crate::alda::{AldaCheck, CheckStatus};
use crate::conversation::{Conversation, ConversationMessage, ConversationState};
use crate::instructions::{
    CreationMode, DurationConstraint, InstructionProfile, ProjectPreferences,
};
use crate::skills::{QualifiedSkillId, SkillOrigin};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const PROJECT_FILE: &str = "project.json";
const CURRENT_FILE: &str = "current.alda";
const WORK_FILE: &str = "work.alda";
const REVISION_FILE: &str = "revision.alda";
const WORK_MIDI_FILE: &str = "work.mid";
const WORK_WAV_FILE: &str = "work.wav";

mod persisted_check_status {
    use crate::alda::CheckStatus;
    use serde::{Deserialize, Deserializer, Serializer};

    #[allow(clippy::trivially_copy_pass_by_ref)]
    pub fn serialize<S>(status: &CheckStatus, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(match status {
            CheckStatus::Pass => "通过",
            CheckStatus::Fail => "失败",
            CheckStatus::Unchecked => "未检查",
        })
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<CheckStatus, D::Error>
    where
        D: Deserializer<'de>,
    {
        match String::deserialize(deserializer)?.as_str() {
            "通过" | "pass" => Ok(CheckStatus::Pass),
            "失败" | "fail" => Ok(CheckStatus::Fail),
            "未检查" | "unchecked" => Ok(CheckStatus::Unchecked),
            value => Err(serde::de::Error::custom(format!("未知检查状态 {value:?}"))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckRecord {
    pub name: String,
    #[serde(with = "persisted_check_status")]
    pub status: CheckStatus,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FormPlan {
    pub target_duration_secs: f64,
    pub sections: Vec<FormSection>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FormSection {
    pub id: String,
    pub target_start_secs: f64,
    pub target_end_secs: f64,
    pub function: String,
    pub material_action: MaterialAction,
    pub energy: SectionEnergy,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MaterialAction {
    Introduce,
    Develop,
    Contrast,
    Reprise,
    Close,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SectionEnergy {
    Low,
    Medium,
    High,
    Peak,
}

impl FormPlan {
    pub fn validate(&self) -> Result<()> {
        if !self.target_duration_secs.is_finite() || self.target_duration_secs <= 0.0 {
            bail!("form_plan.target_duration_secs 必须是正有限数");
        }
        if !(4..=10).contains(&self.sections.len()) {
            bail!("form_plan.sections 必须包含 4–10 个段落");
        }

        let mut previous_end = 0.0;
        let mut ids = std::collections::BTreeSet::new();
        for (index, section) in self.sections.iter().enumerate() {
            let valid_id = section.id.bytes().enumerate().all(|(offset, byte)| {
                if offset == 0 {
                    byte.is_ascii_lowercase()
                } else {
                    byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'
                }
            });
            if !valid_id || !ids.insert(section.id.as_str()) {
                bail!("form_plan 段落 id 必须唯一并匹配 [a-z][a-z0-9_]*");
            }
            if section.function.trim().is_empty() {
                bail!("form_plan.sections[{index}].function 不能为空");
            }
            if !section.target_start_secs.is_finite()
                || !section.target_end_secs.is_finite()
                || section.target_start_secs < 0.0
                || section.target_end_secs <= section.target_start_secs
            {
                bail!("form_plan.sections[{index}] 的时间区间无效");
            }
            if (section.target_start_secs - previous_end).abs() > 0.001 {
                bail!("form_plan 段落必须从 0 开始且连续、不重叠");
            }
            previous_end = section.target_end_secs;
        }
        if (previous_end - self.target_duration_secs).abs() > 0.001 {
            bail!("form_plan 最后一段结束时间必须等于 target_duration_secs");
        }
        Ok(())
    }
}

impl From<&AldaCheck> for CheckRecord {
    fn from(check: &AldaCheck) -> Self {
        Self {
            name: check.name.to_string(),
            status: check.status,
            detail: check.detail.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VersionMeta {
    pub created_at: String,
    pub summary: String,
    pub checks: Vec<CheckRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub form_plan: Option<FormPlan>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkingScoreKind {
    Draft,
    Candidate,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum AgentMode {
    #[default]
    Single,
    CompositionAb,
}

impl std::fmt::Display for AgentMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Single => formatter.write_str("single"),
            Self::CompositionAb => formatter.write_str("composition-ab"),
        }
    }
}

impl std::str::FromStr for AgentMode {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim() {
            "single" => Ok(Self::Single),
            "composition-ab" => Ok(Self::CompositionAb),
            other => bail!("无效的 Agent 模式: {other}（应为 single 或 composition-ab）"),
        }
    }
}

impl std::fmt::Display for WorkingScoreKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Draft => write!(f, "草稿"),
            Self::Candidate => write!(f, "完整候选"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkingScore {
    pub kind: WorkingScoreKind,
    pub summary: String,
    pub checks: Vec<CheckRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub form_plan: Option<FormPlan>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PendingRevision {
    pub kind: WorkingScoreKind,
    pub summary: String,
    pub checks: Vec<CheckRecord>,
    pub source_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub form_plan: Option<FormPlan>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub project_name: String,
    #[serde(default)]
    agent_mode: AgentMode,
    #[serde(default)]
    instruction_profile: InstructionProfile,
    #[serde(flatten)]
    preferences: ProjectPreferences,
    current_version: u32,
    versions: BTreeMap<u32, VersionMeta>,
    working_score: Option<WorkingScore>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_revision: Option<PendingRevision>,
    conversation: Conversation,
    #[serde(skip)]
    root: PathBuf,
}

impl Project {
    pub fn load_or_create(
        root: PathBuf,
        project_name: &str,
        _initial_content: &str,
    ) -> Result<Self> {
        ensure_safe_project_name(project_name)?;
        let project_file = root.join(PROJECT_FILE);
        if project_file.exists() {
            let content = fs::read_to_string(&project_file)
                .with_context(|| format!("无法读取 {}", project_file.display()))?;
            let mut project: Self = serde_json::from_str(&content)
                .with_context(|| format!("无法解析 {}", project_file.display()))?;
            project.root = root;
            let compacted = project.conversation.compact_provider_trace();
            project.validate_metadata()?;
            project.recover_projections()?;
            if compacted {
                project.write_metadata()?;
            }
            return Ok(project);
        }

        fs::create_dir_all(root.join("versions")).context("无法创建 versions 目录")?;
        fs::create_dir_all(root.join("exports")).context("无法创建 exports 目录")?;
        let project = Self {
            project_name: project_name.to_string(),
            agent_mode: AgentMode::default(),
            instruction_profile: InstructionProfile::default(),
            preferences: ProjectPreferences::default(),
            current_version: 0,
            versions: BTreeMap::new(),
            working_score: None,
            pending_revision: None,
            conversation: Conversation::default(),
            root,
        };
        project.write_metadata()?;
        Ok(project)
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn current_version(&self) -> u32 {
        self.current_version
    }

    #[must_use]
    pub fn versions(&self) -> &BTreeMap<u32, VersionMeta> {
        &self.versions
    }

    #[must_use]
    pub fn working_score(&self) -> Option<&WorkingScore> {
        self.working_score.as_ref()
    }

    #[must_use]
    pub fn pending_revision(&self) -> Option<&PendingRevision> {
        self.pending_revision.as_ref()
    }

    #[must_use]
    pub fn current_form_plan(&self) -> Option<&FormPlan> {
        self.versions
            .get(&self.current_version)
            .and_then(|version| version.form_plan.as_ref())
    }

    #[must_use]
    pub fn preferences(&self) -> &ProjectPreferences {
        &self.preferences
    }

    #[must_use]
    pub fn mode(&self) -> CreationMode {
        self.preferences.mode
    }

    #[must_use]
    pub const fn agent_mode(&self) -> AgentMode {
        self.agent_mode
    }

    pub fn configure_agent_mode(&mut self, mode: AgentMode) -> Result<()> {
        self.agent_mode = mode;
        self.write_metadata()
    }

    #[must_use]
    pub fn target_duration_secs(&self) -> Option<DurationConstraint> {
        self.preferences.target_duration_secs
    }

    #[must_use]
    pub fn included_instruments(&self) -> &[String] {
        &self.preferences.included_instruments
    }

    #[must_use]
    pub fn excluded_instruments(&self) -> &[String] {
        &self.preferences.excluded_instruments
    }

    #[must_use]
    pub fn instruction_profile(&self) -> &InstructionProfile {
        &self.instruction_profile
    }

    #[must_use]
    pub fn conversation(&self) -> &Conversation {
        &self.conversation
    }

    pub fn add_user_message(&mut self, content: &str) -> Result<()> {
        if content.trim().is_empty() {
            bail!("对话消息不能为空");
        }
        self.conversation.add_user_message(content.to_string());
        self.write_metadata()
    }

    pub fn prepare_user_message(&mut self, content: &str) -> Result<()> {
        self.prepare_user_message_with_requirement(content, false)
    }

    pub fn prepare_user_message_with_requirement(
        &mut self,
        content: &str,
        require_candidate: bool,
    ) -> Result<()> {
        if content.trim().is_empty() {
            bail!("对话消息不能为空");
        }
        let is_same_pending_request = self.conversation.state()
            == ConversationState::RequestPending
            && self.conversation.last_user_message() == Some(content);
        if !is_same_pending_request {
            self.conversation.add_user_message(content.to_string());
        }
        self.conversation.set_pending_candidate(require_candidate);
        self.conversation
            .set_state(ConversationState::RequestPending);
        self.write_metadata()
    }

    pub fn finish_agent_turn(
        &mut self,
        assistant_text: String,
        state: ConversationState,
    ) -> Result<()> {
        self.conversation.add_assistant_message(assistant_text);
        self.conversation.set_state(state);
        self.write_metadata()
    }

    pub fn replace_conversation(
        &mut self,
        messages: Vec<ConversationMessage>,
        state: ConversationState,
    ) -> Result<()> {
        self.conversation.replace_messages(messages);
        self.conversation.set_state(state);
        self.write_metadata()
    }

    pub fn enable_advisory_skill(&mut self, id: QualifiedSkillId) -> Result<bool> {
        if matches!(id.origin(), SkillOrigin::Builtin) {
            bail!("内建 workflow 固定启用，不能作为 Advisory Skill 配置");
        }
        if self
            .instruction_profile
            .enabled_advisory_skills
            .contains(&id)
        {
            return Ok(false);
        }
        let previous = self.instruction_profile.clone();
        self.instruction_profile.enabled_advisory_skills.push(id);
        self.instruction_profile.enabled_advisory_skills.sort();
        if let Err(error) = self.write_metadata() {
            self.instruction_profile = previous;
            return Err(error);
        }
        Ok(true)
    }

    pub fn disable_advisory_skill(&mut self, id: &QualifiedSkillId) -> Result<bool> {
        if matches!(id.origin(), SkillOrigin::Builtin) {
            bail!("内建 workflow 固定启用，不能禁用");
        }
        let previous = self.instruction_profile.clone();
        self.instruction_profile
            .enabled_advisory_skills
            .retain(|enabled| enabled != id);
        if self.instruction_profile == previous {
            return Ok(false);
        }
        if let Err(error) = self.write_metadata() {
            self.instruction_profile = previous;
            return Err(error);
        }
        Ok(true)
    }

    pub fn configure(&mut self, preferences: &ProjectPreferences) -> Result<()> {
        let normalized = preferences.normalized();
        normalized.validate()?;
        let previous = std::mem::replace(&mut self.preferences, normalized);
        if let Err(error) = self.write_metadata() {
            self.preferences = previous;
            return Err(error);
        }
        Ok(())
    }

    pub fn current_code(&self) -> Result<String> {
        if self.current_version == 0 {
            bail!("项目还没有有效版本");
        }
        self.version_code(self.current_version)
    }

    pub fn current_version_path(&self) -> Result<PathBuf> {
        if self.current_version == 0 {
            bail!("项目还没有有效版本");
        }
        Ok(self.version_path(self.current_version))
    }

    pub fn version_code(&self, version: u32) -> Result<String> {
        if !self.versions.contains_key(&version) {
            bail!("版本 {version} 不存在");
        }
        fs::read_to_string(self.version_path(version))
            .with_context(|| format!("无法读取版本 {version}"))
    }

    pub fn working_code(&self) -> Result<String> {
        if self.working_score.is_none() {
            bail!("项目没有工作乐谱");
        }
        fs::read_to_string(self.root.join(WORK_FILE)).context("无法读取 work.alda")
    }

    pub fn working_path(&self) -> Result<PathBuf> {
        if self.working_score.is_none() {
            bail!("项目没有工作乐谱");
        }
        Ok(self.root.join(WORK_FILE))
    }

    pub fn revision_code(&self) -> Result<String> {
        if self.pending_revision.is_none() {
            bail!("项目没有待修正候选");
        }
        fs::read_to_string(self.root.join(REVISION_FILE)).context("无法读取 revision.alda")
    }

    pub fn revision_path(&self) -> Result<PathBuf> {
        if self.pending_revision.is_none() {
            bail!("项目没有待修正候选");
        }
        Ok(self.root.join(REVISION_FILE))
    }

    pub fn save_pending_revision(
        &mut self,
        alda_code: &str,
        kind: WorkingScoreKind,
        summary: &str,
        checks: &[AldaCheck],
    ) -> Result<()> {
        self.save_pending_revision_with_plan(alda_code, kind, summary, checks, None)
    }

    pub fn save_pending_revision_with_plan(
        &mut self,
        alda_code: &str,
        kind: WorkingScoreKind,
        summary: &str,
        checks: &[AldaCheck],
        form_plan: Option<FormPlan>,
    ) -> Result<()> {
        if alda_code.trim().is_empty() {
            bail!("不能保存空的待修正候选");
        }
        let path = self.root.join(REVISION_FILE);
        let previous_code = fs::read(&path).ok();
        let previous_revision = self.pending_revision.clone();
        write_atomic(&path, alda_code.as_bytes())?;
        self.pending_revision = Some(PendingRevision {
            kind,
            summary: summary.to_string(),
            checks: checks.iter().map(CheckRecord::from).collect(),
            source_hash: format!("{:x}", Sha256::digest(alda_code.as_bytes())),
            form_plan,
        });
        if let Err(error) = self.write_metadata() {
            self.pending_revision = previous_revision;
            if let Some(previous_code) = previous_code {
                let _ = write_atomic(&path, &previous_code);
            } else {
                let _ = fs::remove_file(&path);
            }
            return Err(error);
        }
        Ok(())
    }

    pub fn clear_pending_revision(&mut self) -> Result<()> {
        if self.pending_revision.is_none() {
            return Ok(());
        }
        let previous = self.pending_revision.take();
        if let Err(error) = self.write_metadata() {
            self.pending_revision = previous;
            return Err(error);
        }
        self.remove_revision_projection()
    }

    pub fn save_working_score(
        &mut self,
        alda_code: &str,
        kind: WorkingScoreKind,
        summary: &str,
        checks: &[AldaCheck],
    ) -> Result<()> {
        self.save_working_score_with_plan(alda_code, kind, summary, checks, None)
    }

    pub fn save_working_score_with_plan(
        &mut self,
        alda_code: &str,
        kind: WorkingScoreKind,
        summary: &str,
        checks: &[AldaCheck],
        form_plan: Option<FormPlan>,
    ) -> Result<()> {
        self.save_working_score_inner(alda_code, kind, summary, checks, form_plan, None)
    }

    pub fn save_rendered_candidate(
        &mut self,
        alda_code: &str,
        summary: &str,
        checks: &[AldaCheck],
        midi_source: &Path,
        wav_source: &Path,
    ) -> Result<()> {
        self.save_rendered_candidate_with_plan(
            alda_code,
            summary,
            checks,
            midi_source,
            wav_source,
            None,
        )
    }

    pub fn save_rendered_candidate_with_plan(
        &mut self,
        alda_code: &str,
        summary: &str,
        checks: &[AldaCheck],
        midi_source: &Path,
        wav_source: &Path,
        form_plan: Option<FormPlan>,
    ) -> Result<()> {
        if !midi_source.is_file() {
            bail!("候选 MIDI 不存在: {}", midi_source.display());
        }
        if !wav_source.is_file() {
            bail!("候选 WAV 不存在: {}", wav_source.display());
        }
        self.save_working_score_inner(
            alda_code,
            WorkingScoreKind::Candidate,
            summary,
            checks,
            form_plan,
            Some((midi_source, wav_source)),
        )
    }

    fn save_working_score_inner(
        &mut self,
        alda_code: &str,
        kind: WorkingScoreKind,
        summary: &str,
        checks: &[AldaCheck],
        form_plan: Option<FormPlan>,
        artifacts: Option<(&Path, &Path)>,
    ) -> Result<()> {
        self.validate_settings()?;
        if alda_code.trim().is_empty() {
            bail!("不能保存空工作乐谱");
        }
        if checks.iter().any(|check| check.status == CheckStatus::Fail) {
            bail!("检查未全部通过，不能保存工作乐谱");
        }
        if kind == WorkingScoreKind::Draft && artifacts.is_some() {
            bail!("草稿不能保存渲染产物");
        }
        let work_path = self.root.join(WORK_FILE);
        let midi_path = self.work_midi_path();
        let wav_path = self.work_wav_path();
        let targets = [&work_path, &midi_path, &wav_path];
        let backup = tempfile::tempdir_in(&self.root).context("无法创建工作稿备份目录")?;
        let backup_paths = [
            backup.path().join(WORK_FILE),
            backup.path().join(WORK_MIDI_FILE),
            backup.path().join(WORK_WAV_FILE),
        ];
        let mut backed_up = [false; 3];
        for (index, (target, saved)) in targets.iter().zip(&backup_paths).enumerate() {
            if target.exists() {
                if let Err(error) = fs::rename(target, saved) {
                    restore_projections(&targets, &backup_paths, backed_up);
                    return Err(error).with_context(|| format!("无法备份 {}", target.display()));
                }
                backed_up[index] = true;
            }
        }
        let previous_working = self.working_score.clone();
        let previous_revision = self.pending_revision.clone();
        let write_result = (|| {
            write_atomic(&work_path, alda_code.as_bytes())?;
            if let Some((midi_source, wav_source)) = artifacts {
                copy_atomic(midi_source, &midi_path)?;
                copy_atomic(wav_source, &wav_path)?;
            }
            Ok(())
        })();
        if let Err(error) = write_result {
            restore_projections(&targets, &backup_paths, backed_up);
            return Err(error);
        }
        self.working_score = Some(WorkingScore {
            kind,
            summary: summary.to_string(),
            checks: checks.iter().map(CheckRecord::from).collect(),
            form_plan,
        });
        self.pending_revision = None;
        if let Err(error) = self.write_metadata() {
            self.working_score = previous_working;
            self.pending_revision = previous_revision;
            restore_projections(&targets, &backup_paths, backed_up);
            return Err(error);
        }
        let _ = self.remove_revision_projection();
        Ok(())
    }

    #[must_use]
    pub fn work_midi_path(&self) -> PathBuf {
        self.root.join("exports").join(WORK_MIDI_FILE)
    }

    #[must_use]
    pub fn work_wav_path(&self) -> PathBuf {
        self.root.join("exports").join(WORK_WAV_FILE)
    }

    pub fn update_working_checks(&mut self, checks: &[AldaCheck]) -> Result<()> {
        let working = self
            .working_score
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("项目没有工作乐谱"))?;
        working.checks = checks.iter().map(CheckRecord::from).collect();
        self.write_metadata()
    }

    pub fn accept_working_score(&mut self) -> Result<u32> {
        let working = self
            .working_score
            .clone()
            .ok_or_else(|| anyhow::anyhow!("项目没有可接受的完整候选"))?;
        if working.kind != WorkingScoreKind::Candidate {
            bail!("当前工作乐谱是草稿，不能接受为有效版本");
        }
        if working
            .checks
            .iter()
            .any(|check| check.status == CheckStatus::Fail)
        {
            bail!("完整候选检查未全部通过，不能接受为有效版本");
        }
        let code = self.working_code()?;
        self.save_version_records(
            &code,
            &working.summary,
            &working.checks,
            working.form_plan,
            true,
        )
    }

    pub fn discard_working_score(&mut self) -> Result<()> {
        if self.working_score.is_none() {
            bail!("项目没有可放弃的工作乐谱");
        }
        self.clear_working_score()
    }

    pub fn save_version(
        &mut self,
        alda_code: &str,
        summary: &str,
        checks: &[AldaCheck],
    ) -> Result<u32> {
        self.validate_settings()?;
        if alda_code.trim().is_empty() {
            bail!("不能保存空乐谱");
        }
        if checks.iter().any(|check| check.status == CheckStatus::Fail) {
            bail!("检查未全部通过，不能创建有效版本");
        }
        let records = checks.iter().map(CheckRecord::from).collect::<Vec<_>>();
        self.save_version_records(alda_code, summary, &records, None, false)
    }

    fn save_version_records(
        &mut self,
        alda_code: &str,
        summary: &str,
        checks: &[CheckRecord],
        form_plan: Option<FormPlan>,
        clear_working: bool,
    ) -> Result<u32> {
        if self.current_version > 0 && self.version_code(self.current_version)? == alda_code {
            bail!("新乐谱与当前版本相同，未创建新版本");
        }

        let next = self.versions.keys().next_back().copied().unwrap_or(0) + 1;
        let version_path = self.version_path(next);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&version_path)
            .with_context(|| format!("无法创建版本文件 {}", version_path.display()))?;
        file.write_all(alda_code.as_bytes())
            .context("无法写入版本文件")?;

        if let Err(error) = write_atomic(&self.root.join(CURRENT_FILE), alda_code.as_bytes()) {
            let _ = fs::remove_file(&version_path);
            return Err(error);
        }
        let previous_version = self.current_version;
        let previous_working = self.working_score.clone();
        self.current_version = next;
        self.versions.insert(
            next,
            VersionMeta {
                created_at: timestamp(),
                summary: summary.to_string(),
                checks: checks.to_vec(),
                form_plan,
            },
        );
        if clear_working {
            self.working_score = None;
        }
        if let Err(error) = self.write_metadata() {
            self.current_version = previous_version;
            self.versions.remove(&next);
            self.working_score = previous_working;
            let _ = fs::remove_file(&version_path);
            let _ = self.repair_current_projection();
            return Err(error);
        }
        if clear_working {
            let _ = self.remove_work_projection();
        }
        Ok(next)
    }

    fn clear_working_score(&mut self) -> Result<()> {
        let previous = self.working_score.take();
        if let Err(error) = self.write_metadata() {
            self.working_score = previous;
            return Err(error);
        }
        self.remove_work_projection()
    }

    fn remove_work_projection(&self) -> Result<()> {
        for path in [
            self.root.join(WORK_FILE),
            self.work_midi_path(),
            self.work_wav_path(),
        ] {
            if path.exists() {
                fs::remove_file(&path).with_context(|| format!("无法删除 {}", path.display()))?;
            }
        }
        Ok(())
    }

    pub fn restore_version(&mut self, version: u32) -> Result<()> {
        self.validate_settings()?;
        let code = self.version_code(version)?;
        write_atomic(&self.root.join(CURRENT_FILE), code.as_bytes())?;
        let previous = self.current_version;
        self.current_version = version;
        if let Err(error) = self.write_metadata() {
            self.current_version = previous;
            let _ = self.repair_current_projection();
            return Err(error);
        }
        Ok(())
    }

    pub fn export_alda_version(&self, version: u32) -> Result<PathBuf> {
        let code = self.version_code(version)?;
        let path = self
            .root
            .join("exports")
            .join(format!("version-{version:04}.alda"));
        write_atomic(&path, code.as_bytes())?;
        Ok(path)
    }

    pub fn export_alda(&self) -> Result<PathBuf> {
        self.export_alda_version(self.current_version)
    }

    pub fn midi_export_path(&self) -> Result<PathBuf> {
        if self.current_version == 0 {
            bail!("项目还没有有效版本");
        }
        Ok(self
            .root
            .join("exports")
            .join(format!("version-{:04}.mid", self.current_version)))
    }

    pub fn midi_export_path_for(&self, version: u32) -> Result<PathBuf> {
        if !self.versions.contains_key(&version) {
            bail!("版本 {version} 不存在");
        }
        Ok(self
            .root
            .join("exports")
            .join(format!("version-{version:04}.mid")))
    }

    pub fn version_path_for(&self, version: u32) -> Result<PathBuf> {
        if !self.versions.contains_key(&version) {
            bail!("版本 {version} 不存在");
        }
        Ok(self.version_path(version))
    }

    fn version_path(&self, version: u32) -> PathBuf {
        self.root
            .join("versions")
            .join(format!("{version:04}.alda"))
    }

    fn validate_metadata(&self) -> Result<()> {
        ensure_safe_project_name(&self.project_name)?;
        self.validate_settings()?;
        self.conversation.validate()?;
        for (version, metadata) in &self.versions {
            if let Some(plan) = &metadata.form_plan {
                plan.validate()
                    .with_context(|| format!("版本 {version} 的 form_plan 无效"))?;
            }
        }
        if let Some(plan) = self
            .working_score
            .as_ref()
            .and_then(|working| working.form_plan.as_ref())
        {
            plan.validate().context("工作乐谱的 form_plan 无效")?;
        }
        if let Some(plan) = self
            .pending_revision
            .as_ref()
            .and_then(|revision| revision.form_plan.as_ref())
        {
            plan.validate().context("待修正候选的 form_plan 无效")?;
        }
        if self.working_score.is_some() && !self.root.join(WORK_FILE).is_file() {
            bail!("项目损坏：工作乐谱元数据存在但 work.alda 不存在");
        }
        if self.pending_revision.is_some() && !self.root.join(REVISION_FILE).is_file() {
            bail!("项目损坏：待修正候选元数据存在但 revision.alda 不存在");
        }
        if let Some(revision) = &self.pending_revision {
            let source = fs::read(self.root.join(REVISION_FILE))
                .context("无法读取待修正候选 revision.alda")?;
            let source_hash = format!("{:x}", Sha256::digest(&source));
            if source_hash != revision.source_hash {
                bail!("项目损坏：revision.alda 与待修正候选元数据不一致");
            }
        }
        if self.current_version == 0 {
            if !self.versions.is_empty() {
                bail!("project.json 损坏：存在历史版本但当前版本为 0");
            }
            return Ok(());
        }
        if !self.versions.contains_key(&self.current_version) {
            bail!("project.json 损坏：当前版本不在历史中");
        }
        for version in self.versions.keys() {
            if !self.version_path(*version).is_file() {
                bail!("project.json 损坏：版本文件 {version:04}.alda 不存在");
            }
        }
        Ok(())
    }

    fn recover_projections(&self) -> Result<()> {
        self.remove_orphan_versions()?;
        self.repair_current_projection()?;
        if self.working_score.is_none() {
            self.remove_work_projection()?;
        }
        if self.pending_revision.is_none() {
            self.remove_revision_projection()?;
        }
        Ok(())
    }

    fn remove_revision_projection(&self) -> Result<()> {
        let path = self.root.join(REVISION_FILE);
        if path.exists() {
            fs::remove_file(&path).with_context(|| format!("无法删除 {}", path.display()))?;
        }
        Ok(())
    }

    fn repair_current_projection(&self) -> Result<()> {
        let current_path = self.root.join(CURRENT_FILE);
        if self.current_version == 0 {
            if current_path.exists() {
                fs::remove_file(&current_path)
                    .with_context(|| format!("无法清理 {}", current_path.display()))?;
            }
            return Ok(());
        }
        let canonical = fs::read(self.version_path(self.current_version))
            .with_context(|| format!("无法读取版本 {}", self.current_version))?;
        if fs::read(&current_path).ok().as_deref() != Some(canonical.as_slice()) {
            write_atomic(&current_path, &canonical)?;
        }
        Ok(())
    }

    fn remove_orphan_versions(&self) -> Result<()> {
        for entry in fs::read_dir(self.root.join("versions")).context("无法读取 versions 目录")?
        {
            let entry = entry.context("无法读取 versions 目录项")?;
            let path = entry.path();
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(version) = file_name
                .strip_suffix(".alda")
                .and_then(|number| number.parse::<u32>().ok())
            else {
                continue;
            };
            if file_name != format!("{version:04}.alda") {
                continue;
            }
            if !self.versions.contains_key(&version) {
                fs::remove_file(&path)
                    .with_context(|| format!("无法清理未提交版本文件 {}", path.display()))?;
            }
        }
        Ok(())
    }

    fn validate_settings(&self) -> Result<()> {
        if self.preferences.normalized() != self.preferences {
            bail!("project.json 损坏：项目偏好必须规范化、排序并去重");
        }
        validate_instruction_profile(&self.instruction_profile)?;
        self.preferences.validate()
    }

    fn write_metadata(&self) -> Result<()> {
        self.validate_settings()?;
        let json = serde_json::to_vec_pretty(self).context("无法序列化 project.json")?;
        write_atomic(&self.root.join(PROJECT_FILE), &json)
    }
}

fn validate_instruction_profile(profile: &InstructionProfile) -> Result<()> {
    if profile
        .enabled_advisory_skills
        .iter()
        .any(|id| matches!(id.origin(), SkillOrigin::Builtin))
    {
        bail!("project.json 损坏：内建 Skill 不能出现在 enabled_advisory_skills 中");
    }
    if !profile
        .enabled_advisory_skills
        .windows(2)
        .all(|pair| pair[0] < pair[1])
    {
        bail!("project.json 损坏：enabled_advisory_skills 必须去重并按限定 ID 排序");
    }
    Ok(())
}

pub fn default_project_dir(name: &str) -> Result<PathBuf> {
    ensure_safe_project_name(name)?;
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME 未设置"))?;
    Ok(PathBuf::from(home)
        .join(".alda-agent")
        .join("projects")
        .join(name))
}

pub fn list_projects() -> Result<Vec<(String, PathBuf)>> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME 未设置"))?;
    list_projects_in(&PathBuf::from(home).join(".alda-agent").join("projects"))
}

fn list_projects_in(base: &Path) -> Result<Vec<(String, PathBuf)>> {
    if !base.exists() {
        return Ok(Vec::new());
    }
    let mut projects = Vec::new();
    for entry in fs::read_dir(base).context("无法读取项目目录")? {
        let entry = entry.context("无法读取项目目录项")?;
        let path = entry.path();
        if path.is_dir() && path.join(PROJECT_FILE).is_file() {
            projects.push((entry.file_name().to_string_lossy().into_owned(), path));
        }
    }
    projects.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(projects)
}

fn ensure_safe_project_name(name: &str) -> Result<()> {
    let mut components = Path::new(name).components();
    let valid = matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none()
        && name != "."
        && name != "..";
    if !valid || name.trim().is_empty() {
        bail!("项目名称必须是单个安全目录名");
    }
    Ok(())
}

fn timestamp() -> String {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or_else(
        |_| "0".to_string(),
        |duration| duration.as_secs().to_string(),
    )
}

fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("无效文件路径：{}", path.display()))?
        .to_string_lossy();
    let temporary = path.with_file_name(format!(".{file_name}.tmp-{}", std::process::id()));
    fs::write(&temporary, contents)
        .with_context(|| format!("无法写入临时文件 {}", temporary.display()))?;
    fs::rename(&temporary, path).with_context(|| format!("无法更新 {}", path.display()))?;
    Ok(())
}

fn copy_atomic(source: &Path, target: &Path) -> Result<()> {
    let file_name = target
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("无效文件路径：{}", target.display()))?
        .to_string_lossy();
    let temporary = target.with_file_name(format!(".{file_name}.tmp-{}", std::process::id()));
    fs::copy(source, &temporary).with_context(|| {
        format!(
            "无法复制 {} 到临时文件 {}",
            source.display(),
            temporary.display()
        )
    })?;
    fs::rename(&temporary, target).with_context(|| format!("无法更新 {}", target.display()))?;
    Ok(())
}

fn restore_projections(targets: &[&PathBuf; 3], backups: &[PathBuf; 3], existed: [bool; 3]) {
    for ((target, backup), existed) in targets.iter().zip(backups).zip(existed) {
        let _ = fs::remove_file(target);
        if existed {
            let _ = fs::rename(backup, target);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn passing_checks() -> Vec<AldaCheck> {
        vec![AldaCheck {
            name: "Alda 语法",
            status: CheckStatus::Pass,
            detail: "解析成功".to_string(),
        }]
    }

    #[test]
    fn versions_never_overwrite_after_restore() {
        let directory = tempfile::tempdir().unwrap();
        let mut project =
            Project::load_or_create(directory.path().to_path_buf(), "test", "material").unwrap();
        assert_eq!(
            project
                .save_version("piano: c", "first", &passing_checks())
                .unwrap(),
            1
        );
        assert_eq!(
            project
                .save_version("piano: d", "second", &passing_checks())
                .unwrap(),
            2
        );
        project.restore_version(1).unwrap();
        assert_eq!(
            project
                .save_version("piano: e", "third", &passing_checks())
                .unwrap(),
            3
        );
        assert_eq!(project.version_code(2).unwrap(), "piano: d");
        assert_eq!(project.current_code().unwrap(), "piano: e");
    }

    #[test]
    fn failed_checks_do_not_change_project() {
        let directory = tempfile::tempdir().unwrap();
        let mut project =
            Project::load_or_create(directory.path().to_path_buf(), "test", "material").unwrap();
        project
            .save_version("piano: c", "first", &passing_checks())
            .unwrap();
        let failed = [AldaCheck {
            name: "作品内容",
            status: CheckStatus::Fail,
            detail: "空作品".to_string(),
        }];
        assert!(project.save_version("", "bad", &failed).is_err());
        assert_eq!(project.current_version, 1);
        assert_eq!(project.current_code().unwrap(), "piano: c");
        assert!(!directory.path().join("versions/0002.alda").exists());
    }

    #[test]
    fn identical_code_does_not_create_a_version() {
        let directory = tempfile::tempdir().unwrap();
        let mut project =
            Project::load_or_create(directory.path().to_path_buf(), "test", "material").unwrap();
        project
            .save_version("piano: c", "first", &passing_checks())
            .unwrap();

        let error = project
            .save_version("piano: c", "duplicate", &passing_checks())
            .unwrap_err();

        assert!(error.to_string().contains("与当前版本相同"));
        assert_eq!(project.current_version(), 1);
        assert_eq!(project.versions().len(), 1);
        assert!(!directory.path().join("versions/0002.alda").exists());
    }

    #[test]
    fn current_projection_is_not_the_canonical_version() {
        let directory = tempfile::tempdir().unwrap();
        let mut project =
            Project::load_or_create(directory.path().to_path_buf(), "test", "material").unwrap();
        project
            .save_version("piano: c", "first", &passing_checks())
            .unwrap();
        fs::write(directory.path().join(CURRENT_FILE), "piano: d").unwrap();

        assert_eq!(project.current_code().unwrap(), "piano: c");
        assert_eq!(
            project
                .save_version("piano: d", "manual edit", &passing_checks())
                .unwrap(),
            2
        );
        assert_eq!(project.version_code(2).unwrap(), "piano: d");
    }

    #[test]
    fn reload_repairs_missing_and_stale_current_projection() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let mut project = Project::load_or_create(root.clone(), "test", "").unwrap();
        project
            .save_version("piano: c", "first", &passing_checks())
            .unwrap();

        fs::remove_file(root.join(CURRENT_FILE)).unwrap();
        let project = Project::load_or_create(root.clone(), "ignored", "").unwrap();
        assert_eq!(
            fs::read_to_string(root.join(CURRENT_FILE)).unwrap(),
            "piano: c"
        );
        drop(project);

        fs::write(root.join(CURRENT_FILE), "piano: stale").unwrap();
        let project = Project::load_or_create(root.clone(), "ignored", "").unwrap();
        assert_eq!(project.current_code().unwrap(), "piano: c");
        assert_eq!(
            fs::read_to_string(root.join(CURRENT_FILE)).unwrap(),
            "piano: c"
        );
    }

    #[test]
    fn reload_discards_an_uncommitted_version_and_repairs_current() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let mut project = Project::load_or_create(root.clone(), "test", "").unwrap();
        project
            .save_version("piano: c", "first", &passing_checks())
            .unwrap();
        drop(project);

        fs::write(root.join("versions/0002.alda"), "piano: d").unwrap();
        fs::write(root.join("versions/10000.alda"), "uncommitted").unwrap();
        for name in ["1.alda", "01.alda", "00001.alda"] {
            fs::write(root.join("versions").join(name), "unrelated").unwrap();
        }
        fs::write(root.join(CURRENT_FILE), "piano: d").unwrap();

        let project = Project::load_or_create(root.clone(), "ignored", "").unwrap();
        assert_eq!(project.current_version(), 1);
        assert_eq!(project.current_code().unwrap(), "piano: c");
        assert!(!root.join("versions/0002.alda").exists());
        assert!(!root.join("versions/10000.alda").exists());
        for name in ["1.alda", "01.alda", "00001.alda"] {
            assert!(root.join("versions").join(name).exists());
        }
        assert_eq!(
            fs::read_to_string(root.join(CURRENT_FILE)).unwrap(),
            "piano: c"
        );
    }

    #[test]
    fn reload_cleans_work_projection_after_committed_accept() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let mut project = Project::load_or_create(root.clone(), "test", "").unwrap();
        project
            .save_working_score(
                "piano: c",
                WorkingScoreKind::Candidate,
                "candidate",
                &passing_checks(),
            )
            .unwrap();
        project.accept_working_score().unwrap();
        fs::write(root.join(WORK_FILE), "piano: stale").unwrap();
        drop(project);

        let project = Project::load_or_create(root.clone(), "ignored", "").unwrap();
        assert_eq!(project.current_version(), 1);
        assert_eq!(project.current_code().unwrap(), "piano: c");
        assert!(project.working_score().is_none());
        assert!(!root.join(WORK_FILE).exists());
    }

    #[test]
    fn typed_check_status_round_trips_with_the_existing_json_shape() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let mut project = Project::load_or_create(root.clone(), "test", "").unwrap();
        project
            .save_version("piano: c", "first", &passing_checks())
            .unwrap();

        let metadata = fs::read_to_string(root.join(PROJECT_FILE)).unwrap();
        assert!(metadata.contains(r#""status": "通过""#));
        let reloaded = Project::load_or_create(root, "ignored", "").unwrap();
        assert_eq!(
            reloaded.versions().get(&1).unwrap().checks[0].status,
            CheckStatus::Pass
        );
    }

    #[test]
    fn agent_mode_defaults_to_single_and_persists() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let mut project = Project::load_or_create(root.clone(), "test", "").unwrap();
        assert_eq!(project.agent_mode(), AgentMode::Single);

        project
            .configure_agent_mode(AgentMode::CompositionAb)
            .unwrap();
        drop(project);

        let project = Project::load_or_create(root, "ignored", "").unwrap();
        assert_eq!(project.agent_mode(), AgentMode::CompositionAb);
    }

    #[test]
    fn enabled_advisory_skills_are_sorted_deduplicated_and_persisted() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let mut project = Project::load_or_create(root.clone(), "test", "material").unwrap();
        assert!(
            project
                .enable_advisory_skill("user:zeta".parse().unwrap())
                .unwrap()
        );
        assert!(
            project
                .enable_advisory_skill("project:alpha".parse().unwrap())
                .unwrap()
        );
        assert!(
            !project
                .enable_advisory_skill("user:zeta".parse().unwrap())
                .unwrap()
        );
        let reloaded = Project::load_or_create(root, "ignored", "ignored").unwrap();
        assert_eq!(
            reloaded.instruction_profile().enabled_advisory_skills,
            [
                "project:alpha".parse().unwrap(),
                "user:zeta".parse().unwrap()
            ]
        );
        let metadata = fs::read_to_string(reloaded.root().join(PROJECT_FILE)).unwrap();
        assert!(!metadata.contains("Skill 正文"));
    }

    #[test]
    fn only_a_candidate_can_be_accepted_and_work_survives_restart() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let mut project = Project::load_or_create(root.clone(), "test", "").unwrap();
        project
            .save_working_score(
                "piano: c",
                WorkingScoreKind::Draft,
                "核心材料",
                &passing_checks(),
            )
            .unwrap();
        assert!(project.accept_working_score().is_err());
        assert_eq!(project.current_version(), 0);

        drop(project);
        let mut project = Project::load_or_create(root, "ignored", "").unwrap();
        assert_eq!(project.working_code().unwrap(), "piano: c");
        project
            .save_working_score(
                "piano: d",
                WorkingScoreKind::Candidate,
                "完整候选",
                &passing_checks(),
            )
            .unwrap();
        assert_eq!(project.current_version(), 0);
        assert_eq!(project.accept_working_score().unwrap(), 1);
        assert_eq!(project.current_code().unwrap(), "piano: d");
        assert!(project.working_score().is_none());
        assert!(!project.root().join(WORK_FILE).exists());
    }

    #[test]
    fn pending_revision_keeps_only_the_latest_source_across_restart() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let mut project = Project::load_or_create(root.clone(), "test", "").unwrap();
        let failed = [AldaCheck {
            name: "Alda 语法",
            status: CheckStatus::Fail,
            detail: "第一次失败".to_string(),
        }];

        project
            .save_pending_revision("piano: c+", WorkingScoreKind::Candidate, "第一次", &failed)
            .unwrap();
        project
            .save_pending_revision("piano: d+", WorkingScoreKind::Candidate, "第二次", &failed)
            .unwrap();
        drop(project);

        let reloaded = Project::load_or_create(root, "ignored", "").unwrap();
        assert_eq!(reloaded.revision_code().unwrap(), "piano: d+");
        let revision = reloaded.pending_revision().unwrap();
        assert_eq!(revision.summary, "第二次");
        assert_eq!(revision.kind, WorkingScoreKind::Candidate);
    }

    #[test]
    fn successful_working_score_clears_pending_revision() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let mut project = Project::load_or_create(root.clone(), "test", "").unwrap();
        project
            .save_pending_revision(
                "piano: c+",
                WorkingScoreKind::Candidate,
                "失败候选",
                &[AldaCheck {
                    name: "Alda 语法",
                    status: CheckStatus::Fail,
                    detail: "解析失败".to_string(),
                }],
            )
            .unwrap();

        project
            .save_working_score(
                "piano: c",
                WorkingScoreKind::Candidate,
                "成功候选",
                &passing_checks(),
            )
            .unwrap();

        assert!(project.pending_revision().is_none());
        assert!(!root.join(REVISION_FILE).exists());
        let reloaded = Project::load_or_create(root, "ignored", "").unwrap();
        assert!(reloaded.pending_revision().is_none());
        assert_eq!(reloaded.working_code().unwrap(), "piano: c");
    }

    #[test]
    fn reload_cleans_orphan_revision_projection() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let project = Project::load_or_create(root.clone(), "test", "").unwrap();
        drop(project);
        fs::write(root.join(REVISION_FILE), "piano: orphan").unwrap();

        let project = Project::load_or_create(root.clone(), "ignored", "").unwrap();

        assert!(project.pending_revision().is_none());
        assert!(!root.join(REVISION_FILE).exists());
    }

    #[test]
    fn revision_source_hash_mismatch_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let mut project = Project::load_or_create(root.clone(), "test", "").unwrap();
        project
            .save_pending_revision(
                "piano: c+",
                WorkingScoreKind::Candidate,
                "失败候选",
                &[AldaCheck {
                    name: "Alda 语法",
                    status: CheckStatus::Fail,
                    detail: "解析失败".to_string(),
                }],
            )
            .unwrap();
        drop(project);
        fs::write(root.join(REVISION_FILE), "piano: tampered").unwrap();

        let error = Project::load_or_create(root, "ignored", "").unwrap_err();
        assert!(error.to_string().contains("元数据不一致"));
    }

    #[test]
    fn corrupted_metadata_and_unsafe_names_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        assert!(Project::load_or_create(directory.path().join("x"), "../x", "").is_err());
        fs::write(
            directory.path().join(PROJECT_FILE),
            r#"{"project_name":"test","instruction_profile":{"enabled_advisory_skills":[]},"mode":"full","target_duration_secs":null,"included_instruments":[],"excluded_instruments":[],"current_version":1,"versions":{},"conversation":{"messages":[],"state":"ready"}}"#,
        )
        .unwrap();
        assert!(Project::load_or_create(directory.path().to_path_buf(), "test", "").is_err());
    }

    #[test]
    fn lists_only_project_directories() {
        let directory = tempfile::tempdir().unwrap();
        Project::load_or_create(directory.path().join("beta"), "beta", "").unwrap();
        Project::load_or_create(directory.path().join("alpha"), "alpha", "").unwrap();
        fs::create_dir(directory.path().join("not-a-project")).unwrap();
        let projects = list_projects_in(directory.path()).unwrap();
        assert_eq!(
            projects
                .iter()
                .map(|item| item.0.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "beta"]
        );
    }

    #[test]
    fn invalid_settings_are_not_persisted() {
        let directory = tempfile::tempdir().unwrap();
        let mut project =
            Project::load_or_create(directory.path().to_path_buf(), "test", "material").unwrap();
        let error = project
            .configure(&ProjectPreferences {
                target_duration_secs: Some(DurationConstraint::exact(0.0)),
                ..ProjectPreferences::default()
            })
            .unwrap_err();
        assert!(error.to_string().contains("目标时长"));

        let loaded =
            Project::load_or_create(directory.path().to_path_buf(), "test", "ignored").unwrap();
        assert_eq!(loaded.mode(), CreationMode::Full);
    }

    #[test]
    fn instrument_preferences_are_canonical_before_persistence() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let mut project = Project::load_or_create(root.clone(), "test", "").unwrap();
        project
            .configure(&ProjectPreferences {
                included_instruments: vec![" MIDI-Piano ".to_string(), "midi-piano".to_string()],
                excluded_instruments: vec![" MIDI-TUBA ".to_string()],
                ..ProjectPreferences::default()
            })
            .unwrap();
        assert_eq!(project.included_instruments(), ["midi-piano"]);
        assert_eq!(project.excluded_instruments(), ["midi-tuba"]);

        let error = project
            .configure(&ProjectPreferences {
                included_instruments: vec![" midi-cello ".to_string()],
                excluded_instruments: vec!["MIDI-CELLO".to_string()],
                ..ProjectPreferences::default()
            })
            .unwrap_err();
        assert!(error.to_string().contains("同时"));
        let reloaded = Project::load_or_create(root, "ignored", "").unwrap();
        assert_eq!(reloaded.included_instruments(), ["midi-piano"]);
    }

    #[test]
    fn retrying_same_pending_request_does_not_duplicate_user_message() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let mut project = Project::load_or_create(root.clone(), "test", "").unwrap();

        project.prepare_user_message("同一个请求").unwrap();
        project.prepare_user_message("同一个请求").unwrap();

        assert_eq!(project.conversation().messages().len(), 1);
        assert_eq!(
            project.conversation().state(),
            ConversationState::RequestPending
        );
        drop(project);

        let mut restarted = Project::load_or_create(root, "ignored", "").unwrap();
        restarted.prepare_user_message("同一个请求").unwrap();
        assert_eq!(restarted.conversation().messages().len(), 1);
    }

    #[test]
    fn pending_candidate_requirement_survives_restart_until_the_turn_finishes() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let mut project = Project::load_or_create(root.clone(), "test", "").unwrap();

        project
            .prepare_user_message_with_requirement("写一首圣咏", true)
            .unwrap();
        assert!(project.conversation().pending_candidate());
        drop(project);

        let mut restarted = Project::load_or_create(root, "ignored", "").unwrap();
        assert!(restarted.conversation().pending_candidate());
        restarted
            .finish_agent_turn("完成".to_string(), ConversationState::Ready)
            .unwrap();
        assert!(!restarted.conversation().pending_candidate());
    }
}
