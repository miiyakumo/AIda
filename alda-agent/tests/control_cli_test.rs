use serde_json::Value;
use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn control_cli_uses_jsonl_and_keeps_running_after_request_errors() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("machine-control");
    let mut child = Command::new(env!("CARGO_BIN_EXE_alda-agent"))
        .args(["--project", project.to_str().unwrap(), "control"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(
        stdin,
        r#"{{"id":"bad","action":{{"type":"config_api_key","key":"must-not-appear"}}}}"#
    )
    .unwrap();
    writeln!(
        stdin,
        r#"{{"id":"config","action":{{"type":"config_duration","seconds":120.0}}}}"#
    )
    .unwrap();
    writeln!(
        stdin,
        r#"{{"id":"view","action":{{"type":"project_overview"}}}}"#
    )
    .unwrap();
    drop(stdin);

    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!stdout.contains("must-not-appear"));
    let responses = stdout
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(responses.len(), 3);
    assert_eq!(responses[0]["id"], "bad");
    assert_eq!(responses[0]["error"]["kind"], "invalid_request");
    assert_eq!(responses[1]["project"]["target_duration_secs"], 120.0);
    assert_eq!(responses[2]["id"], "view");
    assert_eq!(responses[2]["type"], "result");
}
