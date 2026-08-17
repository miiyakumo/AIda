use alda_agent::alda::{AldaCheck, CheckStatus};
use alda_agent::config::ModelConfig;
use alda_agent::instructions::{CreationMode, ProjectPreferences};
use alda_agent::project::{Project, WorkingScoreKind};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;

fn tool_response(kind: &str, code: Option<&str>) -> String {
    let mut arguments = serde_json::json!({
        "kind": kind,
        "message": format!("{kind} result")
    });
    if kind == "plan" {
        arguments["plan"] = serde_json::json!({
            "core_material": "theme",
            "form": "A-B-A",
            "orchestration": "piano",
            "development": "variation"
        });
    }
    if let Some(code) = code {
        arguments["alda_code"] = serde_json::Value::String(code.to_string());
    }
    let chunk = serde_json::json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "function": {
                        "name": "submit_result",
                        "arguments": arguments.to_string()
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }]
    });
    format!("data: {chunk}\n\ndata: [DONE]\n")
}

fn response_server(body: String, requests: usize) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    thread::spawn(move || {
        for _ in 0..requests {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            let header_end = loop {
                let read = stream.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..read]);
                if let Some(position) = request.windows(4).position(|item| item == b"\r\n\r\n") {
                    break position + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            while request.len() < header_end + content_length {
                let read = stream.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..read]);
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        }
    });
    format!("http://{address}")
}

struct ComposeRun {
    _directory: tempfile::TempDir,
    output: Output,
    output_file: PathBuf,
    project: PathBuf,
    project_metadata_before: Option<Vec<u8>>,
}

fn run_compose(kind: &str, valid_score: bool) -> ComposeRun {
    run_compose_with(kind, valid_score, |_| {}, &[])
}

fn run_compose_with(
    kind: &str,
    valid_score: bool,
    configure_project: impl FnOnce(&Path),
    extra_args: &[&str],
) -> ComposeRun {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("project");
    let output_directory = directory.path().join("output");
    let bin_directory = directory.path().join("bin");
    let home = directory.path().join("home");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&output_directory).unwrap();
    fs::create_dir_all(&bin_directory).unwrap();
    fs::create_dir_all(&home).unwrap();
    configure_project(&project);
    let project_metadata_before = fs::read(project.join("project.json")).ok();
    let source = directory.path().join("source.txt");
    fs::write(&source, "a short musical idea").unwrap();

    let code = matches!(kind, "draft" | "candidate").then_some("piano: c");
    let requests = if kind == "candidate" && !valid_score {
        3
    } else {
        1
    };
    let base_url = response_server(tool_response(kind, code), requests);
    let mut config = ModelConfig::default();
    config.set_model("test-model").unwrap();
    config.set_base_url(&base_url).unwrap();
    config.set_api_key("test-key").unwrap();
    config.save(&project).unwrap();

    let alda = bin_directory.join("alda");
    let parse_output = if valid_score {
        r#"{"events":[{"offset":0,"duration":1000,"audible-duration":1000,"midi-note":60,"part":"piano"}],"parts":{"piano":{"name":"piano","stock-instrument":"midi-acoustic-grand-piano","tempo":120}}}"#
    } else {
        r#"{"events":[],"parts":{}}"#
    };
    fs::write(
        &alda,
        format!("#!/bin/sh\nprintf '%s\\n' '{parse_output}'\n"),
    )
    .unwrap();
    let mut permissions = fs::metadata(&alda).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&alda, permissions).unwrap();

    let path = std::env::join_paths(std::iter::once(bin_directory).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_alda-agent"));
    command
        .arg("--project")
        .arg(&project)
        .arg("compose")
        .arg("--file")
        .arg(source)
        .arg("--output")
        .arg(&output_directory)
        .args(extra_args)
        .env("PATH", path)
        .env("HOME", home);
    let output = command.output().unwrap();

    ComposeRun {
        _directory: directory,
        output,
        output_file: output_directory.join("current.alda"),
        project,
        project_metadata_before,
    }
}

