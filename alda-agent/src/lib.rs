pub mod agent;
pub mod alda;
pub mod application;
pub mod audio;
pub mod command;
pub mod composition;
pub mod config;
pub mod control;
pub mod conversation;
pub mod deepseek;
pub mod doctor;
pub mod instructions;
pub mod project;
pub mod repl;
pub mod skills;
#[cfg(test)]
pub(crate) mod test_support;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "alda-agent", about = "Alda Music Agent")]
pub struct Cli {
    /// 在默认项目目录中打开此名称
    #[arg(long, conflicts_with = "project")]
    pub name: Option<String>,
    /// 打开或创建指定项目目录
    #[arg(long, conflicts_with = "name")]
    pub project: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// 列出默认目录中的项目
    Projects,
    /// 检查运行时环境
    Doctor {
        /// 额外执行真实连通探测；不指定时只检查本地环境
        #[arg(long, value_enum)]
        probe: Option<ProbeTarget>,
    },
    /// 通过 stdin/stdout 上的 JSONL 协议操控项目
    Control,
    /// 一次性创作 Alda 音乐作品
    Compose {
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

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum ProbeTarget {
    Model,
    Alda,
    All,
}
