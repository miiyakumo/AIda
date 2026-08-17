use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::process::Command;
use std::process::Stdio;

fn executable(directory: &std::path::Path, name: &str, body: &str) {
    let path = directory.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

fn expose_system_program(directory: &std::path::Path, name: &str) {
    let source = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|path| path.join(name))
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("system program not found: {name}"));
    symlink(source, directory.join(name)).unwrap();
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

fn mock_macos(directory: &std::path::Path) {
    executable(directory, "uname", "echo Darwin");
}

#[test]
fn macos_check_mode_never_invokes_installers() {
    let directory = tempfile::tempdir().unwrap();
    mock_macos(directory.path());
    executable(directory.path(), "java", "exit 0");
    executable(directory.path(), "alda", "exit 0");
    executable(directory.path(), "rustc", "exit 0");
    executable(directory.path(), "fluidsynth", "exit 0");
    executable(directory.path(), "brew", "exit 99");
    executable(directory.path(), "curl", "exit 99");
    let path = format!("{}:/usr/bin:/bin", directory.path().display());
    let soundfont = directory.path().join("test.sf2");
    fs::write(&soundfont, b"fake").unwrap();

    let output = Command::new("/bin/bash")
        .arg("scripts/install-macos.sh")
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
    assert!(stdout.contains("fluidsynth"));
}

#[test]
fn macos_unknown_argument_fails_without_side_effects() {
    let output = Command::new("bash")
        .arg("scripts/install-macos.sh")
        .arg("--unknown")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn macos_normal_mode_is_idempotent_when_dependencies_exist() {
    let directory = tempfile::tempdir().unwrap();
    mock_macos(directory.path());
    executable(directory.path(), "java", "exit 0");
    executable(directory.path(), "alda", "exit 0");
    executable(directory.path(), "fluidsynth", "exit 0");
    executable(directory.path(), "brew", "exit 99");
    executable(directory.path(), "curl", "exit 99");
    let path = format!("{}:/usr/bin:/bin", directory.path().display());
    let soundfont = directory.path().join("test.sf2");
    fs::write(&soundfont, b"fake").unwrap();

    let output = Command::new("bash")
        .arg("scripts/install-macos.sh")
        .env("PATH", path)
        .env("ALDA_AGENT_SOUNDFONT", soundfont)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("运行环境已就绪")
    );
}

#[test]
fn macos_installs_missing_formulae_and_verified_soundfont_without_sudo() {
    let directory = tempfile::tempdir().unwrap();
    mock_macos(directory.path());
    for program in [
        "awk", "chmod", "cp", "dirname", "mkdir", "mktemp", "mv", "rm",
    ] {
        expose_system_program(directory.path(), program);
    }
    let brew_log = directory.path().join("brew.log");
    let sudo_marker = directory.path().join("sudo-called");
    let soundfont_source = directory.path().join("source.sf2");
    fs::write(&soundfont_source, b"verified soundfont").unwrap();
    executable(
        directory.path(),
        "brew",
        "printf '%s\\n' \"$*\" > \"$BREW_LOG\"\nfor name in alda fluidsynth java; do\n  printf '#!/bin/sh\\nexit 0\\n' > \"$MOCK_BIN/$name\"\n  chmod +x \"$MOCK_BIN/$name\"\ndone",
    );
    executable(directory.path(), "curl", "cp \"$SOUNDFONT_SOURCE\" \"$4\"");
    executable(
        directory.path(),
        "shasum",
        "echo '9575028c7a1f589f5770fccc8cff2734566af40cd26ed836944e9a5152688cfe  file'",
    );
    executable(directory.path(), "sudo", "touch \"$SUDO_MARKER\"; exit 99");
    let path = directory.path().display().to_string();
    let data_home = directory.path().join("data");

    let output = Command::new("/bin/bash")
        .arg("scripts/install-macos.sh")
        .env("PATH", path)
        .env("BREW_LOG", &brew_log)
        .env("MOCK_BIN", directory.path())
        .env("SOUNDFONT_SOURCE", &soundfont_source)
        .env("SUDO_MARKER", &sudo_marker)
        .env("XDG_DATA_HOME", &data_home)
        .env_remove("ALDA_AGENT_SOUNDFONT")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(brew_log).unwrap().trim(),
        "install alda fluid-synth"
    );
    assert!(!sudo_marker.exists());
    assert_eq!(
        fs::read(data_home.join("alda-agent/soundfonts/GeneralUser-GS.sf2")).unwrap(),
        b"verified soundfont"
    );
}
