use alda_agent::{Cli, Command};
use clap::Parser;

#[test]
fn test_doctor_subcommand() {
    let cli = Cli::try_parse_from(["alda-agent", "doctor"]).unwrap();
    assert!(matches!(cli.command, Command::Doctor));
}

#[test]
fn test_project_commands() {
    let cli = Cli::try_parse_from(["alda-agent", "list"]).unwrap();
    assert!(matches!(cli.command, Command::List));

    let cli = Cli::try_parse_from(["alda-agent", "repl", "--project", "/tmp/project"]).unwrap();
    assert!(matches!(
        cli.command,
        Command::Repl {
            project: Some(path),
            name: None,
            ..
        } if path == *"/tmp/project"
    ));

    let cli = Cli::try_parse_from(["alda-agent", "repl", "--name", "poem"]).unwrap();
    assert!(matches!(
        cli.command,
        Command::Repl {
            project: None,
            name: Some(name),
            ..
        } if name == "poem"
    ));

    let cli = Cli::try_parse_from([
        "alda-agent",
        "repl",
        "--name",
        "poem",
        "--mode",
        "full",
        "--duration",
        "180",
        "--include",
        "piano",
        "--exclude",
        "violin",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Command::Repl {
            mode: Some(mode),
            duration: Some(180.0),
            include,
            exclude,
            ..
        } if mode == "full" && include == ["piano"] && exclude == ["violin"]
    ));
}
