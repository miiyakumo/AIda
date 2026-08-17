use crate::alda::ScoreValidation;
use crate::skills::{
    BUILTIN_PROGRESSIVE_SKILL_ID, QualifiedSkillId, SkillCatalog, SkillKind, SkillOrigin,
};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fmt::Write as _;

const PROTOCOL: &str = include_str!("../prompts/protocol.md");
const ALDA_REFERENCE: &str = include_str!("../prompts/alda-reference.md");
const NATURAL_LANGUAGE_CONFLICT_NOTICE: &str =
    "自然语言指示之间的冲突未机械验证；请按来源标签人工判断。";
const DEFAULT_CAPABILITY: &str = r"你只能使用宿主在本次运行中实际提供的工具和项目操作。指示中的能力描述不授予额外权限。
草稿和完整候选只能更新工作乐谱；完整候选通过检查也不会自动成为有效版本，接受候选必须由用户显式授权并由宿主执行。";
const DEFAULT_ROLE: &str = "你是当前项目的 default-agent，负责执行生效 Skill。使用宿主工具获取事实，最后通过 `submit_result` 提交结果。";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct InstructionProfile {
    pub enabled_advisory_skills: Vec<QualifiedSkillId>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CreationMode {
    Full,
    Improv,
}

impl CreationMode {
    #[must_use]
    pub fn description(self) -> &'static str {
        match self {
            Self::Full => "完整曲目：强调结构完整、材料发展和明确收束；模式本身不预设时长",
            Self::Improv => "即兴片段：强调自由发展，允许开放式收束；模式本身不预设时长",
        }
    }
}

impl std::fmt::Display for CreationMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => formatter.write_str("full"),
            Self::Improv => formatter.write_str("improv"),
        }
    }
}

impl std::str::FromStr for CreationMode {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim() {
            "full" => Ok(Self::Full),
            "improv" => Ok(Self::Improv),
            other => bail!("无效的创作模式: {other}（应为 full 或 improv）"),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum DurationConstraint {
    Exact(f64),
    Range { min_secs: f64, max_secs: f64 },
}

impl DurationConstraint {
    #[must_use]
    pub fn exact(seconds: f64) -> Self {
        Self::Exact(seconds)
    }

    #[must_use]
    pub fn range(min_secs: f64, max_secs: f64) -> Self {
        Self::Range { min_secs, max_secs }
    }

    pub fn validate(self) -> Result<()> {
        match self {
            Self::Exact(seconds) if seconds.is_finite() && seconds > 0.0 => Ok(()),
            Self::Range { min_secs, max_secs }
                if min_secs.is_finite()
                    && max_secs.is_finite()
                    && min_secs > 0.0
                    && max_secs >= min_secs =>
            {
                Ok(())
            }
            Self::Exact(_) => bail!("项目目标时长必须是大于零的有限秒数"),
            Self::Range { .. } => bail!("项目目标时长区间必须是有效的正数，且上限不小于下限"),
        }
    }

