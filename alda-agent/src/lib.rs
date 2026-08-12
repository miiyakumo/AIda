pub mod agent;
pub mod alda;
pub mod config;
pub mod deepseek;
pub mod doctor;
pub mod project;
pub mod repl;
#[cfg(test)]
pub(crate) mod test_support;

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
    /// 列出默认目录中的项目
    List,
    /// 进入单进程交互式项目
    Repl {
        /// 项目目录；默认使用当前目录
        #[arg(short, long, conflicts_with = "name")]
        project: Option<PathBuf>,
        /// 在默认项目目录中打开此名称
        #[arg(short, long, conflicts_with = "project")]
        name: Option<String>,
        /// 设置项目创作模式: full（完整曲目）或 improv（即兴片段）
        #[arg(short, long)]
        mode: Option<String>,
        /// 设置项目目标时长（秒）
        #[arg(long)]
        duration: Option<f64>,
        /// 设置项目必须包含的乐器（可重复指定）
        #[arg(long = "include")]
        include: Vec<String>,
        /// 设置项目必须排除的乐器（可重复指定）
        #[arg(long = "exclude")]
        exclude: Vec<String>,
    },
    /// 检查运行时环境
    Doctor,
    /// 运行 `DeepSeek` API 连通测试
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
