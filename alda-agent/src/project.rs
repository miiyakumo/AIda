use crate::alda::{AldaCheck, CheckStatus};
use crate::deepseek::Message;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const PROJECT_FILE: &str = "project.json";
const CURRENT_FILE: &str = "current.alda";

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub project_name: String,
    pub source_material: String,
    pub requirements: Vec<String>,
    pub interpretation: String,
    creative_strategy: String,
    mode: String,
    target_duration_secs: Option<f64>,
    included_instruments: Vec<String>,
    excluded_instruments: Vec<String>,
    current_version: u32,
    versions: BTreeMap<u32, VersionMeta>,
    pub conversation: Vec<Message>,
    #[serde(skip)]
    root: PathBuf,
}

impl Project {
    pub fn load_or_create(
        root: PathBuf,
        project_name: &str,
        source_material: &str,
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
            source_material: source_material.to_string(),
            requirements: Vec::new(),
            interpretation: String::new(),
            creative_strategy: String::new(),
            mode: "full".to_string(),
            target_duration_secs: None,
            included_instruments: Vec::new(),
            excluded_instruments: Vec::new(),
            current_version: 0,
            versions: BTreeMap::new(),
            conversation: Vec::new(),
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
    pub fn creative_strategy(&self) -> &str {
        &self.creative_strategy
    }

    pub fn set_creative_strategy(&mut self, strategy: &str) -> Result<()> {
        let strategy = strategy.trim().to_string();
        let previous = std::mem::replace(&mut self.creative_strategy, strategy);
        if let Err(error) = self.write_metadata() {
            self.creative_strategy = previous;
            return Err(error);
        }
        Ok(())
    }

    pub fn configure(
        &mut self,
        mode: &str,
        target_duration_secs: Option<f64>,
        included_instruments: Vec<String>,
        excluded_instruments: Vec<String>,
    ) -> Result<()> {
        validate_project_settings(
            mode,
            target_duration_secs,
            &included_instruments,
            &excluded_instruments,
        )?;
        let previous_target = self.target_duration_secs;
        self.target_duration_secs = target_duration_secs;
        let previous = (
            std::mem::replace(&mut self.mode, mode.to_string()),
            previous_target,
            std::mem::replace(&mut self.included_instruments, included_instruments),
            std::mem::replace(&mut self.excluded_instruments, excluded_instruments),
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
                checks: checks.iter().map(CheckRecord::from).collect(),
            },
        );
        self.write_metadata()?;
        Ok(next)
    }

    pub fn restore_version(&mut self, version: u32) -> Result<()> {
        self.validate_settings()?;
        let code = self.version_code(version)?;
        write_atomic(&self.root.join(CURRENT_FILE), code.as_bytes())?;
        self.current_version = version;
        self.write_metadata()
    }

    pub fn update_context(
        &mut self,
        interpretation: String,
        conversation: Vec<Message>,
    ) -> Result<()> {
        self.interpretation = interpretation;
        self.conversation = conversation;
        self.write_metadata()
    }

    pub fn record_requirement(&mut self, requirement: String) -> Result<()> {
        if !requirement.trim().is_empty() {
            self.requirements.push(requirement);
            self.write_metadata()?;
        }
        Ok(())
    }

    pub fn export_alda(&self) -> Result<PathBuf> {
        let code = self.version_code(self.current_version)?;
        let path = self
            .root
            .join("exports")
            .join(format!("version-{:04}.alda", self.current_version));
        write_atomic(&path, code.as_bytes())?;
        Ok(path)
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

    fn version_path(&self, version: u32) -> PathBuf {
        self.root
            .join("versions")
            .join(format!("{version:04}.alda"))
    }

    fn validate_metadata(&self) -> Result<()> {
        ensure_safe_project_name(&self.project_name)?;
        self.validate_settings()?;
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
    fn creative_strategy_is_trimmed_and_persisted() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let mut project = Project::load_or_create(root.clone(), "test", "material").unwrap();
        project
            .set_creative_strategy("  明亮欢快，避免机械重复  ")
            .unwrap();
        let reloaded = Project::load_or_create(root, "ignored", "ignored").unwrap();
        assert_eq!(reloaded.creative_strategy(), "明亮欢快，避免机械重复");
    }

    #[test]
    fn corrupted_metadata_and_unsafe_names_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        assert!(Project::load_or_create(directory.path().join("x"), "../x", "").is_err());
        fs::write(
            directory.path().join(PROJECT_FILE),
            r#"{"project_name":"test","source_material":"","requirements":[],"interpretation":"","mode":"full","target_duration_secs":null,"included_instruments":[],"excluded_instruments":[],"current_version":1,"versions":{},"conversation":[]}"#,
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
}
