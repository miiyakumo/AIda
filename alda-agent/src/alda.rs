use crate::instructions::DurationConstraint;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

// ============================================================
// Alda parse JSON 数据结构
// ============================================================

#[derive(Debug, Deserialize)]
struct ParseOutput {
    #[serde(default)]
    aliases: HashMap<String, Vec<String>>,
    events: Vec<Event>,
    parts: HashMap<String, Part>,
}

#[derive(Debug, Deserialize)]
struct Event {
    part: String,
    offset: f64,
    #[serde(rename = "audible-duration")]
    audible_duration: f64,
}

#[derive(Debug, Deserialize)]
struct Part {
    #[serde(default)]
    name: Option<String>,
    #[serde(rename = "stock-instrument")]
    stock_instrument: String,
    tempo: f64,
}

// ============================================================
// 结构化结果
// ============================================================

#[derive(Debug, Clone, Serialize)]
pub struct ScoreInfo {
    /// 估算总时长（毫秒）
    pub duration_ms: f64,
    /// 声部数量
    pub part_count: usize,
    /// 实际发声事件数量
    pub event_count: usize,
    /// 当前使用的乐器列表（stock-instrument 全名）
    pub instruments: Vec<String>,
    /// tempo
    pub tempo: f64,
    /// 各声部时间范围与全局事件空档，仅用于诊断，不参与候选通过/失败判断。
    pub timeline: TimelineDiagnostics,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PartTimeline {
    pub part: String,
    pub first_event_ms: f64,
    pub last_event_ms: f64,
    pub event_count: usize,
    pub sounding_ms: f64,
    pub max_silent_gap_ms: f64,
    pub coverage_ratio: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EventGap {
    pub start_ms: f64,
    pub end_ms: f64,
    pub duration_ms: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TimelineDiagnostics {
    pub parts: Vec<PartTimeline>,
    pub ending_parts: Vec<String>,
    /// 全曲结尾与第二晚结束声部之间的差值；单声部或并列结束时为 0。
    pub ending_tail_ms: f64,
    /// 按时间顺序排列的全局事件空档。
    pub event_gaps: Vec<EventGap>,
    pub total_event_gap_ms: f64,
    pub event_gap_ratio: f64,
}

/// 检查项结果
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AldaCheck {
    pub name: &'static str,
    pub status: CheckStatus,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Fail,
    Unchecked,
}

#[derive(Debug, Clone)]
pub struct ScoreValidation {
    target_duration: Option<DurationConstraint>,
    included_instruments: Vec<String>,
    excluded_instruments: Vec<String>,
}

impl ScoreValidation {
    #[must_use]
    pub fn new(
        target_duration: Option<DurationConstraint>,
        included_instruments: Vec<String>,
        excluded_instruments: Vec<String>,
    ) -> Self {
        Self {
            target_duration,
            included_instruments,
            excluded_instruments,
        }
    }

    #[must_use]
    pub fn without_duration(mut self) -> Self {
        self.target_duration = None;
        self
    }
}

impl std::fmt::Display for CheckStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CheckStatus::Pass => write!(f, "通过"),
            CheckStatus::Fail => write!(f, "失败"),
            CheckStatus::Unchecked => write!(f, "未检查"),
        }
    }
}

const GLOBAL_EVENT_GAP_TOLERANCE_MS: f64 = 150.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkerKind {
    Definition,
    Reference,
}

#[derive(Debug, Clone)]
struct MarkerOccurrence {
    name: String,
    kind: MarkerKind,
    line: usize,
    order: usize,
}

#[derive(Debug, Default)]
struct MarkerAnalysis {
    definition_count: usize,
    reference_count: usize,
    errors: Vec<String>,
}

fn analyze_markers(source: &str) -> MarkerAnalysis {
    let occurrences = scan_markers(source);
    let mut definitions: BTreeMap<&str, Vec<&MarkerOccurrence>> = BTreeMap::new();
    let mut references = Vec::new();

    for occurrence in &occurrences {
        match occurrence.kind {
            MarkerKind::Definition => definitions
                .entry(&occurrence.name)
                .or_default()
                .push(occurrence),
            MarkerKind::Reference => references.push(occurrence),
        }
    }

    let mut errors = Vec::new();
    for (name, placements) in &definitions {
        if placements.len() > 1 {
            let lines = placements
                .iter()
                .map(|placement| placement.line.to_string())
                .collect::<Vec<_>>()
                .join("、");
            errors.push(format!("标记 %{name} 重复定义于第 {lines} 行"));
        }
    }

    for reference in &references {
        match definitions.get(reference.name.as_str()) {
            None => errors.push(format!(
                "标记 @{} 在第 {} 行引用但未定义",
                reference.name, reference.line
            )),
            Some(placements) if placements[0].order > reference.order => errors.push(format!(
                "标记 @{} 在第 {} 行先引用，后在第 {} 行定义",
                reference.name, reference.line, placements[0].line
            )),
            Some(_) => {}
        }
    }

    MarkerAnalysis {
        definition_count: definitions.values().map(Vec::len).sum(),
        reference_count: references.len(),
        errors,
    }
}

