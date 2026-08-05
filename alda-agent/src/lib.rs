pub mod config;
pub mod deepseek;
pub mod doctor;

use clap::Parser;

#[derive(Parser)]
#[command(name = "alda-agent", about = "Alda Music Agent")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(clap::Subcommand)]
pub enum Command {
    /// 检查运行时环境
    Doctor,
    /// 运行 DeepSeek API 连通测试
    Smoke,
}
