use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::process::Stdio;

fn executable(directory: &std::path::Path, name: &str, body: &str) {
    let path = directory.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

#[test]
fn check_mode_never_invokes_sudo_or_installers() {
    let directory = tempfile::tempdir().unwrap();
    executable(directory.path(), "java", "echo java 21");
    executable(directory.path(), "alda", "echo alda 2.3.0");
    executable(directory.path(), "rustc", "echo rustc 1.85.0");
    executable(directory.path(), "fluidsynth", "echo FluidSynth 2.3");
    executable(directory.path(), "sudo", "exit 99");
    executable(directory.path(), "curl", "exit 99");
    let path = format!("{}:/usr/bin:/bin", directory.path().display());
    let soundfont = directory.path().join("test.sf2");
    fs::write(&soundfont, b"fake").unwrap();

    let output = Command::new("bash")
        .arg("scripts/install-linux.sh")
        .arg("--check")
        .env("PATH", path)
        .env("ALDA_AGENT_SOUNDFONT", soundfont)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("java"));
    assert!(stdout.contains("alda"));
    assert!(stdout.contains("rustc"));
}

#[test]
fn unknown_argument_fails_without_side_effects() {
    let output = Command::new("bash")
        .arg("scripts/install-linux.sh")
        .arg("--unknown")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn normal_mode_is_idempotent_when_dependencies_exist() {
    let directory = tempfile::tempdir().unwrap();
    executable(directory.path(), "java", "echo java 21");
    executable(directory.path(), "alda", "echo alda 2.3.0");
    executable(directory.path(), "rustc", "echo rustc 1.85.0");
    executable(directory.path(), "fluidsynth", "echo FluidSynth 2.3");
    executable(directory.path(), "sudo", "exit 99");
    executable(directory.path(), "curl", "exit 99");
    let path = format!("{}:/usr/bin:/bin", directory.path().display());
    let soundfont = directory.path().join("test.sf2");
    fs::write(&soundfont, b"fake").unwrap();

    let output = Command::new("bash")
        .arg("scripts/install-linux.sh")
        .env("PATH", path)
        .env("ALDA_AGENT_SOUNDFONT", soundfont)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Java 已安装"));
    assert!(stdout.contains("Alda 已安装"));
    assert!(stdout.contains("Rust 工具链已安装"));
}

#[test]
fn declining_java_install_never_invokes_sudo() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("sudo-called");
    executable(
        directory.path(),
        "sudo",
        "echo called > \"$SUDO_MARKER\"; exit 99",
    );
    let mut child = Command::new("/bin/bash")
        .arg("scripts/install-linux.sh")
        .env("PATH", directory.path())
        .env("SUDO_MARKER", &marker)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"n\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(!marker.exists(), "sudo was invoked after the user declined");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Java 未安装"));
}
