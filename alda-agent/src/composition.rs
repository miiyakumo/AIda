//! 长篇作曲角色原型的确定性段落组装边界。
//!
//! Worker 交付按声部拆分的 Alda 事件片段；本模块负责命名空间、固定顺序、
//! 临时时间探针和正式源码生成。探针源码只用于 `alda parse`，不会成为最终作品。

use crate::alda::ScoreInfo;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

const TIMELINE_TOLERANCE_MS: f64 = 0.01;

/// 使用分数保存的拍位，避免在组装阶段累计浮点误差。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Beat {
    pub numerator: u32,
    pub denominator: u32,
}

impl Beat {
    #[must_use]
    pub const fn new(numerator: u32, denominator: u32) -> Self {
        Self {
            numerator,
            denominator,
        }
    }

    #[must_use]
    pub const fn whole(beats: u32) -> Self {
        Self::new(beats, 1)
    }

    fn validate(self, field: &str) -> Result<()> {
        if self.denominator == 0 {
            bail!("{field} 的分母不能为 0");
        }
        Ok(())
    }

    fn checked_add(self, other: Self) -> Result<Self> {
        let numerator = self
            .numerator
            .checked_mul(other.denominator)
            .and_then(|left| {
                other
                    .numerator
                    .checked_mul(self.denominator)
                    .and_then(|right| left.checked_add(right))
            })
            .ok_or_else(|| anyhow::anyhow!("拍位相加溢出"))?;
        let denominator = self
            .denominator
            .checked_mul(other.denominator)
            .ok_or_else(|| anyhow::anyhow!("拍位分母溢出"))?;
        let divisor = gcd(numerator, denominator);
        Ok(Self::new(numerator / divisor, denominator / divisor))
    }

    fn to_millis(self, tempo_bpm: u32) -> f64 {
        60_000.0 * f64::from(self.numerator) / f64::from(self.denominator) / f64::from(tempo_bpm)
    }
}

