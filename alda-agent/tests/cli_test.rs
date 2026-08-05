use alda_agent::{Cli, Command};
use clap::Parser;

#[test]
fn test_doctor_subcommand() {
    let cli = Cli::try_parse_from(["alda-agent", "doctor"]).unwrap();
    assert!(matches!(cli.command, Command::Doctor));
}
