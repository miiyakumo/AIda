use alda_agent::{Cli, Command};
use clap::Parser;

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Doctor => alda_agent::doctor::run(),
    }
}