const fn gcd(mut left: u32, mut right: u32) -> u32 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    if left == 0 { 1 } else { left }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositionSpec {
    pub tempo_bpm: u32,
    pub parts: Vec<PartSpec>,
    pub sections: Vec<SectionContract>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartSpec {
    pub id: String,
    pub instrument: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SectionContract {
    pub id: String,
    pub duration_beats: Beat,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SectionArtifact {
    pub section_id: String,
    pub parts: Vec<PartArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartArtifact {
    pub part_id: String,
    pub alda_sequence_body: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SectionAssembly {
    /// 只含正式 `%section_*` Marker 的最终 Alda 源码。
    pub alda_source: String,
    /// 额外含每声部起止 Marker 的一次性验证源码。
    pub probe_source: String,
    expectations: Vec<TimelineExpectation>,
}

#[derive(Debug, Clone, PartialEq)]
struct TimelineExpectation {
    marker: String,
    expected_beat: Beat,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TimelineVerification {
    pub checkpoints: Vec<TimelineCheckpoint>,
    pub max_error_ms: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TimelineCheckpoint {
    pub marker: String,
    pub expected_ms: f64,
    pub actual_ms: f64,
    pub error_ms: f64,
}

/// 按 `CompositionSpec` 的段落和声部顺序确定性组装 Worker 产物。
pub fn assemble_sections(
    spec: &CompositionSpec,
    artifacts: &[SectionArtifact],
) -> Result<SectionAssembly> {
    validate_inputs(spec, artifacts)?;
    let artifact_map = artifacts
        .iter()
        .map(|artifact| (artifact.section_id.as_str(), artifact))
        .collect::<BTreeMap<_, _>>();
    let alda_source = render_source(spec, &artifact_map, false)?;
    let probe_source = render_source(spec, &artifact_map, true)?;
    let expectations = timeline_expectations(spec)?;

    Ok(SectionAssembly {
        alda_source,
        probe_source,
        expectations,
    })
}

/// 将真实 `alda parse` 的 Marker offset 与声明拍位逐一核对。
pub fn verify_timeline(
    spec: &CompositionSpec,
    assembly: &SectionAssembly,
    score: &ScoreInfo,
) -> Result<TimelineVerification> {
    let actual = score
        .markers
        .iter()
        .map(|marker| (marker.name.as_str(), marker.offset_ms))
        .collect::<BTreeMap<_, _>>();
    let expected_names = assembly
        .expectations
        .iter()
        .map(|expectation| expectation.marker.as_str())
        .collect::<BTreeSet<_>>();
    let unexpected = actual
        .keys()
        .filter(|name| name.starts_with("probe_") && !expected_names.contains(**name))
        .copied()
        .collect::<Vec<_>>();
    if !unexpected.is_empty() {
        bail!("验证源码出现未声明的时间探针：{}", unexpected.join(", "));
    }

    let mut checkpoints = Vec::with_capacity(assembly.expectations.len());
    for expectation in &assembly.expectations {
        let Some(&actual_ms) = actual.get(expectation.marker.as_str()) else {
            bail!("Alda 解析结果缺少时间探针 %{}", expectation.marker);
        };
        let expected_ms = expectation.expected_beat.to_millis(spec.tempo_bpm);
        let error_ms = (actual_ms - expected_ms).abs();
        if error_ms > TIMELINE_TOLERANCE_MS {
            bail!(
                "时间探针 %{} 应位于 {:.3} ms，实际 {:.3} ms，偏差 {:.3} ms",
                expectation.marker,
                expected_ms,
                actual_ms,
                error_ms
            );
        }
        checkpoints.push(TimelineCheckpoint {
            marker: expectation.marker.clone(),
            expected_ms,
            actual_ms,
            error_ms,
        });
    }
    let max_error_ms = checkpoints
        .iter()
        .map(|checkpoint| checkpoint.error_ms)
        .fold(0.0_f64, f64::max);
    Ok(TimelineVerification {
        checkpoints,
        max_error_ms,
    })
}

fn validate_inputs(spec: &CompositionSpec, artifacts: &[SectionArtifact]) -> Result<()> {
    if spec.tempo_bpm == 0 {
        bail!("tempo_bpm 必须大于 0");
    }
    if spec.parts.is_empty() || spec.sections.is_empty() {
        bail!("CompositionSpec 至少需要一个声部和一个段落");
    }

    let mut part_ids = BTreeSet::new();
    for part in &spec.parts {
        validate_id(&part.id, "声部 ID")?;
        validate_instrument(&part.instrument, &part.id)?;
        if !part_ids.insert(part.id.as_str()) {
            bail!("声部 ID 重复：{}", part.id);
        }
    }

    let mut section_ids = BTreeSet::new();
    for section in &spec.sections {
        validate_id(&section.id, "段落 ID")?;
        section
            .duration_beats
            .validate(&format!("段落 {} 时长", section.id))?;
        if section.duration_beats.numerator == 0 {
            bail!("段落 {} 时长必须大于 0 拍", section.id);
        }
        if !section_ids.insert(section.id.as_str()) {
            bail!("段落 ID 重复：{}", section.id);
        }
    }

    let mut artifact_ids = BTreeSet::new();
    for artifact in artifacts {
        if !section_ids.contains(artifact.section_id.as_str()) {
            bail!("收到未知段落产物：{}", artifact.section_id);
        }
        if !artifact_ids.insert(artifact.section_id.as_str()) {
            bail!("段落产物重复：{}", artifact.section_id);
        }
        validate_artifact(artifact, &part_ids)?;
    }
    let missing = section_ids
        .difference(&artifact_ids)
        .copied()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        bail!("缺少段落产物：{}", missing.join(", "));
    }
    Ok(())
}

fn validate_artifact(artifact: &SectionArtifact, expected_parts: &BTreeSet<&str>) -> Result<()> {
    let mut actual_parts = BTreeSet::new();
    for part in &artifact.parts {
        if !expected_parts.contains(part.part_id.as_str()) {
            bail!("段落 {} 含未知声部 {}", artifact.section_id, part.part_id);
        }
        if !actual_parts.insert(part.part_id.as_str()) {
            bail!(
                "段落 {} 的声部产物重复：{}",
                artifact.section_id,
                part.part_id
            );
        }
        if part.alda_sequence_body.trim().is_empty() {
            bail!(
                "段落 {} 声部 {} 没有 Alda 事件；静默声部也必须用休止符填满段落",
                artifact.section_id,
                part.part_id
            );
        }
        validate_fragment_boundary(
            &part.alda_sequence_body,
            &artifact.section_id,
            &part.part_id,
        )?;
    }
    let missing = expected_parts
        .difference(&actual_parts)
        .copied()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        bail!(
            "段落 {} 缺少声部产物：{}",
            artifact.section_id,
            missing.join(", ")
        );
    }
    Ok(())
}

fn validate_fragment_boundary(code: &str, section_id: &str, part_id: &str) -> Result<()> {
    if code.contains('%') || code.contains('@') {
        bail!("段落 {section_id} 声部 {part_id} 的 Alda 片段不能自行声明或跳转 Marker");
    }
    if code.contains('=') {
        bail!("段落 {section_id} 声部 {part_id} 的 Alda 片段不能声明变量");
    }
    for (index, _) in code.match_indices(':') {
        let before = &code[..index];
        let token_start = before
            .char_indices()
            .rev()
            .find(|(_, character)| character.is_whitespace() || matches!(character, '[' | ']'))
            .map_or(0, |(token_index, character)| {
                token_index + character.len_utf8()
            });
        let token = &before[token_start..];
        if !token.strip_prefix('V').is_some_and(|number| {
            !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
        }) {
            bail!("段落 {section_id} 声部 {part_id} 的 Alda 片段不能声明或切换 instrument part");
        }
    }
    for token in code.split(|character: char| {
        !(character.is_ascii_alphanumeric() || matches!(character, '-' | '!' | '_'))
    }) {
        if token.starts_with("frag_s") {
            bail!("段落 {section_id} 声部 {part_id} 的 Alda 片段不能引用宿主或其他 Worker 名称");
        }
        if matches!(
            token,
            "tempo" | "tempo!" | "metric-modulation" | "metric-modulation!"
        ) {
            bail!("段落 {section_id} 声部 {part_id} 的 Alda 片段不能修改 tempo");
        }
    }
    let mut bracket_depth = 0_u32;
    for byte in code.bytes() {
        match byte {
            b'[' => {
                bracket_depth = bracket_depth
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("Alda 片段方括号嵌套溢出"))?;
            }
            b']' => {
                bracket_depth = bracket_depth.checked_sub(1).ok_or_else(|| {
                    anyhow::anyhow!(
                        "段落 {section_id} 声部 {part_id} 的 Alda 片段试图提前闭合宿主变量"
                    )
                })?;
            }
            _ => {}
        }
    }
    if bracket_depth != 0 {
        bail!("段落 {section_id} 声部 {part_id} 的 Alda 片段含未闭合内联序列");
    }
    Ok(())
}

fn validate_instrument(instrument: &str, part_id: &str) -> Result<()> {
    let mut bytes = instrument.bytes();
    if !bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
        || !bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        bail!("声部 {part_id} 的 Alda 乐器名必须是安全 token，实际为 {instrument:?}");
    }
    Ok(())
}

fn validate_id(id: &str, label: &str) -> Result<()> {
    let mut bytes = id.bytes();
    if !bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
        || !bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        bail!("{label} 必须匹配 [a-z][a-z0-9_]*，实际为 {id:?}");
    }
    Ok(())
}

fn render_source(
    spec: &CompositionSpec,
    artifacts: &BTreeMap<&str, &SectionArtifact>,
    include_probes: bool,
) -> Result<String> {
    let mut source = format!(
        "# generated by the section assembly prototype\n(tempo! {})\n\n",
        spec.tempo_bpm
    );

    for (section_index, section) in spec.sections.iter().enumerate() {
        let artifact = artifacts
            .get(section.id.as_str())
            .expect("validated artifact exists");
        for (part_index, part) in spec.parts.iter().enumerate() {
            let part_artifact = artifact
                .parts
                .iter()
                .find(|candidate| candidate.part_id == part.id)
                .expect("validated part artifact exists");
            writeln!(source, "{} = [", fragment_name(section_index, part_index))?;
            writeln!(
                source,
                "  (tempo {}) o4 (set-duration 1) (transpose 0)",
                spec.tempo_bpm
            )?;
            for line in part_artifact
                .alda_sequence_body
                .lines()
                .filter(|line| !line.trim().is_empty())
            {
                writeln!(source, "  {}", line.trim())?;
            }
            source.push_str("]\n\n");
        }
    }

    for (part_index, part) in spec.parts.iter().enumerate() {
        writeln!(source, "{} \"{}\":", part.instrument, part.id)?;
        for (section_index, section) in spec.sections.iter().enumerate() {
            if part_index == 0 {
                writeln!(source, "  %section_{section_index:03}_{}", section.id)?;
            }
            if include_probes {
                writeln!(
                    source,
                    "  %{}",
                    boundary_marker(section_index, part_index, "start")
                )?;
            }
            writeln!(source, "  {}", fragment_name(section_index, part_index))?;
            if include_probes {
                writeln!(
                    source,
                    "  %{}",
                    boundary_marker(section_index, part_index, "end")
                )?;
            }
        }
        source.push('\n');
    }
    Ok(source)
}

fn timeline_expectations(spec: &CompositionSpec) -> Result<Vec<TimelineExpectation>> {
    let mut section_start = Beat::whole(0);
    let mut expectations = Vec::new();
    for (section_index, section) in spec.sections.iter().enumerate() {
        let section_end = section_start.checked_add(section.duration_beats)?;
        for part_index in 0..spec.parts.len() {
            expectations.push(TimelineExpectation {
                marker: boundary_marker(section_index, part_index, "start"),
                expected_beat: section_start,
            });
            expectations.push(TimelineExpectation {
                marker: boundary_marker(section_index, part_index, "end"),
                expected_beat: section_end,
            });
        }
        section_start = section_end;
    }
    Ok(expectations)
}

fn fragment_name(section_index: usize, part_index: usize) -> String {
    format!("frag_s{section_index:03}_p{part_index:03}")
}

fn boundary_marker(section_index: usize, part_index: usize, boundary: &str) -> String {
    format!("probe_s{section_index:03}_p{part_index:03}_{boundary}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alda::{AldaRunner, find_alda};
    use std::fs;

    #[derive(Clone, Copy)]
    enum AnswerLength {
        Short,
        Exact,
        Long,
    }

    fn contrapuntal_fixture(length: AnswerLength) -> (CompositionSpec, Vec<SectionArtifact>) {
        let spec = CompositionSpec {
            tempo_bpm: 120,
            parts: vec![
                PartSpec {
                    id: "lead".to_string(),
                    instrument: "flute".to_string(),
                },
                PartSpec {
                    id: "answer".to_string(),
                    instrument: "oboe".to_string(),
                },
            ],
            sections: vec![
                SectionContract {
                    id: "exposition".to_string(),
                    duration_beats: Beat::whole(8),
                },
                SectionContract {
                    id: "development".to_string(),
                    duration_beats: Beat::whole(8),
                },
            ],
        };
        let answer_notes = match length {
            AnswerLength::Short => "r4 g a b > c < b a",
            AnswerLength::Exact => "r4 g a b > c < b a g",
            AnswerLength::Long => "r4 g a b > c < b a g f",
        };
        let artifacts = vec![
            SectionArtifact {
                section_id: "development".to_string(),
                parts: vec![
                    PartArtifact {
                        part_id: "answer".to_string(),
                        alda_sequence_body: "c1 g".to_string(),
                    },
                    PartArtifact {
                        part_id: "lead".to_string(),
                        alda_sequence_body: "c2 d e f".to_string(),
                    },
                ],
            },
            SectionArtifact {
                section_id: "exposition".to_string(),
                parts: vec![
                    PartArtifact {
                        part_id: "lead".to_string(),
                        alda_sequence_body: "c4 d e f g a b > c".to_string(),
                    },
                    PartArtifact {
                        part_id: "answer".to_string(),
                        alda_sequence_body: answer_notes.to_string(),
                    },
                ],
            },
        ];
        (spec, artifacts)
    }

    fn parse_installed_alda(source: &str) -> Option<ScoreInfo> {
        let alda = find_alda()?;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("assembly.alda");
        fs::write(&path, source).unwrap();
        Some(AldaRunner::new(alda).parse(&path).unwrap())
    }

    #[test]
    fn assembly_is_deterministic_and_does_not_jump_the_timeline() {
        let (spec, artifacts) = contrapuntal_fixture(AnswerLength::Exact);
        let first = assemble_sections(&spec, &artifacts).unwrap();
        let second = assemble_sections(&spec, &artifacts).unwrap();

        assert_eq!(first, second);
        assert!(!first.alda_source.contains("probe_"));
        assert!(!first.alda_source.contains('@'));
        assert!(first.alda_source.contains("%section_000_exposition"));
        assert!(first.alda_source.contains("frag_s000_p000"));
        assert!(!first.alda_source.contains("frag_exposition_lead"));
        assert!(
            first.alda_source.find("flute \"lead\":") < first.alda_source.find("oboe \"answer\":")
        );
    }

    #[test]
    fn indexed_host_names_do_not_collide_when_ids_do() {
        let (mut spec, artifacts) = contrapuntal_fixture(AnswerLength::Exact);
        spec.sections[0].id = "a_b".to_string();
        spec.sections[1].id = "a".to_string();
        spec.parts[0].id = "c".to_string();
        spec.parts[1].id = "b_c".to_string();
        let artifacts = artifacts
            .into_iter()
            .enumerate()
            .map(|(section_index, mut section)| {
                section.section_id = spec.sections[section_index].id.clone();
                for (part_index, part) in section.parts.iter_mut().enumerate() {
                    part.part_id = spec.parts[part_index].id.clone();
                }
                section
            })
            .collect::<Vec<_>>();

        let source = assemble_sections(&spec, &artifacts).unwrap().alda_source;

        for name in [
            "frag_s000_p000",
            "frag_s000_p001",
            "frag_s001_p000",
            "frag_s001_p001",
        ] {
            assert_eq!(source.matches(&format!("{name} = [")).count(), 1);
        }
    }

    #[test]
    fn real_alda_parse_proves_exact_section_boundaries() {
        let (spec, artifacts) = contrapuntal_fixture(AnswerLength::Exact);
        let assembly = assemble_sections(&spec, &artifacts).unwrap();
        let Some(probe_score) = parse_installed_alda(&assembly.probe_source) else {
            eprintln!("alda is not installed; skipping real parser assertion");
            return;
        };
        let verification = verify_timeline(&spec, &assembly, &probe_score).unwrap();

        assert!(verification.max_error_ms < f64::EPSILON);
        assert_eq!(verification.checkpoints.len(), 8);

        let final_score = parse_installed_alda(&assembly.alda_source).unwrap();
        // `duration_ms` is the last audible event end. Alda's default 90%
        // quantization shortens the final half note by 100 ms, while the
        // verified part cursors still end exactly at 8,000 ms.
        assert!((final_score.duration_ms - 7_900.0).abs() < f64::EPSILON);
        assert_eq!(
            final_score
                .markers
                .iter()
                .map(|marker| marker.name.as_str())
                .collect::<Vec<_>>(),
            vec!["section_000_exposition", "section_001_development"]
        );
    }

    #[test]
    fn real_alda_parse_exposes_a_short_worker_fragment() {
        let (spec, artifacts) = contrapuntal_fixture(AnswerLength::Short);
        let assembly = assemble_sections(&spec, &artifacts).unwrap();
        let Some(score) = parse_installed_alda(&assembly.probe_source) else {
            eprintln!("alda is not installed; skipping real parser assertion");
            return;
        };
        let error = verify_timeline(&spec, &assembly, &score).unwrap_err();

        assert!(error.to_string().contains("probe_s000_p001_end"));
        assert!(error.to_string().contains("3500.000 ms"));
    }

    #[test]
    fn real_alda_parse_exposes_a_long_worker_fragment() {
        let (spec, artifacts) = contrapuntal_fixture(AnswerLength::Long);
        let assembly = assemble_sections(&spec, &artifacts).unwrap();
        let Some(score) = parse_installed_alda(&assembly.probe_source) else {
            eprintln!("alda is not installed; skipping real parser assertion");
            return;
        };
        let error = verify_timeline(&spec, &assembly, &score).unwrap_err();

        assert!(error.to_string().contains("probe_s000_p001_end"));
        assert!(error.to_string().contains("4500.000 ms"));
    }

    #[test]
    fn missing_part_is_rejected() {
        let (spec, mut artifacts) = contrapuntal_fixture(AnswerLength::Exact);
        artifacts[0].parts.pop();

        let error = assemble_sections(&spec, &artifacts).unwrap_err();
        assert!(error.to_string().contains("缺少声部产物"));
    }

    #[test]
    fn worker_cannot_take_control_of_markers() {
        for injection in ["%worker_marker", "@section_000_exposition"] {
            let (spec, mut artifacts) = contrapuntal_fixture(AnswerLength::Exact);
            artifacts[1].parts[0].alda_sequence_body = injection.to_string();

            let error = assemble_sections(&spec, &artifacts).unwrap_err();
            assert!(error.to_string().contains("不能自行声明或跳转 Marker"));
        }
    }

    #[test]
    fn worker_cannot_declare_a_variable_or_part() {
        for (injection, expected) in [
            ("junk = [c1]", "不能声明变量"),
            ("violin: c1", "不能声明或切换 instrument part"),
            ("violin \"other\": c1", "不能声明或切换 instrument part"),
        ] {
            let (spec, mut artifacts) = contrapuntal_fixture(AnswerLength::Exact);
            artifacts[1].parts[0].alda_sequence_body = injection.to_string();

            let error = assemble_sections(&spec, &artifacts).unwrap_err();
            assert!(error.to_string().contains(expected));
        }
    }

    #[test]
    fn worker_cannot_reference_host_fragments_across_parts_or_families() {
        for reference in ["frag_s000_p001", "frag_s001_p000"] {
            let (spec, mut artifacts) = contrapuntal_fixture(AnswerLength::Exact);
            artifacts[1].parts[0].alda_sequence_body = reference.to_string();

            let error = assemble_sections(&spec, &artifacts).unwrap_err();
            assert!(error.to_string().contains("不能引用宿主或其他 Worker 名称"));
        }
    }

    #[test]
    fn worker_cannot_change_tempo_or_metric_modulation() {
        for injection in [
            "(tempo 90) c1",
            "(tempo! 90) c1",
            "(metric-modulation 2) c1",
            "(metric-modulation! 2) c1",
        ] {
            let (spec, mut artifacts) = contrapuntal_fixture(AnswerLength::Exact);
            artifacts[1].parts[0].alda_sequence_body = injection.to_string();

            let error = assemble_sections(&spec, &artifacts).unwrap_err();
            assert!(error.to_string().contains("不能修改 tempo"));
        }
    }

    #[test]
    fn worker_cannot_escape_the_host_sequence() {
        for injection in ["c1 ] e1 [", "[c1"] {
            let (spec, mut artifacts) = contrapuntal_fixture(AnswerLength::Exact);
            artifacts[1].parts[0].alda_sequence_body = injection.to_string();

            let error = assemble_sections(&spec, &artifacts).unwrap_err();
            assert!(
                error.to_string().contains("提前闭合宿主变量")
                    || error.to_string().contains("未闭合内联序列")
            );
        }
    }

    #[test]
    fn worker_can_use_native_voices_sequences_chords_and_repeats() {
        let (spec, mut artifacts) = contrapuntal_fixture(AnswerLength::Exact);
        artifacts[1].parts[0].alda_sequence_body = "V1: [c4/e/g r4]*4 V2: c1 d V0:".to_string();
        let assembly = assemble_sections(&spec, &artifacts).unwrap();
        let Some(score) = parse_installed_alda(&assembly.probe_source) else {
            eprintln!("alda is not installed; skipping real parser assertion");
            return;
        };
        let markers = score
            .markers
            .into_iter()
            .map(|marker| (marker.name, marker.offset_ms))
            .collect::<BTreeMap<_, _>>();

        assert!(markers["probe_s000_p000_start"].abs() < f64::EPSILON);
        assert!((markers["probe_s000_p000_end"] - 4_000.0).abs() < f64::EPSILON);
        assert!((markers["probe_s001_p000_start"] - 4_000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn instrument_must_be_a_safe_token() {
        let (mut spec, artifacts) = contrapuntal_fixture(AnswerLength::Exact);
        spec.parts[0].instrument = "flute:\nviolin".to_string();

        let error = assemble_sections(&spec, &artifacts).unwrap_err();
        assert!(error.to_string().contains("必须是安全 token"));
    }
}
