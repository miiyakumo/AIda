use alda_agent::{Cli, Command, ProbeTarget};
use clap::Parser;

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