    #[must_use]
    pub fn validation_bounds(self, tolerance_pct: f64) -> (f64, f64) {
        match self {
            Self::Exact(seconds) => {
                let tolerance = seconds * tolerance_pct / 100.0;
                (seconds - tolerance, seconds + tolerance)
            }
            Self::Range { min_secs, max_secs } => (min_secs, max_secs),
        }
    }
}

impl std::fmt::Display for DurationConstraint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exact(seconds) => write!(formatter, "{seconds} 秒"),
            Self::Range { min_secs, max_secs } => {
                write!(formatter, "{min_secs}–{max_secs} 秒")
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectPreferences {
    pub mode: CreationMode,
    pub target_duration_secs: Option<DurationConstraint>,
    pub included_instruments: Vec<String>,
    pub excluded_instruments: Vec<String>,
}

impl Default for ProjectPreferences {
    fn default() -> Self {
        Self {
            mode: CreationMode::Full,
            target_duration_secs: None,
            included_instruments: Vec::new(),
            excluded_instruments: Vec::new(),
        }
    }
}

impl ProjectPreferences {
    #[must_use]
    pub fn normalized(&self) -> Self {
        Self {
            mode: self.mode,
            target_duration_secs: self.target_duration_secs,
            included_instruments: normalize_instruments(&self.included_instruments),
            excluded_instruments: normalize_instruments(&self.excluded_instruments),
        }
    }

    pub fn validate(&self) -> Result<()> {
        if let Some(duration) = self.target_duration_secs {
            duration.validate()?;
        }
        if self
            .included_instruments
            .iter()
            .chain(&self.excluded_instruments)
            .any(|instrument| instrument.trim().is_empty())
        {
            bail!("项目乐器偏好不能为空字符串");
        }
        if let Some(conflict) = self.included_instruments.iter().find(|included| {
            self.excluded_instruments
                .iter()
                .any(|excluded| included.trim().eq_ignore_ascii_case(excluded.trim()))
        }) {
            bail!("乐器 {conflict:?} 不能同时出现在必须包含和必须排除列表中");
        }
        Ok(())
    }

    #[must_use]
    pub fn score_validation(&self, check_duration: bool) -> ScoreValidation {
        ScoreValidation::new(
            check_duration
                .then_some(self.target_duration_secs)
                .flatten(),
            self.included_instruments.clone(),
            self.excluded_instruments.clone(),
        )
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InstructionFragmentKind {
    Protocol,
    Skill,
    Preference,
    Capability,
    Role,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InstructionScope {
    Global,
    Project,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InstructionStrength {
    Invariant,
    Constraint,
    Preference,
    Guidance,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstructionFragment {
    pub id: String,
    pub qualified_skill_id: Option<QualifiedSkillId>,
    pub kind: InstructionFragmentKind,
    pub origin: SkillOrigin,
    pub scope: InstructionScope,
    pub strength: InstructionStrength,
    pub label: String,
    pub content: String,
    pub digest: String,
}

#[derive(Debug, Clone, Copy)]
struct FragmentClass {
    kind: InstructionFragmentKind,
    origin: SkillOrigin,
    scope: InstructionScope,
    strength: InstructionStrength,
}

impl InstructionFragment {
    fn new(
        id: impl Into<String>,
        qualified_skill_id: Option<QualifiedSkillId>,
        class: FragmentClass,
        label: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        let content = content.into().trim().to_string();
        let digest = sha256_hex(content.as_bytes());
        Self {
            id: id.into(),
            qualified_skill_id,
            kind: class.kind,
            origin: class.origin,
            scope: class.scope,
            strength: class.strength,
            label: label.into(),
            content,
            digest,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CompiledInstructions {
    fragments: Vec<InstructionFragment>,
    resolved_preferences: ProjectPreferences,
    conflicts: Vec<String>,
    rendered: String,
    summary: String,
    fingerprint: String,
}

impl CompiledInstructions {
    pub fn compile(
        catalog: &SkillCatalog,
        profile: &InstructionProfile,
        preferences: &ProjectPreferences,
    ) -> Result<Self> {
        compile_instructions(catalog, profile, preferences)
    }

    #[must_use]
    pub fn fragments(&self) -> &[InstructionFragment] {
        &self.fragments
    }

    #[must_use]
    pub fn resolved_preferences(&self) -> &ProjectPreferences {
        &self.resolved_preferences
    }

    #[must_use]
    pub fn conflicts(&self) -> &[String] {
        &self.conflicts
    }

    #[must_use]
    pub fn rendered(&self) -> &str {
        &self.rendered
    }

    #[must_use]
    pub fn summary(&self) -> &str {
        &self.summary
    }

    #[must_use]
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

pub fn compile_instructions(
    catalog: &SkillCatalog,
    profile: &InstructionProfile,
    preferences: &ProjectPreferences,
) -> Result<CompiledInstructions> {
    let preferences = preferences.normalized();
    preferences.validate()?;

    let mut fragments = Vec::new();
    let core_protocol = format!("{PROTOCOL}\n\n{ALDA_REFERENCE}");
    fragments.push(InstructionFragment::new(
        "core-protocol",
        None,
        FragmentClass {
            kind: InstructionFragmentKind::Protocol,
            origin: SkillOrigin::Builtin,
            scope: InstructionScope::Global,
            strength: InstructionStrength::Invariant,
        },
        "【核心协议｜来源：builtin:protocol】",
        core_protocol,
    ));
    fragments.push(application_capability_fragment());

    let mut active_skills = catalog
        .load_active(&profile.enabled_advisory_skills)?
        .into_iter();
    let workflow = active_skills
        .next()
        .ok_or_else(|| anyhow::anyhow!("Skill catalog 未返回内建 workflow"))?;
    let expected_workflow_id: QualifiedSkillId = BUILTIN_PROGRESSIVE_SKILL_ID.parse()?;
    if workflow.descriptor.id != expected_workflow_id
        || workflow.descriptor.kind != SkillKind::Workflow
    {
        bail!("指示编译器需要内建 workflow Skill {BUILTIN_PROGRESSIVE_SKILL_ID}");
    }
    fragments.push(InstructionFragment::new(
        "workflow:progressive-composition",
        Some(workflow.descriptor.id.clone()),
        FragmentClass {
            kind: InstructionFragmentKind::Skill,
            origin: SkillOrigin::Builtin,
            scope: InstructionScope::Global,
            strength: InstructionStrength::Guidance,
        },
        format!(
            "【工作流 Skill｜来源：{}｜{}】",
            workflow.descriptor.id, workflow.descriptor.description
        ),
        workflow.body,
    ));

    for skill in active_skills {
        let id = skill.descriptor.id.clone();
        let scope = match id.origin() {
            SkillOrigin::Project => InstructionScope::Project,
            SkillOrigin::Builtin | SkillOrigin::User => InstructionScope::Global,
        };
        fragments.push(InstructionFragment::new(
            format!("advisory:{id}"),
            Some(id.clone()),
            FragmentClass {
                kind: InstructionFragmentKind::Skill,
                origin: id.origin(),
                scope,
                strength: InstructionStrength::Guidance,
            },
            format!(
                "【Advisory Skill｜来源：{}｜{}】",
                id, skill.descriptor.description
            ),
            skill.body,
        ));
    }

    fragments.push(InstructionFragment::new(
        "project-preferences",
        None,
        FragmentClass {
            kind: InstructionFragmentKind::Preference,
            origin: SkillOrigin::Project,
            scope: InstructionScope::Project,
            strength: InstructionStrength::Preference,
        },
        "【项目偏好｜来源：project】",
        render_project_preferences(&preferences),
    ));
    fragments.push(default_role_fragment());

    let rendered = render_fragments(&fragments);
    let fingerprint = sha256_hex(rendered.as_bytes());
    let conflicts = vec![NATURAL_LANGUAGE_CONFLICT_NOTICE.to_string()];
    let summary = render_summary(&fragments, &preferences, &conflicts, &fingerprint);
    Ok(CompiledInstructions {
        fragments,
        resolved_preferences: preferences,
        conflicts,
        rendered,
        summary,
        fingerprint,
    })
}

fn normalize_instruments(values: &[String]) -> Vec<String> {
    let mut values = values
        .iter()
        .map(|value| value.trim().to_ascii_lowercase())
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();
    values
}

fn application_capability_fragment() -> InstructionFragment {
    InstructionFragment::new(
        "application-capability",
        None,
        FragmentClass {
            kind: InstructionFragmentKind::Capability,
            origin: SkillOrigin::Builtin,
            scope: InstructionScope::Global,
            strength: InstructionStrength::Constraint,
        },
        "【能力边界｜来源：builtin:application-policy】",
        DEFAULT_CAPABILITY,
    )
}

fn default_role_fragment() -> InstructionFragment {
    InstructionFragment::new(
        "default-role",
        None,
        FragmentClass {
            kind: InstructionFragmentKind::Role,
            origin: SkillOrigin::Builtin,
            scope: InstructionScope::Global,
            strength: InstructionStrength::Constraint,
        },
        "【默认角色｜来源：builtin:default-agent】",
        DEFAULT_ROLE,
    )
}

fn render_project_preferences(preferences: &ProjectPreferences) -> String {
    let mut included: BTreeSet<_> = preferences
        .included_instruments
        .iter()
        .map(|instrument| instrument.trim())
        .collect();
    let mut excluded: BTreeSet<_> = preferences
        .excluded_instruments
        .iter()
        .map(|instrument| instrument.trim())
        .collect();
    let mut content = format!("- 创作模式：{}\n", preferences.mode);
    if let Some(duration) = preferences.target_duration_secs {
        let _ = writeln!(content, "- 目标时长：{duration}");
    } else {
        content.push_str("- 目标时长：未设置\n");
    }
    if included.is_empty() {
        content.push_str("- 必须包含的乐器：未设置\n");
    } else {
        let values = included
            .pop_first()
            .into_iter()
            .chain(included)
            .collect::<Vec<_>>();
        let _ = writeln!(content, "- 必须包含的乐器：{}", values.join("、"));
    }
    if excluded.is_empty() {
        content.push_str("- 必须排除的乐器：未设置");
    } else {
        let values = excluded
            .pop_first()
            .into_iter()
            .chain(excluded)
            .collect::<Vec<_>>();
        let _ = write!(content, "- 必须排除的乐器：{}", values.join("、"));
    }
    content
}

fn render_fragments(fragments: &[InstructionFragment]) -> String {
    let mut rendered = String::new();
    for (index, fragment) in fragments.iter().enumerate() {
        if index > 0 {
            rendered.push_str("\n\n");
        }
        rendered.push_str(&fragment.label);
        rendered.push('\n');
        rendered.push_str(&fragment.content);
    }
    rendered
}

fn render_summary(
    fragments: &[InstructionFragment],
    preferences: &ProjectPreferences,
    conflicts: &[String],
    fingerprint: &str,
) -> String {
    let skills = fragments
        .iter()
        .filter_map(|fragment| fragment.qualified_skill_id.as_ref())
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let fragment_digests = fragments
        .iter()
        .map(|fragment| format!("{}={}", fragment.id, fragment.digest))
        .collect::<Vec<_>>()
        .join(", ");
    let duration = preferences
        .target_duration_secs
        .map_or_else(|| "未设置".to_string(), |duration| duration.to_string());
    let included = display_list(&preferences.included_instruments);
    let excluded = display_list(&preferences.excluded_instruments);
    format!(
        "核心协议：builtin:protocol\n生效 Skill：{skills}\n项目偏好：mode={}, target_duration={}, include={}, exclude={}\n角色：builtin:default-agent\n有效模型工具：submit_result、lookup_alda_docs；项目会话另提供 inspect_score、render_score、play_score\n能力：可查询、校验、渲染、播放和更新工作乐谱；不能接受候选或写入有效版本\n结构化冲突：未发现\n{}\n片段摘要：{fragment_digests}\nFingerprint：{fingerprint}",
        preferences.mode,
        duration,
        included,
        excluded,
        conflicts.join("；")
    )
}

fn display_list(values: &[String]) -> String {
    let values = values
        .iter()
        .map(|value| value.trim())
        .collect::<BTreeSet<_>>();
    if values.is_empty() {
        "未设置".to_string()
    } else {
        values.into_iter().collect::<Vec<_>>().join("、")
    }
}

fn sha256_hex(content: &[u8]) -> String {
    let digest = Sha256::digest(content);
    digest.iter().fold(
        String::with_capacity(digest.len() * 2),
        |mut output, byte| {
            let _ = write!(output, "{byte:02x}");
            output
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::Write;
    use std::path::Path;

    fn write_advisory(root: &Path, name: &str, description: &str, body: &str) {
        let skill_root = root.join(name);
        fs::create_dir_all(&skill_root).unwrap();
        let mut file = File::create(skill_root.join("SKILL.md")).unwrap();
        write!(
            file,
            "---\nname: {name}\ndescription: {description}\nkind: advisory\n---\n{body}"
        )
        .unwrap();
    }

    #[test]
    fn compiler_uses_fixed_layer_order_and_sorted_advisory_ids() {
        let user_root = tempfile::tempdir().unwrap();
        let project_root = tempfile::tempdir().unwrap();
        write_advisory(user_root.path(), "zeta", "Z", "Z 指示");
        write_advisory(project_root.path(), "alpha", "A", "A 指示");
        let catalog =
            SkillCatalog::discover(Some(user_root.path()), Some(project_root.path())).unwrap();
        let profile = InstructionProfile {
            enabled_advisory_skills: vec![
                "user:zeta".parse().unwrap(),
                "project:alpha".parse().unwrap(),
            ],
        };
        let compiled =
            compile_instructions(&catalog, &profile, &ProjectPreferences::default()).unwrap();

        let ids = compiled
            .fragments
            .iter()
            .map(|fragment| fragment.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            [
                "core-protocol",
                "application-capability",
                "workflow:progressive-composition",
                "advisory:project:alpha",
                "advisory:user:zeta",
                "project-preferences",
                "default-role",
            ]
        );
        assert!(
            compiled
                .rendered
                .starts_with("【核心协议｜来源：builtin:protocol】")
        );
        assert!(compiled.rendered.ends_with("提交结果。"));
    }

    #[test]
    fn fingerprint_and_summary_are_stable_for_equivalent_preferences() {
        let catalog = SkillCatalog::discover(None, None).unwrap();
        let first = ProjectPreferences {
            mode: CreationMode::Full,
            target_duration_secs: Some(DurationConstraint::exact(180.0)),
            included_instruments: vec!["midi-violin".to_string(), "midi-cello".to_string()],
            excluded_instruments: vec!["midi-tuba".to_string()],
        };
        let second = ProjectPreferences {
            included_instruments: vec!["midi-cello".to_string(), "midi-violin".to_string()],
            ..first.clone()
        };
        let first = compile_instructions(&catalog, &InstructionProfile::default(), &first).unwrap();
        let second =
            compile_instructions(&catalog, &InstructionProfile::default(), &second).unwrap();
        assert_eq!(first.fingerprint, second.fingerprint);
        assert_eq!(first.rendered, second.rendered);
        assert_eq!(first.fingerprint.len(), 64);
        assert!(first.summary.contains(&first.fingerprint));
        assert!(first.summary.contains(&first.fragments[0].digest));
    }

    #[test]
    fn conflict_reporting_does_not_claim_natural_language_validation() {
        let catalog = SkillCatalog::discover(None, None).unwrap();
        let compiled = compile_instructions(
            &catalog,
            &InstructionProfile::default(),
            &ProjectPreferences::default(),
        )
        .unwrap();
        assert_eq!(compiled.conflicts, [NATURAL_LANGUAGE_CONFLICT_NOTICE]);
        assert!(compiled.summary.contains("结构化冲突：未发现"));
        assert!(
            compiled
                .summary
                .contains("自然语言指示之间的冲突未机械验证")
        );
    }

    #[test]
    fn structured_preference_conflicts_fail_compilation() {
        let catalog = SkillCatalog::discover(None, None).unwrap();
        let preferences = ProjectPreferences {
            included_instruments: vec!["midi-cello".to_string()],
            excluded_instruments: vec!["midi-cello".to_string()],
            ..ProjectPreferences::default()
        };
        let error = compile_instructions(&catalog, &InstructionProfile::default(), &preferences)
            .unwrap_err();
        assert!(error.to_string().contains("同时出现在"));
    }

    #[test]
    fn invalid_mode_is_rejected_by_serde_and_duration_before_rendering() {
        let catalog = SkillCatalog::discover(None, None).unwrap();
        assert!(serde_json::from_str::<ProjectPreferences>(
            r#"{"mode":"custom","target_duration_secs":null,"included_instruments":[],"excluded_instruments":[]}"#
        )
        .is_err());
        let invalid_duration = ProjectPreferences {
            target_duration_secs: Some(DurationConstraint::exact(f64::NAN)),
            ..ProjectPreferences::default()
        };
        assert!(
            compile_instructions(&catalog, &InstructionProfile::default(), &invalid_duration)
                .is_err()
        );
    }

    #[test]
    fn duration_constraint_keeps_numeric_compatibility_and_serializes_ranges() {
        let exact: ProjectPreferences = serde_json::from_str(
            r#"{"mode":"full","target_duration_secs":180,"included_instruments":[],"excluded_instruments":[]}"#,
        )
        .unwrap();
        assert_eq!(
            exact.target_duration_secs,
            Some(DurationConstraint::exact(180.0))
        );
        let range = ProjectPreferences {
            target_duration_secs: Some(DurationConstraint::range(120.0, 180.0)),
            ..ProjectPreferences::default()
        };
        let json = serde_json::to_value(range).unwrap();
        assert_eq!(json["target_duration_secs"]["min_secs"], 120.0);
        assert_eq!(json["target_duration_secs"]["max_secs"], 180.0);
    }

    #[test]
    fn inactive_advisory_body_is_not_loaded() {
        let root = tempfile::tempdir().unwrap();
        write_advisory(root.path(), "huge", "大正文", &"x".repeat(40 * 1024));
        let catalog = SkillCatalog::discover(Some(root.path()), None).unwrap();
        compile_instructions(
            &catalog,
            &InstructionProfile::default(),
            &ProjectPreferences::default(),
        )
        .unwrap();
        let profile = InstructionProfile {
            enabled_advisory_skills: vec!["user:huge".parse().unwrap()],
        };
        assert!(compile_instructions(&catalog, &profile, &ProjectPreferences::default()).is_err());
    }
}
