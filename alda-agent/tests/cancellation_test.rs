use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn ctrl_c_terminates_an_active_alda_process_group() {
    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join("alda");
    let pid_file = directory.path().join("alda.pid");
    fs::write(
        &executable,
        "#!/bin/sh\necho $$ > \"$FAKE_ALDA_PID_FILE\"\nsleep 5\n",
    )
    .unwrap();
    let mut permissions = fs::metadata(&executable).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&executable, permissions).unwrap();

    let path = format!("{}:/usr/bin:/bin", directory.path().display());
    let mut child = Command::new(env!("CARGO_BIN_EXE_alda-agent"))
        .arg("alda-smoke")
        .env("PATH", path)
        .env("FAKE_ALDA_PID_FILE", &pid_file)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let start = Instant::now();
    while !pid_file.exists() && start.elapsed() < Duration::from_secs(1) {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(pid_file.exists(), "fake Alda process did not start");
    let status = Command::new("kill")
        .args(["-INT", child.id().to_string().as_str()])
        .status()
        .unwrap();
    assert!(status.success());

    let exit_status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "CLI did not cancel promptly"
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert!(!exit_status.success());

    let alda_pid = fs::read_to_string(pid_file).unwrap();
    let still_running = Command::new("kill")
        .args(["-0", alda_pid.trim()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    assert!(!still_running, "Alda child remained after Ctrl+C");
}
