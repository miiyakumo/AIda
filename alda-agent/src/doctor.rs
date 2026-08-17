struct CheckResult {
    name: &'static str,
    status: CheckStatus,
    detail: String,
    suggestion: Option<String>,
}

enum CheckStatus {
    Pass,
    Fail,
}

fn run_checks(
    exec: impl Fn(&str, &[&str]) -> Option<String>,
    soundfont: impl Fn() -> Option<std::path::PathBuf>,
) -> Vec<CheckResult> {
    let mut results = run_runtime_checks(&exec, &soundfont);
    results.push(check_rust(&exec));
    results
}

fn run_runtime_checks(
    exec: &impl Fn(&str, &[&str]) -> Option<String>,
    soundfont: &impl Fn() -> Option<std::path::PathBuf>,
) -> Vec<CheckResult> {
    let java = check_java(&exec);
    let alda = check_alda(&exec);
    let fluidsynth = check_fluidsynth(&exec);
    let soundfont = check_soundfont(&soundfont);
    vec![java, alda, fluidsynth, soundfont]
}

fn check_fluidsynth(exec: &impl Fn(&str, &[&str]) -> Option<String>) -> CheckResult {
    exec("which", &["fluidsynth"]).map_or_else(
        || CheckResult {
            name: "FluidSynth",
            status: CheckStatus::Fail,
            detail: "未找到 fluidsynth".to_string(),
            suggestion: Some("运行 scripts/install-linux.sh 安装 FluidSynth".to_string()),
        },
        |output| CheckResult {
            name: "FluidSynth",
            status: CheckStatus::Pass,
            detail: output.trim().to_string(),
            suggestion: None,
        },
    )
}

fn check_soundfont(find: &impl Fn() -> Option<std::path::PathBuf>) -> CheckResult {
    find().map_or_else(
        || CheckResult {
            name: "GM SoundFont",
            status: CheckStatus::Fail,
            detail: "未找到 General MIDI SoundFont".to_string(),
            suggestion: Some("安装 fluid-soundfont-gm，或设置 ALDA_AGENT_SOUNDFONT".to_string()),
        },
        |path| CheckResult {
            name: "GM SoundFont",
            status: CheckStatus::Pass,
            detail: path.display().to_string(),
            suggestion: None,
        },
    )
}

fn check_rust(exec: &impl Fn(&str, &[&str]) -> Option<String>) -> CheckResult {
    exec("rustc", &["--version"]).map_or_else(
        || CheckResult {
            name: "Rust 工具链",
            status: CheckStatus::Fail,
            detail: "未找到 rustc".to_string(),
            suggestion: Some("从 https://rustup.rs 安装 Rust 1.85+".to_string()),
        },
        |output| CheckResult {
            name: "Rust 工具链",
            status: CheckStatus::Pass,
            detail: output.trim().to_string(),
            suggestion: None,
        },
    )
}

fn check_java(exec: &impl Fn(&str, &[&str]) -> Option<String>) -> CheckResult {
    match exec("java", &["-version"]) {
        Some(output) => {
            let first_line = output.lines().next().unwrap_or("");
            let parts: Vec<&str> = first_line.split_whitespace().collect();
            let detail = if parts.len() >= 3 {
                let vendor = parts[0];
                let version = parts[2].trim_matches('"');
                format!("{vendor} {version}")
            } else {
                first_line.to_string()
            };
            CheckResult {
                name: "Java 运行环境",
                status: CheckStatus::Pass,
                detail,
                suggestion: None,
            }
        }
        None => CheckResult {
            name: "Java 运行环境",
            status: CheckStatus::Fail,
            detail: "未找到 java".to_string(),
            suggestion: Some("运行 scripts/install-linux.sh 或安装 OpenJDK 21+".to_string()),
        },
    }
}

