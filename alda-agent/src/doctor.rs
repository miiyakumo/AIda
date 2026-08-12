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

fn run_checks(exec: impl Fn(&str, &[&str]) -> Option<String>) -> Vec<CheckResult> {
    let java = check_java(&exec);
    let alda = check_alda(&exec);
    let rust = check_rust(&exec);
    vec![java, alda, rust]
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

pub fn run() -> anyhow::Result<()> {
    let exec = |program: &str, args: &[&str]| -> Option<String> {
        let output = std::process::Command::new(program)
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .ok()?;
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            Some(format!("{stdout}{stderr}"))
        } else {
            None
        }
    };

    let mut results = vec![check_config()];
    results.extend(run_checks(exec));
    print_results(&results);
    let failed = results
        .iter()
        .filter(|result| matches!(result.status, CheckStatus::Fail))
        .count();
    if failed > 0 {
        anyhow::bail!("环境检查有 {failed} 项失败");
    }
    Ok(())
}

fn check_config() -> CheckResult {
    match crate::config::Config::from_env_file() {
        Ok(config) => CheckResult {
            name: "模型配置",
            status: CheckStatus::Pass,
            detail: format!(
                "端点={}，模型={}，thinking={}",
                config.base_url, config.model, config.thinking
            ),
            suggestion: None,
        },
        Err(error) => CheckResult {
            name: "模型配置",
            status: CheckStatus::Fail,
            detail: error.to_string(),
            suggestion: Some("复制 .env.example 为 .env，并填写规范配置".to_string()),
        },
    }
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
        let exec = |program: &str, _args: &[&str]| -> Option<String> {
            match program {
                "java" => Some("openjdk version \"21.0.4\" 2024-07-16 LTS\n".to_string()),
                "which" => Some("/usr/local/bin/alda\n".to_string()),
                "rustc" => Some("rustc 1.85.0\n".to_string()),
                _ => None,
            }
        };
        let results = run_checks(exec);
        assert_eq!(results.len(), 3);
        assert!(matches!(results[0].status, CheckStatus::Pass));
        assert!(matches!(results[1].status, CheckStatus::Pass));
        assert!(matches!(results[2].status, CheckStatus::Pass));
        assert!(results[0].detail.contains("openjdk"));
        assert_eq!(results[1].detail.trim(), "/usr/local/bin/alda");
    }

    #[test]
    fn test_run_checks_all_fail() {
        let exec = |_program: &str, _args: &[&str]| -> Option<String> { None };
        let results = run_checks(exec);
        assert_eq!(results.len(), 3);
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
        let results = run_checks(exec);
        assert_eq!(results.len(), 3);
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
