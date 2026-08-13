use alda_agent::{Cli, Command};
use anyhow::Context;
use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let Cli {
        project,
        name,
        command,
    } = Cli::parse();

    match command {
        Some(Command::Projects) => list_projects(),
        Some(Command::Doctor { probe }) => {
            alda_agent::doctor::run(probe, selected_project_root(project, name)?).await
        }
        Some(Command::Control) => {
            let (project, name) = selected_project(project, name)?;
            alda_agent::control::run(project, name).await
        }
        Some(Command::Compose {
            file,
            mode,
            duration,
            include,
            exclude,
            output,
        }) => {
            let project_root = selected_project_root(project, name)?;
            run_cancelable(compose(
                project_root,
                file,
                mode,
                duration,
                include,
                exclude,
                output,
            ))
            .await
        }
        None => {
            let (project, name) = selected_project(project, name)?;
            alda_agent::repl::run_repl(project, name).await
        }
    }
}

fn selected_project(
    project: Option<std::path::PathBuf>,
    name: Option<String>,
) -> anyhow::Result<(std::path::PathBuf, String)> {
    if let Some(project) = project {
        let name = project
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("project")
            .to_string();
        Ok((project, name))
    } else if let Some(name) = name {
        Ok((alda_agent::project::default_project_dir(&name)?, name))
    } else {
        let project = std::env::current_dir()?;
        let name = project
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("project")
            .to_string();
        Ok((project, name))
    }
}

fn selected_project_root(
    project: Option<std::path::PathBuf>,
    name: Option<String>,
) -> anyhow::Result<std::path::PathBuf> {
    if let Some(project) = project {
        Ok(project)
    } else if let Some(name) = name {
        alda_agent::project::default_project_dir(&name)
    } else {
        Ok(std::env::current_dir()?)
    }
}

async fn run_cancelable<F>(operation: F) -> anyhow::Result<()>
where
    F: std::future::Future<Output = anyhow::Result<()>>,
{
    tokio::select! {
        result = operation => result,
        signal = tokio::signal::ctrl_c() => {
            signal.context("无法监听 Ctrl+C")?;
            anyhow::bail!("操作已由用户取消")
        }
    }
}

fn list_projects() -> anyhow::Result<()> {
    let projects = alda_agent::project::list_projects()?;
    if projects.is_empty() {
        println!("默认目录中没有项目。");
    }
    for (name, path) in projects {
        println!("{}\t{}", name, path.display());
    }
    Ok(())
}

async fn compose(
    project_root: std::path::PathBuf,
    file: Option<std::path::PathBuf>,
    mode: String,
    duration: Option<f64>,
    include: Vec<String>,
    exclude: Vec<String>,
    output: std::path::PathBuf,
) -> anyhow::Result<()> {
    use alda_agent::agent::{Agent, AgentResultKind, CreationMode, CreationRequest};
    use alda_agent::alda::{AldaRunner, CheckStatus, find_alda};
    use alda_agent::config::ModelConfig;
    use alda_agent::deepseek::DeepSeekClient;
    use std::io::Read;

    // 读取素材
    let source_material = if let Some(ref path) = file {
        std::fs::read_to_string(path).context("无法读取素材文件")?
    } else {
        eprintln!("请输入创作素材（Ctrl+D 结束）:");
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("无法读取 stdin")?;
        buf.trim().to_string()
    };

    if source_material.is_empty() {
        anyhow::bail!("素材不能为空");
    }

    // 初始化
    let config = ModelConfig::load(&project_root)?.resolve()?;
    let client = DeepSeekClient::new(config.api_key, config.base_url, config.model)?;
    let alda_path = find_alda().ok_or_else(|| anyhow::anyhow!("未找到 alda，请先安装"))?;
    let runner = AldaRunner::new(alda_path);
    let agent = Agent::new(client, runner);

    let creation_mode = match mode.as_str() {
        "full" => CreationMode::FullPiece,
        "improv" => CreationMode::Improvisation,
        other => anyhow::bail!("无效的创作模式: {other}（应为 full 或 improv）"),
    };

    let request = CreationRequest {
        source_material,
        instructions: String::new(),
        creative_strategy: String::new(),
        mode: creation_mode,
        target_duration_secs: duration,
        included_instruments: include,
        excluded_instruments: exclude,
        max_rounds: 3,
    };

    println!("\n=== 开始创作 ===\n");

    let result = agent.create(request).await?;

    println!("\n=== 创作完成 ({}/{} 轮) ===\n", result.rounds, 3);
    println!(
        "状态: {}",
        if result.success {
            "✅ 成功"
        } else {
            "❌ 失败"
        }
    );
    println!();

    println!("校验结果:");
    for check in &result.checks {
        let icon = match check.status {
            CheckStatus::Pass => "✅",
            CheckStatus::Fail => "❌",
            CheckStatus::Unchecked => "⏭ ",
        };
        println!("  {} {}: {}", icon, check.name, check.detail);
    }

    if result.needs_input {
        anyhow::bail!("模型需要补充信息；请进入交互模式回答澄清问题");
    } else if result.success && result.kind == AgentResultKind::Candidate {
        if let Some(ref code) = result.alda_code {
            let output_file = output.join("current.alda");
            std::fs::write(&output_file, code)?;
            println!("\n作品已保存到: {}", output_file.display());
        }
    } else if matches!(result.kind, AgentResultKind::Answer | AgentResultKind::Plan) {
        anyhow::bail!("模型返回了文字结果；请进入交互模式继续创作");
    } else if result.kind == AgentResultKind::Draft {
        anyhow::bail!("模型返回了草稿；请进入交互模式试听和继续发展");
    } else {
        if result.was_truncated {
            println!("\n⚠️  模型输出被截断，作品可能不完整。");
        }
        anyhow::bail!("作品在 {} 轮校验后仍未通过，未保存有效版本", result.rounds);
    }

    Ok(())
}