fn check_alda(exec: &impl Fn(&str, &[&str]) -> Option<String>) -> CheckResult {
    if let Some(path) = exec("which", &["alda"]) {
        let path = path.trim().to_string();
        if !path.is_empty() {
            return CheckResult {
                name: "Alda",
                status: CheckStatus::Pass,
                detail: path,
                suggestion: None,
            };
        }
    }

    for dir in [
        "/usr/local/bin",
        "/usr/bin",
        "/opt/homebrew/bin",
        "/home/linuxbrew/.linuxbrew/bin",
    ] {
        let full = format!("{dir}/alda");
        if std::path::Path::new(&full).exists() {
            return CheckResult {
                name: "Alda",
                status: CheckStatus::Pass,
                detail: full,
                suggestion: None,
            };
        }
    }

    CheckResult {
        name: "Alda",
        status: CheckStatus::Fail,
        detail: "未找到 alda".to_string(),
        suggestion: Some(
            "运行 scripts/install-linux.sh 或访问 https://alda.io/install".to_string(),
        ),
    }
}

pub async fn run(
    probe: Option<crate::ProbeTarget>,
    project_root: std::path::PathBuf,
) -> anyhow::Result<()> {
    let results = run_checks(exec_local, crate::audio::find_soundfont);
    print_results(&results);
    let failed = results
        .iter()
        .filter(|result| matches!(result.status, CheckStatus::Fail))
        .count();
    if failed > 0 {
        anyhow::bail!("环境检查有 {failed} 项失败");
    }
    if let Some(probe) = probe {
        run_probe(probe, &project_root).await?;
    }
    Ok(())
}

pub fn require_runtime() -> anyhow::Result<()> {
    let results = run_runtime_checks(&exec_local, &crate::audio::find_soundfont);
    let missing = results
        .iter()
        .filter(|result| matches!(result.status, CheckStatus::Fail))
        .map(|result| result.name)
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "运行环境不完整，未启动 Alda Agent：{}。请先运行 scripts/install-linux.sh 安装依赖，再用 `alda-agent doctor` 验证",
        missing.join("、")
    )
}

fn exec_local(program: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    Some(format!("{stdout}{stderr}"))
}

async fn run_probe(
    probe: crate::ProbeTarget,
    project_root: &std::path::Path,
) -> anyhow::Result<()> {
    if matches!(probe, crate::ProbeTarget::Model | crate::ProbeTarget::All) {
        probe_model(project_root).await?;
    }
    if matches!(probe, crate::ProbeTarget::Alda | crate::ProbeTarget::All) {
        probe_alda().await?;
    }
    Ok(())
}

async fn probe_model(project_root: &std::path::Path) -> anyhow::Result<()> {
    use crate::deepseek::{DeepSeekClient, Message, StreamEvent};
    let config = crate::config::ModelConfig::load(project_root)?.resolve()?;
    let client = DeepSeekClient::new(config.api_key, config.base_url, config.model)?;
    let events = client
        .chat_stream(
            vec![Message {
                role: "user".to_string(),
                content: Some("只回复 OK。".to_string()),
                tool_calls: None,
                tool_call_id: None,
            }],
            None,
        )
        .await?;
    if !events
        .iter()
        .any(|event| matches!(event, StreamEvent::Text(_)))
    {
        anyhow::bail!("模型探测未返回文本");
    }
    println!("✓ 模型真实连通探测通过");
    Ok(())
}

