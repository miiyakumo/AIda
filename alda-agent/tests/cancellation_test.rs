use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn operation_token_terminates_an_active_alda_process_group() {
    use alda_agent::alda::{AldaRunner, CancellationToken};
    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join("alda");
    let pid_file = directory.path().join("alda.pid");
    fs::write(
        &executable,
        format!("#!/bin/sh\necho $$ > '{}'\nsleep 5\n", pid_file.display()),
    )
    .unwrap();
    let mut permissions = fs::metadata(&executable).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&executable, permissions).unwrap();

    let token = CancellationToken::default();
    let runner = AldaRunner::new(executable).with_cancellation(token.clone());
    let score = directory.path().join("score.alda");
    fs::write(&score, "piano: c").unwrap();
    let handle = thread::spawn(move || runner.parse(&score));

    let start = Instant::now();
    while !pid_file.exists() && start.elapsed() < Duration::from_secs(1) {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(pid_file.exists(), "fake Alda process did not start");
    token.cancel();
    assert!(
        handle
            .join()
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("取消")
    );

    let alda_pid = fs::read_to_string(pid_file).unwrap();
    let still_running = Command::new("kill")
        .args(["-0", alda_pid.trim()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    assert!(!still_running, "Alda child remained after Ctrl+C");
}
