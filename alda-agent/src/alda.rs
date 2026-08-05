use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

// ============================================================
// Alda parse JSON 数据结构
// ============================================================

#[derive(Debug, Deserialize)]
struct ParseOutput {
    events: Vec<Event>,
    parts: std::collections::HashMap<String, Part>,
}

#[derive(Debug, Deserialize)]
struct Event {
    offset: f64,
    #[allow(dead_code)]
    duration: f64,
    #[serde(rename = "audible-duration")]
    audible_duration: f64,
    #[serde(rename = "midi-note")]
    #[allow(dead_code)]
    midi_note: u8,
    #[allow(dead_code)]
    part: String,
}

#[derive(Debug, Deserialize)]
struct Part {
    #[allow(dead_code)]
    name: String,
    #[serde(rename = "stock-instrument")]
    stock_instrument: String,
    tempo: f64,
}

// ============================================================
// 结构化结果
// ============================================================

#[derive(Debug, Clone)]
pub struct ScoreInfo {
    /// 估算总时长（毫秒）
    pub duration_ms: f64,
    /// 声部数量
    pub part_count: usize,
    /// 当前使用的乐器列表（stock-instrument 全名）
    pub instruments: Vec<String>,
    /// tempo
    pub tempo: f64,
}

/// 检查项结果
#[derive(Debug, Clone)]
pub struct AldaCheck {
    pub name: &'static str,
    pub status: CheckStatus,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CheckStatus {
    Pass,
    Fail,
    Unchecked,
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

// ============================================================
// Alda 命令执行器
// ============================================================

pub struct AldaRunner {
    alda_path: PathBuf,
    #[allow(dead_code)]
    timeout: Duration,
    max_output_bytes: usize,
}

impl AldaRunner {
    pub fn new(alda_path: PathBuf) -> Self {
        AldaRunner {
            alda_path,
            timeout: Duration::from_secs(60),
            max_output_bytes: 10 * 1024 * 1024, // 10MB
        }
    }

    /// 解析 Alda 文件，返回结构化信息
    pub fn parse(&self, score_path: &Path) -> Result<ScoreInfo> {
        let output = self.run_alda(&["parse", "-f", &score_path.to_string_lossy()])?;

        let parsed: ParseOutput =
            serde_json::from_str(&output).context("无法解析 alda parse 输出为 JSON")?;

        // 计算时长：max(offset + audible_duration)
        let duration_ms = parsed
            .events
            .iter()
            .map(|e| e.offset + e.audible_duration)
            .fold(0.0_f64, f64::max);

        let part_count = parsed.parts.len();

        let instruments: Vec<String> = parsed
            .parts
            .values()
            .map(|p| p.stock_instrument.clone())
            .collect();

        // tempo：取第一个 part 的 tempo 值
        let tempo = parsed.parts.values().next().map(|p| p.tempo).unwrap_or(0.0);

        Ok(ScoreInfo {
            duration_ms,
            part_count,
            instruments,
            tempo,
        })
    }

    /// 语法检查，返回原始 JSON 字符串或错误
    pub fn parse_raw(&self, score_path: &Path) -> Result<String> {
        self.run_alda(&["parse", "-f", &score_path.to_string_lossy()])
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
    pub fn validate(
        &self,
        score_path: &Path,
        included_instruments: &[String],
        excluded_instruments: &[String],
        target_duration_ms: Option<f64>,
        duration_tolerance_pct: f64,
    ) -> Vec<AldaCheck> {
        let mut checks = Vec::new();

        let info = match self.parse(score_path) {
            Ok(info) => info,
            Err(e) => {
                checks.push(AldaCheck {
                    name: "Alda 语法",
                    status: CheckStatus::Fail,
                    detail: format!("{}", e),
                });
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

        // 2. 时长检查
        if let Some(target_ms) = target_duration_ms {
            let actual_ms = info.duration_ms;
            let actual_seconds = actual_ms / 1000.0;
            let target_seconds = target_ms / 1000.0;
            let deviation = ((actual_ms - target_ms).abs() / target_ms) * 100.0;

            if deviation <= duration_tolerance_pct {
                checks.push(AldaCheck {
                    name: "时长",
                    status: CheckStatus::Pass,
                    detail: format!(
                        "约 {:.0}秒（目标 {:.0}秒，偏差 {:.0}%）",
                        actual_seconds, target_seconds, deviation
                    ),
                });
            } else {
                checks.push(AldaCheck {
                    name: "时长",
                    status: CheckStatus::Fail,
                    detail: format!(
                        "约 {:.0}秒（目标 {:.0}秒，偏差 {:.0}%，超出容差 {:.0}%）",
                        actual_seconds, target_seconds, deviation, duration_tolerance_pct
                    ),
                });
            }
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
                    detail: format!("\"{}\" 已在乐谱中", required),
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
                    detail: format!("\"{}\" 未出现在乐谱中", excluded),
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

    // ============================================================
    // 内部命令执行
    // ============================================================

    /// 运行 alda 命令，捕获 stdout（合并 stderr），返回字符串
    fn run_alda(&self, args: &[&str]) -> Result<String> {
        let output = Command::new(&self.alda_path)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .context("无法执行 alda")?;

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

        if !output.status.success() {
            bail!("Alda 命令失败: {}", stderr.trim());
        }

        // 检查输出大小
        if stdout.len() > self.max_output_bytes {
            bail!(
                "Alda 输出超过上限 ({} MB)",
                self.max_output_bytes / 1024 / 1024
            );
        }

        Ok(stdout)
    }

    /// 运行 alda 命令，不捕获输出（用于 play/stop）
    fn run_alda_no_capture(&self, args: &[&str]) -> Result<()> {
        let status = Command::new(&self.alda_path)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("无法执行 alda")?;

        if !status.success() {
            bail!("Alda 命令失败");
        }
        Ok(())
    }
}

// ============================================================
// 查找 alda 可执行文件
// ============================================================

/// 在 PATH 和常见目录中查找 alda 可执行文件
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

    fn runner() -> Option<AldaRunner> {
        find_alda().map(AldaRunner::new)
    }

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    #[test]
    fn test_parse_valid_simple() {
        let runner = runner().expect("alda 未安装，跳过测试");
        let info = runner.parse(&fixture("valid_simple.alda")).unwrap();
        assert_eq!(info.part_count, 1);
        assert!(info.duration_ms > 0.0);
        assert!(info.instruments.len() == 1);
    }

    #[test]
    fn test_parse_valid_multi_part() {
        let runner = runner().expect("alda 未安装，跳过测试");
        let info = runner.parse(&fixture("valid_multi_part.alda")).unwrap();
        assert_eq!(info.part_count, 2);
        assert!(info.instruments.len() == 2);
    }

    #[test]
    fn test_parse_invalid_syntax() {
        let runner = runner().expect("alda 未安装，跳过测试");
        let result = runner.parse(&fixture("invalid_syntax.alda"));
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_empty() {
        let runner = runner().expect("alda 未安装，跳过测试");
        let result = runner.parse(&fixture("empty.alda"));
        // 空文件可能解析成功但 events 为空
        if let Ok(info) = result {
            assert_eq!(info.duration_ms, 0.0);
            assert_eq!(info.part_count, 0);
        }
    }

    #[test]
    fn test_validate_duration_pass() {
        let runner = runner().expect("alda 未安装，跳过测试");
        let info = runner.parse(&fixture("valid_simple.alda")).unwrap();
        let target = info.duration_ms; // 精确目标
        let checks = runner.validate(
            &fixture("valid_simple.alda"),
            &[],
            &[],
            Some(target),
            10.0,
        );
        let duration_check = checks.iter().find(|c| c.name == "时长").unwrap();
        assert_eq!(duration_check.status, CheckStatus::Pass);
    }

    #[test]
    fn test_validate_instrument_excluded() {
        let runner = runner().expect("alda 未安装，跳过测试");
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
        let runner = runner().expect("alda 未安装，跳过测试");
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
    #[ignore] // 需要本机有播放设备
    fn test_export_midi() {
        let runner = runner().expect("alda 未安装，跳过测试");
        let tmp = tempfile::tempdir().unwrap();
        let output = tmp.path().join("out.mid");
        let result = runner.export_midi(&fixture("valid_simple.alda"), &output);
        assert!(result.is_ok());
        assert!(output.exists());
    }

    #[test]
    fn test_find_alda() {
        let result = find_alda();
        assert!(result.is_some(), "alda 应该已在 PATH 中");
    }

    #[test]
    fn test_list_instruments() {
        let runner = runner().expect("alda 未安装，跳过测试");
        let instruments = runner.list_instruments().unwrap();
        assert!(instruments.len() >= 128, "应有至少 128 种 GM 乐器");
    }
}