fn scan_markers(source: &str) -> Vec<MarkerOccurrence> {
    let bytes = source.as_bytes();
    let mut occurrences = Vec::new();
    let mut index = 0;
    let mut line = 1;
    let mut in_comment = false;
    let mut in_string = false;
    let mut escaped = false;

    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'\n' {
            line += 1;
            in_comment = false;
            escaped = false;
            index += 1;
            continue;
        }
        if in_comment {
            index += 1;
            continue;
        }
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if byte == b'#' {
            in_comment = true;
            index += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            index += 1;
            continue;
        }
        if matches!(byte, b'%' | b'@') {
            let start = index + 1;
            let mut end = start;
            while end < bytes.len() && is_marker_name_byte(bytes[end]) {
                end += 1;
            }
            if end > start {
                occurrences.push(MarkerOccurrence {
                    name: source[start..end].to_owned(),
                    kind: if byte == b'%' {
                        MarkerKind::Definition
                    } else {
                        MarkerKind::Reference
                    },
                    line,
                    order: index,
                });
                index = end;
                continue;
            }
        }
        index += 1;
    }

    occurrences
}

fn is_marker_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'+' | b'\'' | b'(' | b')')
}

fn marker_check(score_path: &Path) -> AldaCheck {
    match fs::read_to_string(score_path) {
        Err(error) => AldaCheck {
            name: "标记",
            status: CheckStatus::Fail,
            detail: format!("无法读取乐谱以检查标记：{error}"),
        },
        Ok(source) => {
            let analysis = analyze_markers(&source);
            if analysis.errors.is_empty() {
                AldaCheck {
                    name: "标记",
                    status: CheckStatus::Pass,
                    detail: format!(
                        "{} 个定义，{} 个引用；定义唯一且引用顺序有效",
                        analysis.definition_count, analysis.reference_count
                    ),
                }
            } else {
                AldaCheck {
                    name: "标记",
                    status: CheckStatus::Fail,
                    detail: analysis.errors.join("；"),
                }
            }
        }
    }
}

fn analyze_events(parsed: &ParseOutput) -> Result<(f64, TimelineDiagnostics)> {
    let mut by_part: BTreeMap<&str, Vec<(f64, f64)>> = BTreeMap::new();
    let mut all_intervals = Vec::with_capacity(parsed.events.len());

    for (index, event) in parsed.events.iter().enumerate() {
        if !event.offset.is_finite() || event.offset < 0.0 {
            bail!("事件 {} 的 offset 必须是有限且非负的数值", index + 1);
        }
        if !event.audible_duration.is_finite() || event.audible_duration < 0.0 {
            bail!(
                "事件 {} 的 audible-duration 必须是有限且非负的数值",
                index + 1
            );
        }
        if !parsed.parts.contains_key(&event.part) {
            bail!("事件 {} 引用了不存在的声部 {:?}", index + 1, event.part);
        }
        let end = event.offset + event.audible_duration;
        if !end.is_finite() {
            bail!("事件 {} 的结束时间不是有限数值", index + 1);
        }
        let interval = (event.offset, end);
        by_part.entry(&event.part).or_default().push(interval);
        all_intervals.push(interval);
    }

    let duration_ms = all_intervals
        .iter()
        .map(|(_, end)| *end)
        .fold(0.0_f64, f64::max);
    let mut parts = Vec::with_capacity(by_part.len());
    for (part, intervals) in by_part {
        let event_count = intervals.len();
        let merged = merge_intervals(intervals, 0.0);
        let first_event_ms = merged.first().map_or(0.0, |(start, _)| *start);
        let last_event_ms = merged.last().map_or(0.0, |(_, end)| *end);
        let sounding_ms: f64 = merged.iter().map(|(start, end)| end - start).sum();
        let max_silent_gap_ms = merged
            .windows(2)
            .map(|pair| pair[1].0 - pair[0].1)
            .fold(0.0_f64, f64::max);
        let span_ms = last_event_ms - first_event_ms;
        let coverage_ratio = if span_ms > 0.0 {
            (sounding_ms / span_ms).clamp(0.0, 1.0)
        } else {
            0.0
        };
        parts.push(PartTimeline {
            part: readable_part_name(parsed, part),
            first_event_ms,
            last_event_ms,
            event_count,
            sounding_ms,
            max_silent_gap_ms,
            coverage_ratio,
        });
    }

    let ending_parts = parts
        .iter()
        .filter(|part| (part.last_event_ms - duration_ms).abs() <= 0.001)
        .map(|part| part.part.clone())
        .collect();
    let mut part_endings = parts
        .iter()
        .map(|part| part.last_event_ms)
        .collect::<Vec<_>>();
    part_endings.sort_by(|left, right| right.total_cmp(left));
    let ending_tail_ms = part_endings
        .get(1)
        .map_or(0.0, |second| (duration_ms - second).max(0.0));
    let merged_global = merge_intervals(all_intervals, GLOBAL_EVENT_GAP_TOLERANCE_MS);
    let mut event_gaps = Vec::new();
    if let Some((first_start, _)) = merged_global.first() {
        if *first_start > GLOBAL_EVENT_GAP_TOLERANCE_MS {
            event_gaps.push(EventGap {
                start_ms: 0.0,
                end_ms: *first_start,
                duration_ms: *first_start,
            });
        }
    }
    event_gaps.extend(merged_global.windows(2).map(|pair| EventGap {
        start_ms: pair[0].1,
        end_ms: pair[1].0,
        duration_ms: pair[1].0 - pair[0].1,
    }));
    let total_event_gap_ms = event_gaps.iter().map(|gap| gap.duration_ms).sum();
    let event_gap_ratio = if duration_ms > 0.0 {
        total_event_gap_ms / duration_ms
    } else {
        0.0
    };

    Ok((
        duration_ms,
        TimelineDiagnostics {
            parts,
            ending_parts,
            ending_tail_ms,
            event_gaps,
            total_event_gap_ms,
            event_gap_ratio,
        },
    ))
}