async fn probe_alda() -> anyhow::Result<()> {
    use crate::alda::{AldaRunner, find_alda};
    use crate::audio::AudioRenderer;
    let path = find_alda().ok_or_else(|| anyhow::anyhow!("未找到 alda"))?;
    let score_dir = tempfile::tempdir()?;
    let score = score_dir.path().join("probe.alda");
    std::fs::write(&score, "piano: c")?;
    let runner = AldaRunner::new(path);
    let checks = runner
        .clone()
        .validate_async(
            score.clone(),
            crate::alda::ScoreValidation::new(None, Vec::new(), Vec::new()),
        )
        .await?;
    if checks
        .iter()
        .any(|check| check.status == crate::alda::CheckStatus::Fail)
    {
        anyhow::bail!("Alda 真实探测未通过校验");
    }
    let report = AudioRenderer::discover()?
        .render_score_async(
            runner,
            score,
            score_dir.path().join("probe.mid"),
            score_dir.path().join("probe.wav"),
        )
        .await?;
    println!(
        "✓ Alda→MIDI→WAV 真实探测通过（解析 {:.2} 秒，音频 {:.2} 秒，peak {:.4}）",
        report.parsed_duration_secs, report.wav.duration_secs, report.wav.peak
    );
    Ok(())
}

fn print_results(results: &[CheckResult]) {
    let total = results.len();
    let passed = results
        .iter()
        .filter(|r| matches!(r.status, CheckStatus::Pass))
        .count();
    let failed = total - passed;

    for r in results {
        let icon = match r.status {
            CheckStatus::Pass => "✅",
            CheckStatus::Fail => "❌",
        };
        println!("  {:<16}{}  {}", r.name, icon, r.detail);
        if let Some(s) = &r.suggestion {
            println!("                      → {s}");
        }
    }

    println!();
    if failed == 0 {
        println!("环境状态：{passed}/{total} 通过");
    } else {
        println!("环境状态：{passed}/{total} 通过，{failed} 项失败");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run_checks_all_pass() {
        let exec = |program: &str, args: &[&str]| -> Option<String> {
            match program {
                "java" => Some("openjdk version \"21.0.4\" 2024-07-16 LTS\n".to_string()),
                "which" => Some(format!("/usr/local/bin/{}\n", args[0])),
                "rustc" => Some("rustc 1.85.0\n".to_string()),
                _ => None,
            }
        };
        let results = run_checks(exec, || Some("/tmp/test.sf2".into()));
        assert_eq!(results.len(), 5);
        assert!(matches!(results[0].status, CheckStatus::Pass));
        assert!(matches!(results[1].status, CheckStatus::Pass));
        assert!(matches!(results[2].status, CheckStatus::Pass));
        assert!(results[0].detail.contains("openjdk"));
        assert_eq!(results[1].detail.trim(), "/usr/local/bin/alda");
    }

    #[test]
    fn test_run_checks_all_fail() {
        let exec = |_program: &str, _args: &[&str]| -> Option<String> { None };
        let results = run_checks(exec, || None);
        assert_eq!(results.len(), 5);
        assert!(matches!(results[0].status, CheckStatus::Fail));
        assert!(matches!(results[1].status, CheckStatus::Fail));
        assert!(matches!(results[2].status, CheckStatus::Fail));
        assert!(results[0].suggestion.is_some());
        assert!(results[1].suggestion.is_some());
    }

    #[test]
    fn test_run_checks_one_pass_one_fail() {
        let exec = |program: &str, _args: &[&str]| -> Option<String> {
            match program {
                "java" => Some("openjdk version \"21.0.4\" 2024-07-16 LTS\n".to_string()),
                _ => None,
            }
        };
        let results = run_checks(exec, || None);
        assert_eq!(results.len(), 5);
        assert!(matches!(results[0].status, CheckStatus::Pass));
        assert!(matches!(results[1].status, CheckStatus::Fail));
        assert!(results[1].suggestion.is_some());
        assert!(matches!(results[2].status, CheckStatus::Fail));
    }

    #[test]
    fn test_print_results_output() {
        let results = vec![
            CheckResult {
                name: "Java 运行环境",
                status: CheckStatus::Pass,
                detail: "openjdk 21.0.4".to_string(),
                suggestion: None,
            },
            CheckResult {
                name: "Alda",
                status: CheckStatus::Pass,
                detail: "/usr/local/bin/alda".to_string(),
                suggestion: None,
            },
        ];
        // 调用 print_results 确保不会 panic
        print_results(&results);
    }
}
