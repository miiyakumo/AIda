use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::str::FromStr;

const BUILTIN_PROGRESSIVE_SKILL: &str = include_str!("../skills/progressive-composition/SKILL.md");
pub const BUILTIN_PROGRESSIVE_SKILL_ID: &str = "builtin:progressive-composition";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkillLimits {
    pub max_skills: usize,
    pub max_header_bytes: usize,
    pub max_body_bytes: usize,
    pub max_total_loaded_bytes: usize,
}

impl Default for SkillLimits {
    fn default() -> Self {
        Self {
            max_skills: 64,
            max_header_bytes: 4 * 1024,
            max_body_bytes: 32 * 1024,
            max_total_loaded_bytes: 128 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SkillOrigin {
    Builtin,
    Project,
    User,
}

impl SkillOrigin {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

impl fmt::Display for SkillOrigin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for SkillOrigin {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "builtin" => Ok(Self::Builtin),
            "user" => Ok(Self::User),
            "project" => Ok(Self::Project),
            _ => bail!("未知 Skill 来源 {value:?}；应为 builtin、user 或 project"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct QualifiedSkillId {
    origin: SkillOrigin,
    name: String,
}

impl QualifiedSkillId {
    pub fn new(origin: SkillOrigin, name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        validate_skill_name(&name)?;
        Ok(Self { origin, name })
    }

    #[must_use]
    pub const fn origin(&self) -> SkillOrigin {
        self.origin
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl fmt::Display for QualifiedSkillId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.origin, self.name)
    }
}

impl FromStr for QualifiedSkillId {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let (origin, name) = value
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("Skill ID {value:?} 缺少来源限定"))?;
        if name.contains(':') {
            bail!("Skill ID {value:?} 格式无效");
        }
        Self::new(origin.parse()?, name)
    }
}

impl Serialize for QualifiedSkillId {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for QualifiedSkillId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SkillKind {
    Workflow,
    Advisory,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillDescriptor {
    pub id: QualifiedSkillId,
    pub name: String,
    pub description: String,
    pub kind: SkillKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedSkill {
    pub descriptor: SkillDescriptor,
    pub body: String,
}

#[derive(Debug, Clone)]
enum SkillSource {
    Builtin(&'static str),
    External {
        skill_root: PathBuf,
        document_path: PathBuf,
    },
}

#[derive(Debug, Clone)]
struct CatalogEntry {
    descriptor: SkillDescriptor,
    source: SkillSource,
}

#[derive(Debug, Clone)]
pub struct SkillCatalog {
    entries: BTreeMap<QualifiedSkillId, CatalogEntry>,
    diagnostics: Vec<String>,
    limits: SkillLimits,
}

impl SkillCatalog {
    /// Discovers direct child directories containing `SKILL.md`.
    ///
    /// Missing roots are treated as empty. Discovery parses only frontmatter;
    /// bodies are read only by [`Self::load`] or [`Self::load_active`].
    pub fn discover(
        user_skills_root: Option<&Path>,
        project_skills_root: Option<&Path>,
    ) -> Result<Self> {
        Self::discover_with_limits(
            user_skills_root,
            project_skills_root,
            SkillLimits::default(),
        )
    }

    pub fn discover_with_limits(
        user_skills_root: Option<&Path>,
        project_skills_root: Option<&Path>,
        limits: SkillLimits,
    ) -> Result<Self> {
        validate_limits(limits)?;
        let (metadata, _) = parse_skill_document(
            BUILTIN_PROGRESSIVE_SKILL.as_bytes(),
            limits.max_header_bytes,
            limits.max_body_bytes,
            false,
        )?;
        if metadata.kind != SkillKind::Workflow {
            bail!("内建 progressive-composition 必须是 workflow Skill");
        }
        let builtin_id: QualifiedSkillId = BUILTIN_PROGRESSIVE_SKILL_ID.parse()?;
        if metadata.name != builtin_id.name() {
            bail!("内建 Skill 名称必须与 {BUILTIN_PROGRESSIVE_SKILL_ID} 一致");
        }

        let builtin_descriptor = metadata.into_descriptor(builtin_id.clone());
        let mut catalog = Self {
            entries: BTreeMap::from([(
                builtin_id,
                CatalogEntry {
                    descriptor: builtin_descriptor,
                    source: SkillSource::Builtin(BUILTIN_PROGRESSIVE_SKILL),
                },
            )]),
            diagnostics: Vec::new(),
            limits,
        };

        if let Some(root) = user_skills_root {
            catalog.discover_external_root(root, SkillOrigin::User)?;
        }
        if let Some(root) = project_skills_root {
            catalog.discover_external_root(root, SkillOrigin::Project)?;
        }
        Ok(catalog)
    }

    #[must_use]
    pub fn descriptors(&self) -> Vec<&SkillDescriptor> {
        self.entries
            .values()
            .map(|entry| &entry.descriptor)
            .collect()
    }

    #[must_use]
    pub fn descriptor(&self, id: &QualifiedSkillId) -> Option<&SkillDescriptor> {
        self.entries.get(id).map(|entry| &entry.descriptor)
    }

    #[must_use]
    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    pub fn load(&self, id: &QualifiedSkillId) -> Result<LoadedSkill> {
        let entry = self
            .entries
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("未发现 Skill {id}"))?;
        let document = match &entry.source {
            SkillSource::Builtin(document) => document.as_bytes().to_vec(),
            SkillSource::External {
                skill_root,
                document_path,
            } => {
                let canonical_document = fs::canonicalize(document_path)
                    .with_context(|| format!("无法定位 Skill 文档 {}", document_path.display()))?;
                if !canonical_document.starts_with(skill_root) {
                    bail!("Skill 文档 {} 越出自身目录", document_path.display());
                }
                let maximum_document_bytes = self
                    .limits
                    .max_header_bytes
                    .saturating_add(self.limits.max_body_bytes);
                let metadata = fs::metadata(&canonical_document).with_context(|| {
                    format!("无法读取 Skill 文档元数据 {}", canonical_document.display())
                })?;
                if metadata.len() > maximum_document_bytes as u64 {
                    bail!("Skill {id} 文档超过 {maximum_document_bytes} 字节限制");
                }
                fs::read(&canonical_document).with_context(|| {
                    format!("无法读取 Skill 文档 {}", canonical_document.display())
                })?
            }
        };

        let (metadata, body) = parse_skill_document(
            &document,
            self.limits.max_header_bytes,
            self.limits.max_body_bytes,
            true,
        )?;
        let loaded_descriptor = metadata.into_descriptor(id.clone());
        if loaded_descriptor != entry.descriptor {
            bail!("Skill {id} 的元数据在发现后发生变化，请重新发现 Skill");
        }
        Ok(LoadedSkill {
            descriptor: loaded_descriptor,
            body,
        })
    }

    pub fn load_builtin_workflow(&self) -> Result<LoadedSkill> {
        self.load(&BUILTIN_PROGRESSIVE_SKILL_ID.parse()?)
    }

    /// Loads the builtin workflow followed by active advisory skills sorted by
    /// qualified ID, enforcing the aggregate body byte limit.
    pub fn load_active(&self, advisory_ids: &[QualifiedSkillId]) -> Result<Vec<LoadedSkill>> {
        let workflow = self.load_builtin_workflow()?;
        let advisory = self.load_active_advisory(advisory_ids)?;
        let total_bytes = advisory
            .iter()
            .try_fold(workflow.body.len(), |total, skill| {
                total
                    .checked_add(skill.body.len())
                    .ok_or_else(|| anyhow::anyhow!("生效 Skill 正文字节数溢出"))
            })?;
        if total_bytes > self.limits.max_total_loaded_bytes {
            let limit = self.limits.max_total_loaded_bytes;
            bail!("生效 Skill 正文总计超过 {limit} 字节限制");
        }
        let mut active = Vec::with_capacity(advisory.len() + 1);
        active.push(workflow);
        active.extend(advisory);
        Ok(active)
    }

    pub fn load_active_advisory(&self, ids: &[QualifiedSkillId]) -> Result<Vec<LoadedSkill>> {
        let unique_ids: BTreeSet<_> = ids.iter().cloned().collect();
        if unique_ids.len() != ids.len() {
            bail!("启用的 Advisory Skill ID 不能重复");
        }

        let mut total_bytes = 0usize;
        let mut loaded = Vec::with_capacity(ids.len());
        for id in unique_ids {
            let skill = self.load(&id)?;
            if skill.descriptor.kind != SkillKind::Advisory {
                bail!("Skill {id} 不是 advisory，不能通过 Advisory 配置启用");
            }
            total_bytes = total_bytes
                .checked_add(skill.body.len())
                .ok_or_else(|| anyhow::anyhow!("启用的 Skill 正文字节数溢出"))?;
            if total_bytes > self.limits.max_total_loaded_bytes {
                bail!(
                    "启用的 Advisory Skill 正文总计超过 {} 字节限制",
                    self.limits.max_total_loaded_bytes
                );
            }
            loaded.push(skill);
        }
        Ok(loaded)
    }

    fn discover_external_root(&mut self, root: &Path, origin: SkillOrigin) -> Result<()> {
        if !root.exists() {
            return Ok(());
        }
        let canonical_root = fs::canonicalize(root)
            .with_context(|| format!("无法定位 {origin} Skill 根目录 {}", root.display()))?;
        if !canonical_root.is_dir() {
            bail!("{origin} Skill 根路径 {} 不是目录", root.display());
        }

        let mut children = fs::read_dir(&canonical_root)
            .with_context(|| format!("无法扫描 Skill 根目录 {}", canonical_root.display()))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        children.sort_by_key(std::fs::DirEntry::file_name);

        let mut scanned_directories = 0usize;
        for child in children {
            let child_path = child.path();
            if !fs::metadata(&child_path).is_ok_and(|metadata| metadata.is_dir()) {
                continue;
            }
            scanned_directories += 1;
            if scanned_directories > self.limits.max_skills {
                bail!("外部 Skill 数量超过 {} 个限制", self.limits.max_skills);
            }
            match discover_external_entry(
                &canonical_root,
                &child_path,
                origin,
                self.limits.max_header_bytes,
            ) {
                Ok(Some((id, entry))) => {
                    if self.entries.insert(id.clone(), entry).is_some() {
                        self.diagnostics.push(format!("发现重复 Skill {id}"));
                    }
                }
                Ok(None) => {}
                Err(error) => self.diagnostics.push(format!(
                    "{}：{error:#}",
                    child_path.file_name().map_or_else(
                        || child_path.display().to_string(),
                        |name| name.to_string_lossy().into_owned()
                    )
                )),
            }
        }
        Ok(())
    }
}

fn discover_external_entry(
    canonical_root: &Path,
    child_path: &Path,
    origin: SkillOrigin,
    max_header_bytes: usize,
) -> Result<Option<(QualifiedSkillId, CatalogEntry)>> {
    let canonical_skill_root = fs::canonicalize(child_path)
        .with_context(|| format!("无法定位 Skill 目录 {}", child_path.display()))?;
    if !canonical_skill_root.starts_with(canonical_root) {
        bail!("Skill 目录 {} 通过符号链接越出根目录", child_path.display());
    }
    let document_path = canonical_skill_root.join("SKILL.md");
    if !document_path.exists() {
        return Ok(None);
    }
    let canonical_document = fs::canonicalize(&document_path)
        .with_context(|| format!("无法定位 Skill 文档 {}", document_path.display()))?;
    if !canonical_document.starts_with(&canonical_skill_root) {
        bail!(
            "Skill 文档 {} 通过符号链接越出 Skill 目录",
            document_path.display()
        );
    }
    let metadata = read_frontmatter(&canonical_document, max_header_bytes)?;
    if metadata.kind != SkillKind::Advisory {
        bail!("外部 Skill {origin}:{} 只能声明为 advisory", metadata.name);
    }
    let directory_name = canonical_skill_root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("Skill 目录名必须是 UTF-8"))?;
    if directory_name != metadata.name {
        bail!(
            "Skill 目录名 {directory_name:?} 与 frontmatter name {:?} 不一致",
            metadata.name
        );
    }
    let id = QualifiedSkillId::new(origin, metadata.name.clone())?;
    let descriptor = metadata.into_descriptor(id.clone());
    Ok(Some((
        id,
        CatalogEntry {
            descriptor,
            source: SkillSource::External {
                skill_root: canonical_skill_root,
                document_path: canonical_document,
            },
        },
    )))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Frontmatter {
    name: String,
    description: String,
    kind: SkillKind,
}

impl Frontmatter {
    fn into_descriptor(self, id: QualifiedSkillId) -> SkillDescriptor {
        SkillDescriptor {
            id,
            name: self.name,
            description: self.description,
            kind: self.kind,
        }
    }
}

fn read_frontmatter(path: &Path, max_header_bytes: usize) -> Result<Frontmatter> {
    let file = File::open(path)
        .with_context(|| format!("无法读取 Skill frontmatter {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut header = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        let bytes = reader.read_until(b'\n', &mut line)?;
        if bytes == 0 {
            bail!("Skill 文档 {} 缺少 frontmatter 结束标记", path.display());
        }
        header.extend_from_slice(&line);
        if header.len() > max_header_bytes {
            bail!(
                "Skill 文档 {} 的 frontmatter 超过 {} 字节限制",
                path.display(),
                max_header_bytes
            );
        }
        if trim_line_ending(&line) == b"---" && header.len() > line.len() {
            break;
        }
    }
    parse_frontmatter_bytes(&header)
}

fn parse_skill_document(
    document: &[u8],
    max_header_bytes: usize,
    max_body_bytes: usize,
    include_body: bool,
) -> Result<(Frontmatter, String)> {
    let header_end = find_frontmatter_end(document, max_header_bytes)?;
    let metadata = parse_frontmatter_bytes(&document[..header_end])?;
    if !include_body {
        return Ok((metadata, String::new()));
    }
    let body_bytes = &document[header_end..];
    if body_bytes.len() > max_body_bytes {
        bail!("Skill 正文超过 {max_body_bytes} 字节限制");
    }
    let body = std::str::from_utf8(body_bytes)
        .context("Skill 正文不是有效 UTF-8")?
        .trim()
        .to_string();
    if body.is_empty() {
        bail!("Skill 正文不能为空");
    }
    Ok((metadata, body))
}

fn find_frontmatter_end(document: &[u8], max_header_bytes: usize) -> Result<usize> {
    let mut offset = 0usize;
    let mut line_number = 0usize;
    while offset < document.len() {
        let relative_end = document[offset..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(document.len() - offset, |position| position + 1);
        offset += relative_end;
        line_number += 1;
        if offset > max_header_bytes {
            bail!("Skill frontmatter 超过 {max_header_bytes} 字节限制");
        }
        let line_start = offset - relative_end;
        if trim_line_ending(&document[line_start..offset]) == b"---" && line_number > 1 {
            return Ok(offset);
        }
    }
    bail!("Skill 文档缺少 frontmatter 结束标记")
}

fn parse_frontmatter_bytes(header: &[u8]) -> Result<Frontmatter> {
    let header = std::str::from_utf8(header).context("Skill frontmatter 不是有效 UTF-8")?;
    let mut lines = header.lines();
    if lines.next() != Some("---") {
        bail!("Skill 文档必须以 YAML frontmatter `---` 开始");
    }

    let mut name = None;
    let mut description = None;
    let mut kind = None;
    let mut closed = false;
    for line in lines {
        if line.trim_end_matches('\r') == "---" {
            closed = true;
            break;
        }
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("无效 Skill frontmatter 行 {line:?}"))?;
        let value = unquote(value.trim())?;
        match key.trim() {
            "name" => set_once(&mut name, value, "name")?,
            "description" => set_once(&mut description, value, "description")?,
            "kind" => {
                let parsed = match value.as_str() {
                    "workflow" => SkillKind::Workflow,
                    "advisory" => SkillKind::Advisory,
                    _ => bail!("Skill kind 必须是 workflow 或 advisory"),
                };
                if kind.replace(parsed).is_some() {
                    bail!("Skill frontmatter 字段 kind 重复");
                }
            }
            unknown => bail!("不支持的 Skill frontmatter 字段 {unknown:?}"),
        }
    }
    if !closed {
        bail!("Skill 文档缺少 frontmatter 结束标记");
    }
    let name = name.ok_or_else(|| anyhow::anyhow!("Skill frontmatter 缺少 name"))?;
    validate_skill_name(&name)?;
    let description =
        description.ok_or_else(|| anyhow::anyhow!("Skill frontmatter 缺少 description"))?;
    if description.trim().is_empty() {
        bail!("Skill description 不能为空");
    }
    let kind = kind.ok_or_else(|| anyhow::anyhow!("Skill frontmatter 缺少 kind"))?;
    Ok(Frontmatter {
        name,
        description,
        kind,
    })
}

fn set_once(slot: &mut Option<String>, value: String, field: &str) -> Result<()> {
    if slot.replace(value).is_some() {
        bail!("Skill frontmatter 字段 {field} 重复");
    }
    Ok(())
}

fn unquote(value: &str) -> Result<String> {
    if value.is_empty() {
        bail!("Skill frontmatter 字段值不能为空");
    }
    let bytes = value.as_bytes();
    if matches!(bytes.first(), Some(b'\'' | b'\"')) {
        if bytes.last() != bytes.first() || bytes.len() < 2 {
            bail!("Skill frontmatter 引号不匹配");
        }
        return Ok(value[1..value.len() - 1].to_string());
    }
    Ok(value.to_string())
}

fn trim_line_ending(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

fn validate_skill_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        bail!("Skill 名称长度必须为 1..=64 字节");
    }
    if !name.bytes().enumerate().all(|(index, byte)| {
        byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || (index > 0 && matches!(byte, b'-' | b'_'))
    }) {
        bail!("Skill 名称只能使用小写字母、数字、连字符和下划线，且必须以字母或数字开头");
    }
    Ok(())
}

fn validate_limits(limits: SkillLimits) -> Result<()> {
    if limits.max_skills == 0
        || limits.max_header_bytes == 0
        || limits.max_body_bytes == 0
        || limits.max_total_loaded_bytes == 0
    {
        bail!("Skill 限制必须大于零");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_skill(root: &Path, name: &str, kind: &str, body: &str) {
        let skill_root = root.join(name);
        fs::create_dir_all(&skill_root).unwrap();
        let mut file = File::create(skill_root.join("SKILL.md")).unwrap();
        write!(
            file,
            "---\nname: {name}\ndescription: 测试 Skill\nkind: {kind}\n---\n{body}"
        )
        .unwrap();
    }

    #[test]
    fn qualified_id_is_a_validated_serde_string() {
        let id: QualifiedSkillId = "project:counterpoint".parse().unwrap();
        assert_eq!(id.origin(), SkillOrigin::Project);
        assert_eq!(id.name(), "counterpoint");
        assert_eq!(
            serde_json::to_string(&id).unwrap(),
            "\"project:counterpoint\""
        );
        assert_eq!(
            serde_json::from_str::<QualifiedSkillId>("\"project:counterpoint\"").unwrap(),
            id
        );
        assert!("counterpoint".parse::<QualifiedSkillId>().is_err());
        assert!("remote:counterpoint".parse::<QualifiedSkillId>().is_err());
        assert!("user:../escape".parse::<QualifiedSkillId>().is_err());
    }

    #[test]
    fn discovery_reads_frontmatter_and_load_reads_body_on_demand() {
        let root = tempfile::tempdir().unwrap();
        write_skill(
            root.path(),
            "counterpoint",
            "advisory",
            "先固定对位轮廓。\n",
        );
        let limits = SkillLimits {
            max_body_bytes: 4,
            ..SkillLimits::default()
        };
        let catalog = SkillCatalog::discover_with_limits(Some(root.path()), None, limits).unwrap();
        let id: QualifiedSkillId = "user:counterpoint".parse().unwrap();
        assert_eq!(catalog.descriptor(&id).unwrap().kind, SkillKind::Advisory);
        assert!(catalog.load(&id).unwrap_err().to_string().contains("超过"));
    }

    #[test]
    fn external_workflow_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        write_skill(root.path(), "replacement", "workflow", "替换工作流");
        let catalog = SkillCatalog::discover(Some(root.path()), None).unwrap();
        assert!(catalog.diagnostics()[0].contains("只能声明为 advisory"));
    }

    #[test]
    fn discovery_is_single_level_and_enforces_catalog_count() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("group");
        fs::create_dir(&nested).unwrap();
        write_skill(&nested, "nested", "advisory", "不应被发现");
        write_skill(root.path(), "alpha", "advisory", "A");
        let limits = SkillLimits {
            max_skills: 2,
            ..SkillLimits::default()
        };
        let catalog = SkillCatalog::discover_with_limits(Some(root.path()), None, limits).unwrap();
        assert!(
            catalog
                .descriptor(&"user:nested".parse().unwrap())
                .is_none()
        );

        write_skill(root.path(), "beta", "advisory", "B");
        let error =
            SkillCatalog::discover_with_limits(Some(root.path()), None, limits).unwrap_err();
        assert!(error.to_string().contains("数量超过"));
    }

    #[test]
    fn oversized_frontmatter_is_rejected_during_discovery() {
        let root = tempfile::tempdir().unwrap();
        write_skill(root.path(), "verbose", "advisory", "正文不会被读取");
        let path = root.path().join("verbose/SKILL.md");
        let mut file = File::create(&path).unwrap();
        write!(
            file,
            "---\nname: verbose\ndescription: {}\nkind: advisory\n---\n正文",
            "x".repeat(512)
        )
        .unwrap();
        let limits = SkillLimits {
            max_header_bytes: 256,
            ..SkillLimits::default()
        };
        let catalog = SkillCatalog::discover_with_limits(Some(root.path()), None, limits).unwrap();
        assert!(catalog.diagnostics()[0].contains("frontmatter 超过"));
    }

    #[test]
    fn active_advisory_is_sorted_and_total_bytes_are_limited() {
        let root = tempfile::tempdir().unwrap();
        write_skill(root.path(), "zeta", "advisory", "1234");
        write_skill(root.path(), "alpha", "advisory", "5678");
        let limits = SkillLimits {
            max_total_loaded_bytes: 8,
            ..SkillLimits::default()
        };
        let catalog = SkillCatalog::discover_with_limits(None, Some(root.path()), limits).unwrap();
        let ids = vec![
            "project:zeta".parse().unwrap(),
            "project:alpha".parse().unwrap(),
        ];
        let loaded = catalog.load_active_advisory(&ids).unwrap();
        assert_eq!(loaded[0].descriptor.name, "alpha");
        assert_eq!(loaded[1].descriptor.name, "zeta");

        let too_small = SkillLimits {
            max_total_loaded_bytes: 7,
            ..SkillLimits::default()
        };
        let catalog =
            SkillCatalog::discover_with_limits(None, Some(root.path()), too_small).unwrap();
        assert!(catalog.load_active_advisory(&ids).is_err());
    }

    #[test]
    fn same_name_from_user_and_project_remains_qualified() {
        let user_root = tempfile::tempdir().unwrap();
        let project_root = tempfile::tempdir().unwrap();
        write_skill(user_root.path(), "phrasing", "advisory", "用户方法");
        write_skill(project_root.path(), "phrasing", "advisory", "项目方法");
        let catalog =
            SkillCatalog::discover(Some(user_root.path()), Some(project_root.path())).unwrap();
        let active = catalog
            .load_active_advisory(&[
                "user:phrasing".parse().unwrap(),
                "project:phrasing".parse().unwrap(),
            ])
            .unwrap();
        assert_eq!(active.len(), 2);
        assert_ne!(active[0].descriptor.id, active[1].descriptor.id);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_cannot_escape_catalog_or_skill_root() {
        use std::os::unix::fs::symlink;

        let catalog_root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write_skill(outside.path(), "escape", "advisory", "不应读取");
        symlink(
            outside.path().join("escape"),
            catalog_root.path().join("escape"),
        )
        .unwrap();
        let catalog = SkillCatalog::discover(Some(catalog_root.path()), None).unwrap();
        assert!(catalog.diagnostics()[0].contains("越出根目录"));

        let catalog_root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir(catalog_root.path().join("escape")).unwrap();
        write_skill(outside.path(), "escape", "advisory", "不应读取");
        symlink(
            outside.path().join("escape/SKILL.md"),
            catalog_root.path().join("escape/SKILL.md"),
        )
        .unwrap();
        let catalog = SkillCatalog::discover(Some(catalog_root.path()), None).unwrap();
        assert!(catalog.diagnostics()[0].contains("越出 Skill 目录"));
    }

    #[test]
    fn builtin_workflow_is_always_available() {
        let catalog = SkillCatalog::discover(None, None).unwrap();
        let workflow = catalog.load_builtin_workflow().unwrap();
        assert_eq!(workflow.descriptor.kind, SkillKind::Workflow);
        assert!(workflow.body.contains("渐进式"));
        let active = catalog.load_active(&[]).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0], workflow);

        let limits = SkillLimits {
            max_total_loaded_bytes: workflow.body.len() - 1,
            ..SkillLimits::default()
        };
        let catalog = SkillCatalog::discover_with_limits(None, None, limits).unwrap();
        assert!(catalog.load_active(&[]).is_err());
    }
}