fn readable_part_name(parsed: &ParseOutput, part_id: &str) -> String {
    let Some(part) = parsed.parts.get(part_id) else {
        return part_id.to_string();
    };
    let alias = parsed
        .aliases
        .iter()
        .filter(|(alias, ids)| !alias.contains('.') && ids.iter().any(|id| id == part_id))
        .map(|(alias, _)| alias.as_str())
        .min_by_key(|alias| (alias.len(), *alias));
    if let Some(alias) = alias {
        return format!("{alias}（{}）", part.stock_instrument);
    }
    if let Some(name) = part.name.as_deref().filter(|name| !name.is_empty()) {
        return name.to_string();
    }
    if !looks_like_internal_part_id(part_id) {
        return part_id.to_string();
    }
    if !part.stock_instrument.is_empty() {
        return part.stock_instrument.clone();
    }
    format!("内部声部 {part_id}")
}

fn looks_like_internal_part_id(part_id: &str) -> bool {
    part_id
        .strip_prefix("0x")
        .is_some_and(|hex| !hex.is_empty() && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn merge_intervals(mut intervals: Vec<(f64, f64)>, tolerance_ms: f64) -> Vec<(f64, f64)> {
    intervals.sort_by(|left, right| {
        left.0
            .total_cmp(&right.0)
            .then_with(|| left.1.total_cmp(&right.1))
    });
    let mut merged: Vec<(f64, f64)> = Vec::with_capacity(intervals.len());
    for (start, end) in intervals {
        match merged.last_mut() {
            Some((_, previous_end)) if start <= *previous_end + tolerance_ms => {
                *previous_end = previous_end.max(end);
            }
            _ => merged.push((start, end)),
        }
    }
    merged
}

// ============================================================
// Alda 命令执行器
// ============================================================

#[derive(Debug, Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Clone)]
pub struct AldaRunner {
    alda_path: PathBuf,
    timeout: Duration,
    max_output_bytes: usize,
    cancellation: CancellationToken,
}

struct CapturedOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

type ReaderThread = thread::JoinHandle<std::io::Result<(Vec<u8>, bool)>>;

impl AldaRunner {
    #[must_use]
    pub fn new(alda_path: PathBuf) -> Self {
        AldaRunner {
            alda_path,
            // Alda 2.3.3 may start its background player processes even for
            // `export`; a cold JVM/player startup can exceed one minute.
            timeout: Duration::from_secs(120),
            max_output_bytes: 10 * 1024 * 1024, // 10MB
            cancellation: CancellationToken::default(),
        }
    }

    #[must_use]
    pub fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    #[must_use]
    pub fn with_limits(mut self, timeout: Duration, max_output_bytes: usize) -> Self {
        self.timeout = timeout;
        self.max_output_bytes = max_output_bytes;
        self
    }

    /// 解析 Alda 文件，返回结构化信息
    pub fn parse(&self, score_path: &Path) -> Result<ScoreInfo> {
        let output = self.run_alda(&["parse", "-f", &score_path.to_string_lossy()])?;

        let parsed: ParseOutput =
            serde_json::from_str(&output).context("无法解析 alda parse 输出为 JSON")?;

        let (duration_ms, timeline) = analyze_events(&parsed).context("Alda 事件数据无效")?;

        let part_count = parsed.parts.len();
        let event_count = parsed.events.len();

        let instruments: Vec<String> = parsed
            .parts
            .values()
            .map(|p| p.stock_instrument.clone())
            .collect();

        // tempo：取第一个 part 的 tempo 值
        let tempo = parsed.parts.values().next().map_or(0.0, |p| p.tempo);

        Ok(ScoreInfo {
            duration_ms,
            part_count,
            event_count,
            instruments,
            tempo,
            timeline,
        })
    }

    /// 导出 MIDI
    pub fn export_midi(&self, score_path: &Path, output_path: &Path) -> Result<PathBuf> {
        self.run_alda(&[
            "export",
            "-f",
            &score_path.to_string_lossy(),
            "-o",
            &output_path.to_string_lossy(),
        ])?;

        if output_path.exists() {
            Ok(output_path.to_path_buf())
        } else {
            bail!("MIDI 文件未生成: {}", output_path.display())
        }
    }

    /// 播放（非阻塞）
    pub fn play(&self, score_path: &Path) -> Result<()> {
        self.run_alda_no_capture(&["play", "-f", &score_path.to_string_lossy()])?;
        Ok(())
    }

    /// 停止播放
    pub fn stop(&self) -> Result<()> {
        self.run_alda_no_capture(&["stop"])?;
        Ok(())
    }

    /// 列出所有可用乐器
    pub fn list_instruments(&self) -> Result<Vec<String>> {
        let output = self.run_alda(&["instruments", "list"])?;
        Ok(output
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect())
    }

    // ============================================================
    // 检查方法
    // ============================================================

    /// 对乐谱执行一系列检查，返回检查结果列表
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn validate(
        &self,
        score_path: &Path,
        included_instruments: &[String],
        excluded_instruments: &[String],
        target_duration: Option<DurationConstraint>,
        duration_tolerance_pct: f64,
    ) -> Vec<AldaCheck> {
        let mut checks = Vec::new();
        let markers = marker_check(score_path);

        let info = match self.parse(score_path) {
            Ok(info) => info,
            Err(e) => {
                checks.push(AldaCheck {
                    name: "Alda 语法",
                    status: CheckStatus::Fail,
                    detail: format!("{e}"),
                });
                checks.push(markers);
                // 语法失败则后续检查跳过
                checks.push(AldaCheck {
                    name: "时长",
                    status: CheckStatus::Unchecked,
                    detail: "语法检查未通过，跳过".into(),
                });
                checks.push(AldaCheck {
                    name: "乐器",
                    status: CheckStatus::Unchecked,
                    detail: "语法检查未通过，跳过".into(),
                });
                return checks;
            }
        };

        // 1. 语法检查通过
        checks.push(AldaCheck {
            name: "Alda 语法",
            status: CheckStatus::Pass,
            detail: "解析成功".into(),
        });
        checks.push(markers);

        // Alda 会接受空文件，因此解析成功后仍需验证作品确实可播放。
        if info.part_count == 0 || info.event_count == 0 || info.duration_ms <= 0.0 {
            checks.push(AldaCheck {
                name: "作品内容",
                status: CheckStatus::Fail,
                detail: format!(
                    "作品为空或没有可播放事件（{} 声部，{} 事件，约 {:.0} 秒）",
                    info.part_count,
                    info.event_count,
                    info.duration_ms / 1000.0
                ),
            });
        } else {
            checks.push(AldaCheck {
                name: "作品内容",
                status: CheckStatus::Pass,
                detail: format!(
                    "{} 声部，{} 个可播放事件",
                    info.part_count, info.event_count
                ),
            });
        }

        let endings = if info.timeline.ending_parts.is_empty() {
            "无".to_string()
        } else {
            info.timeline.ending_parts.join("、")
        };
        let gaps = if info.timeline.event_gaps.is_empty() {
            "无超过 150ms 的全局事件空档".to_string()
        } else {
            let mut longest = info.timeline.event_gaps.iter().collect::<Vec<_>>();
            longest.sort_by(|left, right| right.duration_ms.total_cmp(&left.duration_ms));
            longest
                .into_iter()
                .take(3)
                .map(|gap| {
                    format!(
                        "{:.1}–{:.1}秒（{:.1}秒）",
                        gap.start_ms / 1000.0,
                        gap.end_ms / 1000.0,
                        gap.duration_ms / 1000.0
                    )
                })
                .collect::<Vec<_>>()
                .join("，")
        };
        checks.push(AldaCheck {
            name: "声部时间轴/事件空档",
            status: CheckStatus::Unchecked,
            detail: format!(
                "{} 个有事件声部；决定结尾：{endings}；结尾尾差 {:.1}秒；事件空档占比 {:.1}%；{gaps}",
                info.timeline.parts.len(),
                info.timeline.ending_tail_ms / 1000.0,
                info.timeline.event_gap_ratio * 100.0,
            ),
        });

        // 2. 时长检查
        if let Some(target) = target_duration {
            if target.validate().is_err() {
                checks.push(AldaCheck {
                    name: "时长",
                    status: CheckStatus::Fail,
                    detail: "目标时长必须是大于 0 的有限数值".to_string(),
                });
                return checks;
            }
            let actual_seconds = info.duration_ms / 1000.0;
            let (min_seconds, max_seconds) = target.validation_bounds(duration_tolerance_pct);
            let passed = (min_seconds..=max_seconds).contains(&actual_seconds);
            let target_detail = match target {
                DurationConstraint::Exact(seconds) => {
                    format!("目标 {seconds:.0}秒，允许偏差 {duration_tolerance_pct:.0}%")
                }
                DurationConstraint::Range { min_secs, max_secs } => {
                    format!("目标 {min_secs:.0}–{max_secs:.0}秒")
                }
            };
            checks.push(AldaCheck {
                name: "时长",
                status: if passed {
                    CheckStatus::Pass
                } else {
                    CheckStatus::Fail
                },
                detail: format!("约 {actual_seconds:.0}秒（{target_detail}）"),
            });
        } else {
            checks.push(AldaCheck {
                name: "时长",
                status: CheckStatus::Unchecked,
                detail: format!("约 {:.0}秒（未指定目标时长）", info.duration_ms / 1000.0),
            });
        }

        // 3. 乐器检查
        let mut instrument_checks = Vec::new();

        // 检查必须包含的乐器（子串匹配）
        for required in included_instruments {
            let found = info
                .instruments
                .iter()
                .any(|inst| inst.to_lowercase().contains(&required.to_lowercase()));
            if found {
                instrument_checks.push(AldaCheck {
                    name: "包含乐器",
                    status: CheckStatus::Pass,
                    detail: format!("\"{required}\" 已在乐谱中"),
                });
            } else {
                instrument_checks.push(AldaCheck {
                    name: "包含乐器",
                    status: CheckStatus::Fail,
                    detail: format!(
                        "\"{}\" 未出现在乐谱中（现用：{}）",
                        required,
                        info.instruments.join(", ")
                    ),
                });
            }
        }

        // 检查排除的乐器（子串匹配）
        for excluded in excluded_instruments {
            let found: Vec<&String> = info
                .instruments
                .iter()
                .filter(|inst| inst.to_lowercase().contains(&excluded.to_lowercase()))
                .collect();

            if found.is_empty() {
                instrument_checks.push(AldaCheck {
                    name: "排除乐器",
                    status: CheckStatus::Pass,
                    detail: format!("\"{excluded}\" 未出现在乐谱中"),
                });
            } else {
                instrument_checks.push(AldaCheck {
                    name: "排除乐器",
                    status: CheckStatus::Fail,
                    detail: format!(
                        "\"{}\" 仍然出现：{}",
                        excluded,
                        found
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                });
            }
        }

        // 若没有任何乐器约束，添加一条
        if included_instruments.is_empty() && excluded_instruments.is_empty() {
            instrument_checks.push(AldaCheck {
                name: "乐器",
                status: CheckStatus::Unchecked,
                detail: format!("当前配器：{}（未指定约束）", info.instruments.join(", ")),
            });
        }

        checks.extend(instrument_checks);
        checks
    }

    pub async fn validate_async(
        &self,
        score_path: PathBuf,
        validation: ScoreValidation,
    ) -> Result<Vec<AldaCheck>> {
        let runner = self.clone();
        let checks = tokio::task::spawn_blocking(move || {
            runner.validate(
                &score_path,
                &validation.included_instruments,
                &validation.excluded_instruments,
                validation.target_duration,
                10.0,
            )
        })
        .await
        .context("Alda 校验任务异常退出")?;
        if self.cancellation.is_cancelled() {
            bail!("Alda 校验已由用户取消，子进程已终止");
        }
        Ok(checks)
    }

    pub async fn play_async(&self, score_path: PathBuf) -> Result<()> {
        let runner = self.clone();
        tokio::task::spawn_blocking(move || runner.play(&score_path))
            .await
            .context("Alda 播放任务异常退出")?
    }

    pub async fn stop_async(&self) -> Result<()> {
        let runner = self.clone();
        tokio::task::spawn_blocking(move || runner.stop())
            .await
            .context("Alda 停止任务异常退出")?
    }

    pub async fn export_midi_async(
        &self,
        score_path: PathBuf,
        output_path: PathBuf,
    ) -> Result<PathBuf> {
        let runner = self.clone();
        tokio::task::spawn_blocking(move || runner.export_midi(&score_path, &output_path))
            .await
            .context("Alda 导出任务异常退出")?
    }

    // ============================================================
    // 内部命令执行
    // ============================================================

    /// 运行 alda 命令，捕获 stdout（合并 stderr），返回字符串
    fn run_alda(&self, args: &[&str]) -> Result<String> {
        let output = self.run_process(args, true)?;

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

        if !output.status.success() {
            bail!("Alda 命令失败: {}", stderr.trim());
        }

        Ok(stdout)
    }

    /// 运行 alda 命令，不捕获输出（用于 play/stop）
    fn run_alda_no_capture(&self, args: &[&str]) -> Result<()> {
        let output = self.run_process(args, false)?;
        if !output.status.success() {
            bail!("Alda 命令失败");
        }
        Ok(())
    }

    fn run_process(&self, args: &[&str], capture: bool) -> Result<CapturedOutput> {
        let mut command = Command::new(&self.alda_path);
        command.args(args);
        if capture {
            command.stdout(Stdio::piped()).stderr(Stdio::piped());
        } else {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
        command.process_group(0);

        let mut attempts = 0;
        let mut child = loop {
            match command.spawn() {
                Ok(child) => break child,
                Err(error) if error.raw_os_error() == Some(26) && attempts < 3 => {
                    attempts += 1;
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error).context("无法执行 alda"),
            }
        };
        let stdout_reader = child.stdout.take();
        let stderr_reader = child.stderr.take();
        let limit = self.max_output_bytes;
        let stdout_thread =
            stdout_reader.map(|reader| thread::spawn(move || read_limited(reader, limit)));
        let stderr_thread =
            stderr_reader.map(|reader| thread::spawn(move || read_limited(reader, limit)));
        let deadline = Instant::now() + self.timeout;

        let status = loop {
            if let Some(status) = child.try_wait().context("无法等待 alda 子进程")? {
                break status;
            }
            if self.cancellation.is_cancelled() {
                terminate_process_group(child.id());
                let _ = child.kill();
                let _ = child.wait();
                join_reader(stdout_thread)?;
                join_reader(stderr_thread)?;
                bail!("Alda 命令已由用户取消，子进程已终止");
            }
            if Instant::now() >= deadline {
                terminate_process_group(child.id());
                let _ = child.kill();
                let _ = child.wait();
                join_reader(stdout_thread)?;
                join_reader(stderr_thread)?;
                bail!(
                    "Alda 命令超时（{} 秒），子进程已终止",
                    self.timeout.as_secs_f64()
                );
            }
            thread::sleep(Duration::from_millis(10));
        };

        let (stdout, stdout_exceeded) = join_reader(stdout_thread)?;
        let (stderr, stderr_exceeded) = join_reader(stderr_thread)?;
        if stdout_exceeded || stderr_exceeded {
            bail!("Alda 输出超过上限（{} 字节）", self.max_output_bytes);
        }
        Ok(CapturedOutput {
            status,
            stdout,
            stderr,
        })
    }
}

fn terminate_process_group(process_id: u32) {
    let group = format!("-{process_id}");
    let _ = Command::new("kill")
        .args(["-TERM", "--", group.as_str()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn read_limited(mut reader: impl Read, limit: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let mut output = Vec::new();
    let mut exceeded = false;
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(output.len());
        output.extend_from_slice(&buffer[..read.min(remaining)]);
        exceeded |= read > remaining;
    }
    Ok((output, exceeded))
}

fn join_reader(reader: Option<ReaderThread>) -> Result<(Vec<u8>, bool)> {
    reader.map_or_else(
        || Ok((Vec::new(), false)),
        |handle| {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("读取 Alda 输出的线程异常退出"))
                .and_then(|result| result.context("读取 Alda 输出失败"))
        },
    )
}

// ============================================================
// 查找 alda 可执行文件
// ============================================================

/// 在 PATH 和常见目录中查找 alda 可执行文件
#[must_use]
pub fn find_alda() -> Option<PathBuf> {
    // 先尝试 which
    if let Ok(output) = Command::new("which").arg("alda").output() {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }

    // 遍历常见目录
    for dir in [
        "/usr/local/bin",
        "/usr/bin",
        "/opt/homebrew/bin",
        "/home/linuxbrew/.linuxbrew/bin",
    ] {
        let full = Path::new(dir).join("alda");
        if full.exists() {
            return Some(full);
        }
    }

    // 检查 HOME/.local/bin
    if let Ok(home) = std::env::var("HOME") {
        let full = Path::new(&home).join(".local").join("bin").join("alda");
        if full.exists() {
            return Some(full);
        }
    }

    None
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    const SIMPLE_JSON: &str = r#"{"events":[{"offset":0,"duration":500,"audible-duration":450,"midi-note":60,"part":"piano"},{"offset":500,"duration":500,"audible-duration":450,"midi-note":62,"part":"piano"}],"parts":{"piano":{"name":"piano","stock-instrument":"midi-acoustic-grand-piano","tempo":120}}}"#;
    const MULTI_JSON: &str = r#"{"events":[{"offset":0,"duration":500,"audible-duration":450,"midi-note":60,"part":"piano"},{"offset":0,"duration":500,"audible-duration":450,"midi-note":69,"part":"violin"}],"parts":{"piano":{"name":"piano","stock-instrument":"midi-acoustic-grand-piano","tempo":120},"violin":{"name":"violin","stock-instrument":"midi-violin","tempo":120}}}"#;
    const TIMELINE_JSON: &str = r#"{"events":[{"offset":5000,"audible-duration":1000,"part":"piano"},{"offset":0,"audible-duration":1000,"part":"piano"},{"offset":3000,"audible-duration":500,"part":"flute"}],"parts":{"piano":{"stock-instrument":"midi-acoustic-grand-piano","tempo":120},"flute":{"stock-instrument":"midi-flute","tempo":120}}}"#;
    const INVALID_TIME_JSON: &str = r#"{"events":[{"offset":-1,"audible-duration":100,"part":"piano"}],"parts":{"piano":{"stock-instrument":"midi-acoustic-grand-piano","tempo":120}}}"#;
    const UNKNOWN_PART_JSON: &str = r#"{"events":[{"offset":0,"audible-duration":100,"part":"missing"}],"parts":{"piano":{"stock-instrument":"midi-acoustic-grand-piano","tempo":120}}}"#;
    const EMPTY_JSON: &str = r#"{"events":[],"parts":{}}"#;

    fn runner() -> (tempfile::TempDir, AldaRunner) {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("alda");
        let script = format!(
            r#"#!/bin/sh
command="$1"
if [ "$command" = "parse" ]; then
  score="$3"
  case "$score" in
    *slow.alda) sleep 2 ;;
    *invalid_syntax.alda|*invalid_instrument.alda) echo "invalid score" >&2; exit 1 ;;
    *empty.alda) printf '%s\n' '{EMPTY_JSON}' ;;
    *invalid_time_event.alda) printf '%s\n' '{INVALID_TIME_JSON}' ;;
    *unknown_part_event.alda) printf '%s\n' '{UNKNOWN_PART_JSON}' ;;
    *timeline.alda) printf '%s\n' '{TIMELINE_JSON}' ;;
    *valid_multi_part.alda) printf '%s\n' '{MULTI_JSON}' ;;
    *) printf '%s\n' '{SIMPLE_JSON}' ;;
  esac
