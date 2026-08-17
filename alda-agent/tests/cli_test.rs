use alda_agent::{Cli, Command, ProbeTarget};
use clap::Parser;
use std::process::Command as ProcessCommand;

#[test]
fn default_and_project_entries_have_no_subcommand() {
    let cli = Cli::try_parse_from(["alda-agent"]).unwrap();
    assert!(cli.command.is_none());
    let cli = Cli::try_parse_from(["alda-agent", "--project", "/tmp/project"]).unwrap();
    assert_eq!(
        cli.project.unwrap(),
        std::path::PathBuf::from("/tmp/project")
    );
    let cli = Cli::try_parse_from(["alda-agent", "--name", "poem"]).unwrap();
    assert_eq!(cli.name.as_deref(), Some("poem"));
}

#[test]
fn new_shell_commands_parse_and_removed_ones_fail() {
    let cli = Cli::try_parse_from(["alda-agent", "projects"]).unwrap();
    assert!(matches!(cli.command, Some(Command::Projects)));
    let cli = Cli::try_parse_from(["alda-agent", "doctor", "--probe", "alda"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Doctor {
            probe: Some(ProbeTarget::Alda)
        })
    ));
    let cli = Cli::try_parse_from(["alda-agent", "--project", "/tmp/project", "control"]).unwrap();
    assert!(matches!(cli.command, Some(Command::Control)));
    assert!(Cli::try_parse_from(["alda-agent", "repl"]).is_err());
    assert!(Cli::try_parse_from(["alda-agent", "list"]).is_err());
    assert!(Cli::try_parse_from(["alda-agent", "create"]).is_err());
    assert!(Cli::try_parse_from(["alda-agent", "smoke"]).is_err());
    assert!(Cli::try_parse_from(["alda-agent", "alda-smoke"]).is_err());
}

#[test]
fn missing_runtime_dependencies_refuse_to_start_before_creating_a_project() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("must-not-be-created");
    let output = ProcessCommand::new(env!("CARGO_BIN_EXE_alda-agent"))
        .args(["--project", project.to_str().unwrap()])
        .env("PATH", "")
        .env_remove("ALDA_AGENT_SOUNDFONT")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("运行环境不完整，未启动 Alda Agent"));
    assert!(stderr.contains("scripts/install-linux.sh"));
    assert!(!project.exists());
}
