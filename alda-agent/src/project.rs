use crate::alda::{AldaCheck, CheckStatus};
use crate::conversation::{Conversation, ConversationMessage, ConversationState};
use crate::instructions::InstructionProfile;
use crate::skills::{QualifiedSkillId, SkillOrigin};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const PROJECT_FILE: &str = "project.json";
const CURRENT_FILE: &str = "current.alda";
const WORK_FILE: &str = "work.alda";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckRecord {
    pub name: String,
    pub status: String,
    pub detail: String,
}

impl From<&AldaCheck> for CheckRecord {
    fn from(check: &AldaCheck) -> Self {
        Self {
            name: check.name.to_string(),
            status: check.status.to_string(),
            detail: check.detail.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VersionMeta {
    pub created_at: String,
    pub summary: String,
    pub checks: Vec<CheckRecord>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkingScoreKind {
    Draft,
    Candidate,
}

impl std::fmt::Display for WorkingScoreKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Draft => write!(f, "草稿"),
            Self::Candidate => write!(f, "完整候选"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkingScore {
    pub kind: WorkingScoreKind,
    pub summary: String,
    pub checks: Vec<CheckRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub project_name: String,
    #[serde(default)]
    instruction_profile: InstructionProfile,
    mode: String,
    target_duration_secs: Option<f64>,
    included_instruments: Vec<String>,
    excluded_instruments: Vec<String>,
    current_version: u32,
    versions: BTreeMap<u32, VersionMeta>,
    working_score: Option<WorkingScore>,
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
            project.validate_metadata()?;
            return Ok(project);
        }

        fs::create_dir_all(root.join("versions")).context("无法创建 versions 目录")?;
        fs::create_dir_all(root.join("exports")).context("无法创建 exports 目录")?;
        let project = Self {
            project_name: project_name.to_string(),
            instruction_profile: InstructionProfile::default(),
            mode: "full".to_string(),
            target_duration_secs: None,
            included_instruments: Vec::new(),
            excluded_instruments: Vec::new(),
            current_version: 0,
            versions: BTreeMap::new(),
            working_score: None,
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
    pub fn mode(&self) -> &str {
        &self.mode
    }

    #[must_use]
    pub fn target_duration_secs(&self) -> Option<f64> {
        self.target_duration_secs
    }

    #[must_use]
    pub fn included_instruments(&self) -> &[String] {
        &self.included_instruments
    }

    #[must_use]
    pub fn excluded_instruments(&self) -> &[String] {
        &self.excluded_instruments
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
        if content.trim().is_empty() {
            bail!("对话消息不能为空");
        }
        let is_same_pending_request = self.conversation.state()
            == ConversationState::RequestPending
            && self.conversation.last_user_message() == Some(content);
        if !is_same_pending_request {
            self.conversation.add_user_message(content.to_string());
        }
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

    pub fn configure(
        &mut self,
        mode: &str,
        target_duration_secs: Option<f64>,
        included_instruments: Vec<String>,
        excluded_instruments: Vec<String>,
    ) -> Result<()> {
        let normalized = crate::instructions::ProjectPreferences {
            mode: mode.to_string(),
            target_duration_secs,
            included_instruments,
            excluded_instruments,
        }
        .normalized();
        validate_project_settings(
            &normalized.mode,
            normalized.target_duration_secs,
            &normalized.included_instruments,
            &normalized.excluded_instruments,
        )?;
        let previous_target = self.target_duration_secs;
        self.target_duration_secs = normalized.target_duration_secs;
        let previous = (
            std::mem::replace(&mut self.mode, normalized.mode),
            previous_target,
            std::mem::replace(
                &mut self.included_instruments,
                normalized.included_instruments,
            ),
            std::mem::replace(
                &mut self.excluded_instruments,
                normalized.excluded_instruments,
            ),
        );
        if let Err(error) = self.write_metadata() {
            self.mode = previous.0;
            self.target_duration_secs = previous.1;
            self.included_instruments = previous.2;
            self.excluded_instruments = previous.3;
            return Err(error);
        }
        Ok(())
    }

    pub fn current_code(&self) -> Result<String> {
        if self.current_version == 0 {
            bail!("项目还没有有效版本");
        }
        fs::read_to_string(self.root.join(CURRENT_FILE)).context("无法读取 current.alda")
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

    pub fn save_working_score(
        &mut self,
        alda_code: &str,
        kind: WorkingScoreKind,
        summary: &str,
        checks: &[AldaCheck],
    ) -> Result<()> {
        self.validate_settings()?;
        if alda_code.trim().is_empty() {
            bail!("不能保存空工作乐谱");
        }
        if checks.iter().any(|check| check.status == CheckStatus::Fail) {
            bail!("检查未全部通过，不能保存工作乐谱");
        }
        write_atomic(&self.root.join(WORK_FILE), alda_code.as_bytes())?;
        self.working_score = Some(WorkingScore {
            kind,
            summary: summary.to_string(),
            checks: checks.iter().map(CheckRecord::from).collect(),
        });
        self.write_metadata()
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
        if working.checks.iter().any(|check| check.status == "失败") {
            bail!("完整候选检查未全部通过，不能接受为有效版本");
        }
        let code = self.working_code()?;
        let version = self.save_version_records(&code, &working.summary, &working.checks)?;
        self.clear_working_score()?;
        Ok(version)
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
        self.save_version_records(alda_code, summary, &records)
    }

    fn save_version_records(
        &mut self,
        alda_code: &str,
        summary: &str,
        checks: &[CheckRecord],
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

        write_atomic(&self.root.join(CURRENT_FILE), alda_code.as_bytes())?;
        self.current_version = next;
        self.versions.insert(
            next,
            VersionMeta {
                created_at: timestamp(),
                summary: summary.to_string(),
                checks: checks.to_vec(),
            },
        );
        self.write_metadata()?;
        Ok(next)
    }

    fn clear_working_score(&mut self) -> Result<()> {
        self.working_score = None;
        self.write_metadata()?;
        let path = self.root.join(WORK_FILE);
        if path.exists() {
            fs::remove_file(&path).with_context(|| format!("无法删除 {}", path.display()))?;
        }
        Ok(())
    }

    pub fn restore_version(&mut self, version: u32) -> Result<()> {
        self.validate_settings()?;
        let code = self.version_code(version)?;
        write_atomic(&self.root.join(CURRENT_FILE), code.as_bytes())?;
        self.current_version = version;
        self.write_metadata()
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
        if self.working_score.is_some() && !self.root.join(WORK_FILE).is_file() {
            bail!("项目损坏：工作乐谱元数据存在但 work.alda 不存在");
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
        if !self.root.join(CURRENT_FILE).is_file() {
            bail!("项目损坏：current.alda 不存在");
        }
        Ok(())
    }

    fn validate_settings(&self) -> Result<()> {
        let preferences = crate::instructions::ProjectPreferences {
            mode: self.mode.clone(),
            target_duration_secs: self.target_duration_secs,
            included_instruments: self.included_instruments.clone(),
            excluded_instruments: self.excluded_instruments.clone(),
        };
        if preferences.normalized() != preferences {
            bail!("project.json 损坏：项目偏好必须规范化、排序并去重");
        }
        validate_instruction_profile(&self.instruction_profile)?;
        validate_project_settings(
            &self.mode,
            self.target_duration_secs,
            &self.included_instruments,
            &self.excluded_instruments,
        )
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

fn validate_project_settings(
    mode: &str,
    target_duration_secs: Option<f64>,
    included_instruments: &[String],
    excluded_instruments: &[String],
) -> Result<()> {
    if !matches!(mode, "full" | "improv") {
        bail!("project.json 损坏：mode 必须是 full 或 improv");
    }
    if target_duration_secs.is_some_and(|duration| !duration.is_finite() || duration <= 0.0) {
        bail!("project.json 损坏：目标时长必须大于 0");
    }
    if included_instruments
        .iter()
        .chain(excluded_instruments)
        .any(|instrument| instrument.trim().is_empty())
    {
        bail!("project.json 损坏：乐器约束不能为空");
    }
    if let Some(conflict) = included_instruments.iter().find(|included| {
        excluded_instruments
            .iter()
            .any(|excluded| included.eq_ignore_ascii_case(excluded))
    }) {
        bail!("project.json 损坏：乐器 {conflict} 同时被包含和排除");
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
    fn edited_working_file_can_be_adopted_as_a_new_version() {
        let directory = tempfile::tempdir().unwrap();
        let mut project =
            Project::load_or_create(directory.path().to_path_buf(), "test", "material").unwrap();
        project
            .save_version("piano: c", "first", &passing_checks())
            .unwrap();
        fs::write(directory.path().join(CURRENT_FILE), "piano: d").unwrap();

        assert_eq!(
            project
                .save_version("piano: d", "manual edit", &passing_checks())
                .unwrap(),
            2
        );
        assert_eq!(project.version_code(2).unwrap(), "piano: d");
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
            .configure("invalid", None, Vec::new(), Vec::new())
            .unwrap_err();
        assert!(error.to_string().contains("mode"));

        let loaded =
            Project::load_or_create(directory.path().to_path_buf(), "test", "ignored").unwrap();
        assert_eq!(loaded.mode(), "full");
    }

    #[test]
    fn instrument_preferences_are_canonical_before_persistence() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let mut project = Project::load_or_create(root.clone(), "test", "").unwrap();
        project
            .configure(
                "full",
                None,
                vec![" MIDI-Piano ".to_string(), "midi-piano".to_string()],
                vec![" MIDI-TUBA ".to_string()],
            )
            .unwrap();
        assert_eq!(project.included_instruments(), ["midi-piano"]);
        assert_eq!(project.excluded_instruments(), ["midi-tuba"]);

        let error = project
            .configure(
                "full",
                None,
                vec![" midi-cello ".to_string()],
                vec!["MIDI-CELLO".to_string()],
            )
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
}
