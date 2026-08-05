pub mod agent;
pub mod alda;
pub mod config;
pub mod deepseek;
pub mod doctor;

use clap::Parser;
use std::path::PathBuf;

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
    /// 运行 Alda 工具连通测试
    AldaSmoke,
    /// 基于素材创作 Alda 音乐作品
    Create {
        /// 素材文本文件路径（不指定则从 stdin 读取）
        #[arg(short, long)]
        file: Option<PathBuf>,
        /// 创作模式: full（完整曲目）或 improv（即兴片段），默认 full
        #[arg(short, long, default_value = "full")]
        mode: String,
        /// 目标时长（秒）
        #[arg(long)]
        duration: Option<f64>,
        /// 必须包含的乐器（可重复指定）
        #[arg(long = "include")]
        include: Vec<String>,
        /// 必须排除的乐器（可重复指定）
        #[arg(long = "exclude")]
        exclude: Vec<String>,
        /// 输出目录（保存 current.alda），默认当前目录
        #[arg(short, long, default_value = ".")]
        output: PathBuf,
    },
}