#[test]
fn compose_candidate_preserves_stdout_exit_and_output_contract() {
    let run = run_compose("candidate", true);
    assert!(run.output.status.success());
    let stdout = String::from_utf8(run.output.stdout).unwrap();
    assert!(stdout.contains("=== 开始创作 ==="));
    assert!(stdout.contains("=== 创作完成 (1/3 轮) ==="));
    assert!(stdout.contains("状态: ✅ 成功"));
    assert!(stdout.contains("校验结果:"));
    assert!(stdout.contains("作品已保存到:"));
    assert!(run.output.stderr.is_empty());
    assert_eq!(fs::read_to_string(run.output_file).unwrap(), "piano: c");
}

#[test]
fn compose_non_candidate_results_preserve_failure_contracts() {
    for (kind, expected) in [
        ("plan", "模型返回了文字结果；请进入交互模式继续创作"),
        (
            "clarification",
            "模型需要补充信息；请进入交互模式回答澄清问题",
        ),
        ("draft", "模型返回了草稿；请进入交互模式试听和继续发展"),
    ] {
        let run = run_compose(kind, true);
        assert!(!run.output.status.success(), "{kind} should fail");
        assert!(
            String::from_utf8(run.output.stderr)
                .unwrap()
                .contains(expected),
            "unexpected {kind} stderr"
        );
        assert!(!run.output_file.exists());
    }

    let run = run_compose("candidate", false);
    assert!(!run.output.status.success());
    assert!(
        String::from_utf8(run.output.stderr)
            .unwrap()
            .contains("作品修正仍未通过")
    );
    assert!(!run.output_file.exists());
}

#[test]
fn compose_uses_cli_preferences_without_mutating_existing_project_state() {
    let checks = [AldaCheck {
        name: "Alda 语法",
        status: CheckStatus::Pass,
        detail: "解析成功".to_string(),
    }];
    let run = run_compose_with(
        "candidate",
        true,
        |root| {
            let mut project = Project::load_or_create(root.to_path_buf(), "project", "").unwrap();
            project
                .configure(&ProjectPreferences {
                    mode: CreationMode::Improv,
                    target_duration_secs: Some(
                        alda_agent::instructions::DurationConstraint::exact(99.0),
                    ),
                    included_instruments: vec!["midi-tuba".to_string()],
                    excluded_instruments: vec!["midi-violin".to_string()],
                })
                .unwrap();
            project.add_user_message("existing conversation").unwrap();
            project
                .save_version("piano: old", "existing version", &checks)
                .unwrap();
            project
                .save_working_score(
                    "piano: work",
                    WorkingScoreKind::Draft,
                    "existing draft",
                    &checks,
                )
                .unwrap();
        },
        &["--mode", "full", "--duration", "1"],
    );

    assert!(run.output.status.success());
    assert_eq!(
        fs::read(run.project.join("project.json")).unwrap(),
        run.project_metadata_before.unwrap()
    );
    let project = Project::load_or_create(run.project, "ignored", "").unwrap();
    assert_eq!(
        project.preferences(),
        &ProjectPreferences {
            mode: CreationMode::Improv,
            target_duration_secs: Some(alda_agent::instructions::DurationConstraint::exact(99.0),),
            included_instruments: vec!["midi-tuba".to_string()],
            excluded_instruments: vec!["midi-violin".to_string()],
        }
    );
    assert_eq!(project.conversation().messages().len(), 1);
    assert_eq!(
        project.conversation().first_request(),
        Some("existing conversation")
    );
    assert_eq!(project.current_version(), 1);
    assert_eq!(project.versions().len(), 1);
    assert_eq!(project.version_code(1).unwrap(), "piano: old");
    assert_eq!(
        project.working_score().unwrap().kind,
        WorkingScoreKind::Draft
    );
    assert_eq!(project.working_code().unwrap(), "piano: work");
}

#[test]
fn compose_preflight_failure_has_no_start_banner_or_project_side_effects() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("missing-project");
    let source = directory.path().join("source.txt");
    let output_directory = directory.path().join("output");
    let home = directory.path().join("home");
    fs::write(&source, "material").unwrap();
    fs::create_dir(&output_directory).unwrap();
    fs::create_dir(&home).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_alda-agent"))
        .arg("--project")
        .arg(&project)
        .arg("compose")
        .arg("--file")
        .arg(source)
        .arg("--output")
        .arg(output_directory)
        .env("HOME", home)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        !String::from_utf8(output.stdout)
            .unwrap()
            .contains("=== 开始创作 ===")
    );
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("项目模型配置不完整")
    );
    assert!(!project.exists());
}
