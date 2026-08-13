use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
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
    events: Vec<Event>,
    parts: std::collections::HashMap<String, Part>,
}

#[derive(Debug, Deserialize)]
struct Event {
    offset: f64,
    #[serde(rename = "audible-duration")]
    audible_duration: f64,
}

#[derive(Debug, Deserialize)]
struct Part {
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
    /// 实际发声事件数量
    pub event_count: usize,
    /// 当前使用的乐器列表（stock-instrument 全名）
    pub instruments: Vec<String>,
    /// tempo
    pub tempo: f64,
}

/// 检查项结果
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AldaCheck {
    pub name: &'static str,
    pub status: CheckStatus,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
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
            timeout: Duration::from_secs(60),
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

        // 计算时长：max(offset + audible_duration)
        let duration_ms = parsed
            .events
            .iter()
            .map(|e| e.offset + e.audible_duration)
            .fold(0.0_f64, f64::max);

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
                    detail: format!("{e}"),
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

        // 2. 时长检查
        if let Some(target_ms) = target_duration_ms {
            if !target_ms.is_finite() || target_ms <= 0.0 {
                checks.push(AldaCheck {
                    name: "时长",
                    status: CheckStatus::Fail,
                    detail: "目标时长必须是大于 0 的有限数值".to_string(),
                });
                return checks;
            }
            let actual_ms = info.duration_ms;
            let actual_seconds = actual_ms / 1000.0;
            let target_seconds = target_ms / 1000.0;
            let deviation = ((actual_ms - target_ms).abs() / target_ms) * 100.0;

            if deviation <= duration_tolerance_pct {
                checks.push(AldaCheck {
                    name: "时长",
                    status: CheckStatus::Pass,
                    detail: format!(
                        "约 {actual_seconds:.0}秒（目标 {target_seconds:.0}秒，偏差 {deviation:.0}%）"
                    ),
                });
            } else {
                checks.push(AldaCheck {
                    name: "时长",
                    status: CheckStatus::Fail,
                    detail: format!(
                        "约 {actual_seconds:.0}秒（目标 {target_seconds:.0}秒，偏差 {deviation:.0}%，超出容差 {duration_tolerance_pct:.0}%）"
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
        included_instruments: Vec<String>,
        excluded_instruments: Vec<String>,
        target_duration_ms: Option<f64>,
        duration_tolerance_pct: f64,
    ) -> Result<Vec<AldaCheck>> {
        let runner = self.clone();
        let checks = tokio::task::spawn_blocking(move || {
            runner.validate(
                &score_path,
                &included_instruments,
                &excluded_instruments,
                target_duration_ms,
                duration_tolerance_pct,
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
        let target = info.duration_ms; // 精确目标
        let checks = runner.validate(&fixture("valid_simple.alda"), &[], &[], Some(target), 10.0);
        let duration_check = checks.iter().find(|c| c.name == "时长").unwrap();
        assert_eq!(duration_check.status, CheckStatus::Pass);
    }

    #[test]
    fn invalid_duration_target_fails_without_division() {
        let (_directory, runner) = runner();
        let checks = runner.validate(&fixture("valid_simple.alda"), &[], &[], Some(0.0), 10.0);
        let duration = checks.iter().find(|check| check.name == "时长").unwrap();
        assert_eq!(duration.status, CheckStatus::Fail);
        assert!(duration.detail.contains("大于 0"));
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
                .validate_async(fixture("slow.alda"), Vec::new(), Vec::new(), None, 10.0)
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