elif [ "$command" = "export" ]; then
  while [ "$#" -gt 0 ]; do
    if [ "$1" = "-o" ]; then shift; : > "$1"; exit 0; fi
    shift
  done
  exit 1
elif [ "$command" = "instruments" ]; then
  i=0
  while [ "$i" -lt 128 ]; do printf 'instrument-%s\n' "$i"; i=$((i + 1)); done
elif [ "$command" = "play" ] || [ "$command" = "stop" ]; then
  exit 0
else
  exit 1
fi
"#
        );
        fs::write(&executable, script).unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        (directory, AldaRunner::new(executable))
    }

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    fn score_file(directory: &tempfile::TempDir, name: &str, source: &str) -> PathBuf {
        let path = directory.path().join(name);
        fs::write(&path, source).unwrap();
        path
    }

    fn part(stock_instrument: &str) -> Part {
        Part {
            name: None,
            stock_instrument: stock_instrument.to_string(),
            tempo: 120.0,
        }
    }

    fn assert_approx(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1.0e-9,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn test_parse_valid_simple() {
        let (_directory, runner) = runner();
        let info = runner.parse(&fixture("valid_simple.alda")).unwrap();
        assert_eq!(info.part_count, 1);
        assert!(info.duration_ms > 0.0);
        assert!(info.instruments.len() == 1);
    }

    #[test]
    fn test_parse_valid_multi_part() {
        let (_directory, runner) = runner();
        let info = runner.parse(&fixture("valid_multi_part.alda")).unwrap();
        assert_eq!(info.part_count, 2);
        assert!(info.instruments.len() == 2);
    }

    #[test]
    fn timeline_merges_overlapping_unsorted_events() {
        let parsed = ParseOutput {
            aliases: HashMap::new(),
            events: vec![
                Event {
                    part: "piano".into(),
                    offset: 1_000.0,
                    audible_duration: 500.0,
                },
                Event {
                    part: "piano".into(),
                    offset: 0.0,
                    audible_duration: 800.0,
                },
                Event {
                    part: "piano".into(),
                    offset: 700.0,
                    audible_duration: 500.0,
                },
            ],
            parts: HashMap::from([("piano".into(), part("midi-piano"))]),
        };

        let (duration, diagnostics) = analyze_events(&parsed).unwrap();
        assert_approx(duration, 1_500.0);
        assert_eq!(diagnostics.ending_parts, ["piano"]);
        let piano = &diagnostics.parts[0];
        assert_approx(piano.first_event_ms, 0.0);
        assert_approx(piano.last_event_ms, 1_500.0);
        assert_eq!(piano.event_count, 3);
        assert_approx(piano.sounding_ms, 1_500.0);
        assert_approx(piano.max_silent_gap_ms, 0.0);
        assert_approx(piano.coverage_ratio, 1.0);
        assert_approx(diagnostics.ending_tail_ms, 0.0);
        assert!(diagnostics.event_gaps.is_empty());
        assert_approx(diagnostics.total_event_gap_ms, 0.0);
        assert_approx(diagnostics.event_gap_ratio, 0.0);
    }

    #[test]
    fn timeline_uses_alias_instead_of_internal_part_id() {
        let parsed: ParseOutput = serde_json::from_str(
            r#"{
                "aliases": {
                    "violin-a": ["0xc000578000"],
                    "violin-a.violin": ["0xc000578000"]
                },
                "events": [{
                    "offset": 0,
                    "audible-duration": 1000,
                    "part": "0xc000578000"
                }],
                "parts": {
                    "0xc000578000": {
                        "name": "violin",
                        "stock-instrument": "midi-tremolo-strings",
                        "tempo": 120
                    }
                }
            }"#,
        )
        .unwrap();

        let (_, diagnostics) = analyze_events(&parsed).unwrap();

        assert_eq!(
            diagnostics.ending_parts,
            ["violin-a（midi-tremolo-strings）"]
        );
        assert_eq!(
            diagnostics.parts[0].part,
            "violin-a（midi-tremolo-strings）"
        );
    }

    #[test]
    fn late_entry_and_long_rests_are_diagnostics_not_failures() {
        let (directory, runner) = runner();
        let path = score_file(
            &directory,
            "timeline.alda",
            "piano: c1 r1~1~1~1 c1\nflute: r1~1~1 c2",
        );
        let info = runner.parse(&path).unwrap();
        assert_eq!(info.timeline.parts.len(), 2);
        assert_eq!(info.timeline.ending_parts, ["piano"]);
        assert_approx(info.timeline.ending_tail_ms, 2_500.0);
        assert_eq!(info.timeline.event_gaps.len(), 2);
        assert_approx(info.timeline.event_gaps[0].duration_ms, 2_000.0);
        assert_approx(info.timeline.event_gaps[1].duration_ms, 1_500.0);
        assert_approx(info.timeline.total_event_gap_ms, 3_500.0);
        assert_approx(info.timeline.event_gap_ratio, 3_500.0 / 6_000.0);

        let checks = runner.validate(&path, &[], &[], None, 10.0);
        assert!(!checks.iter().any(|check| check.status == CheckStatus::Fail));
        let timeline = checks
            .iter()
            .find(|check| check.name == "声部时间轴/事件空档")
            .unwrap();
        assert_eq!(timeline.status, CheckStatus::Unchecked);
        assert!(timeline.detail.contains("piano"));
    }

    #[test]
    fn invalid_event_times_and_unknown_parts_are_rejected() {
        for event in [
            Event {
                part: "piano".into(),
                offset: f64::NAN,
                audible_duration: 100.0,
            },
            Event {
                part: "piano".into(),
                offset: 0.0,
                audible_duration: -1.0,
            },
            Event {
                part: "missing".into(),
                offset: 0.0,
                audible_duration: 100.0,
            },
        ] {
            let parsed = ParseOutput {
                aliases: HashMap::new(),
                events: vec![event],
                parts: HashMap::from([("piano".into(), part("midi-piano"))]),
            };
            assert!(analyze_events(&parsed).is_err());
        }
    }

    #[test]
    fn parse_rejects_invalid_event_data_from_alda_output() {
        let (directory, runner) = runner();
        let invalid_time = score_file(&directory, "invalid_time_event.alda", "piano: c1");
        let unknown_part = score_file(&directory, "unknown_part_event.alda", "piano: c1");

        let time_error = format!("{:#}", runner.parse(&invalid_time).unwrap_err());
        assert!(time_error.contains("offset"), "{time_error}");
        let part_error = format!("{:#}", runner.parse(&unknown_part).unwrap_err());
        assert!(part_error.contains("不存在的声部"), "{part_error}");
    }

    #[test]
    fn marker_scanner_allows_one_definition_and_multiple_references() {
        let analysis = analyze_markers(
            "# %ignored @ignored\npiano \"alias-%ignored\": r1 %theme\nflute: @theme c1\noboe: @theme e1",
        );
        assert!(analysis.errors.is_empty(), "{:?}", analysis.errors);
        assert_eq!(analysis.definition_count, 1);
        assert_eq!(analysis.reference_count, 2);
    }

    #[test]
    fn marker_scanner_rejects_duplicate_undefined_and_forward_references() {
        let duplicate = analyze_markers("piano: %theme c1\nflute: %theme e1");
        assert!(duplicate.errors[0].contains("%theme"));
        assert!(duplicate.errors[0].contains("第 1、2 行"));

        let undefined = analyze_markers("piano: @missing c1");
        assert!(undefined.errors[0].contains("@missing"));
        assert!(undefined.errors[0].contains("第 1 行"));
        assert!(undefined.errors[0].contains("未定义"));

        let forward = analyze_markers("piano: @later c1\nflute: r1 %later");
        assert!(forward.errors[0].contains("@later"));
        assert!(forward.errors[0].contains("第 1 行"));
        assert!(forward.errors[0].contains("第 2 行"));
    }

    #[test]
    fn marker_errors_are_hard_validation_failures() {
        let (directory, runner) = runner();
        let path = score_file(
            &directory,
            "markers.alda",
            "piano: %theme c1\nflute: %theme e1",
        );
        let checks = runner.validate(&path, &[], &[], None, 10.0);
        let markers = checks.iter().find(|check| check.name == "标记").unwrap();
        assert_eq!(markers.status, CheckStatus::Fail);
        assert!(markers.detail.contains("%theme"));
        assert!(markers.detail.contains("第 1、2 行"));
    }

    #[test]
    fn test_parse_invalid_syntax() {
        let (_directory, runner) = runner();
        let result = runner.parse(&fixture("invalid_syntax.alda"));
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_empty() {
        let (_directory, runner) = runner();
        let result = runner.parse(&fixture("empty.alda"));
        // 空文件可能解析成功但 events 为空
        if let Ok(info) = result {
            assert!(info.duration_ms.abs() < f64::EPSILON);
            assert_eq!(info.part_count, 0);
        }
    }

    #[test]
    fn test_validate_empty_score_fails() {
        let (_directory, runner) = runner();
        let checks = runner.validate(&fixture("empty.alda"), &[], &[], None, 10.0);
        let content = checks
            .iter()
            .find(|check| check.name == "作品内容")
            .unwrap_or_else(|| panic!("缺少作品内容检查：{checks:?}"));
        assert_eq!(content.status, CheckStatus::Fail);
    }

    #[test]
    fn test_validate_duration_pass() {
        let (_directory, runner) = runner();
        let info = runner.parse(&fixture("valid_simple.alda")).unwrap();
        let target = info.duration_ms / 1000.0; // 精确目标（秒）
        let checks = runner.validate(
            &fixture("valid_simple.alda"),
            &[],
            &[],
            Some(DurationConstraint::exact(target)),
            10.0,
        );
        let duration_check = checks.iter().find(|c| c.name == "时长").unwrap();
        assert_eq!(duration_check.status, CheckStatus::Pass);
    }

    #[test]
    fn invalid_duration_target_fails_without_division() {
        let (_directory, runner) = runner();
        let checks = runner.validate(
            &fixture("valid_simple.alda"),
            &[],
            &[],
            Some(DurationConstraint::exact(0.0)),
            10.0,
        );
        let duration = checks.iter().find(|check| check.name == "时长").unwrap();
        assert_eq!(duration.status, CheckStatus::Fail);
        assert!(duration.detail.contains("大于 0"));
    }

    #[test]
    fn duration_range_uses_hard_bounds() {
        let (_directory, runner) = runner();
        let info = runner.parse(&fixture("valid_simple.alda")).unwrap();
        let actual = info.duration_ms / 1000.0;
        let inside = runner.validate(
            &fixture("valid_simple.alda"),
            &[],
            &[],
            Some(DurationConstraint::range(actual * 0.5, actual * 1.5)),
            10.0,
        );
        let outside = runner.validate(
            &fixture("valid_simple.alda"),
            &[],
            &[],
            Some(DurationConstraint::range(actual * 2.0, actual * 3.0)),
            10.0,
        );
        assert_eq!(
            inside
                .iter()
                .find(|check| check.name == "时长")
                .unwrap()
                .status,
            CheckStatus::Pass
        );
        assert_eq!(
            outside
                .iter()
                .find(|check| check.name == "时长")
                .unwrap()
                .status,
            CheckStatus::Fail
        );
    }

    #[test]
    fn test_validate_instrument_excluded() {
        let (_directory, runner) = runner();
        let checks = runner.validate(
            &fixture("valid_simple.alda"),
            &[],
            &["piano".to_string()],
            None,
            10.0,
        );
        // piano 子串匹配 stock-instrument "midi-acoustic-grand-piano"
        let excluded = checks.iter().find(|c| c.name == "排除乐器").unwrap();
        assert_eq!(excluded.status, CheckStatus::Fail);
    }

    #[test]
    fn test_validate_instrument_included() {
        let (_directory, runner) = runner();
        let checks = runner.validate(
            &fixture("valid_simple.alda"),
            &["piano".to_string()],
            &[],
            None,
            10.0,
        );
        let included = checks.iter().find(|c| c.name == "包含乐器").unwrap();
        assert_eq!(included.status, CheckStatus::Pass);
    }

    #[test]
    fn test_export_midi() {
        let (_directory, runner) = runner();
        let tmp = tempfile::tempdir().unwrap();
        let output = tmp.path().join("out.mid");
        let result = runner.export_midi(&fixture("valid_simple.alda"), &output);
        assert!(result.is_ok());
        assert!(output.exists());
    }

    #[test]
    fn test_list_instruments() {
        let (_directory, runner) = runner();
        let instruments = runner.list_instruments().unwrap();
        assert!(instruments.len() >= 128, "应有至少 128 种 GM 乐器");
    }

    #[test]
    fn timeout_terminates_child() {
        let (_directory, runner) = runner();
        let runner = runner.with_limits(Duration::from_millis(50), 1024);
        let error = runner.parse(&fixture("slow.alda")).unwrap_err();
        assert!(error.to_string().contains("超时"));
    }

    #[tokio::test]
    async fn cancelled_async_validation_returns_an_error() {
        let (_directory, runner) = runner();
        let cancellation = CancellationToken::default();
        let runner = runner.with_cancellation(cancellation.clone());
        let task = tokio::spawn(async move {
            runner
                .validate_async(
                    fixture("slow.alda"),
                    ScoreValidation::new(None, Vec::new(), Vec::new()),
                )
                .await
        });
        std::thread::sleep(Duration::from_millis(20));
        cancellation.cancel();
        let error = task.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("取消"));
    }

    #[test]
    fn output_limit_covers_stdout() {
        let (_directory, runner) = runner();
        let runner = runner.with_limits(Duration::from_secs(1), 16);
        let error = runner.list_instruments().unwrap_err();
        assert!(error.to_string().contains("输出超过上限"));
    }
}
