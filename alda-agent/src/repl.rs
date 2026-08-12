use crate::agent::{
    Agent, ContinueRequest, CreationMode, CreationRequest, CreationResult, ModifyRequest,
};
use crate::alda::{AldaRunner, CheckStatus, find_alda};
use crate::config::Config;
use crate::deepseek::{DeepSeekClient, Message};
use crate::project::Project;
use anyhow::{Context, Result, bail};
use std::io::{BufRead, Write};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReplCommand {
    Help,
    Play,
    Stop,
    Export,
    History,
    Restore(u32),
    Reload,
    Continue,
    StrategyShow,
    StrategySet(String),
    StrategyClear,
    Quit,
    NaturalLanguage(String),
    Empty,
}

struct Services {
    agent: Agent,
    runner: AldaRunner,
}

#[derive(Debug, Default)]
pub struct ReplSettings {
    pub mode: Option<String>,
    pub target_duration_secs: Option<f64>,
    pub included_instruments: Vec<String>,
    pub excluded_instruments: Vec<String>,
}

impl Services {
    fn load() -> Result<Self> {
        let config = Config::from_env_file()?;
        let client = DeepSeekClient::new_with_thinking(
            config.api_key,
            config.base_url,
            config.model,
            config.thinking,
        )?;
        let alda_path =
            find_alda().ok_or_else(|| anyhow::anyhow!("未找到 alda，请先运行 doctor"))?;
        Ok(Self {
            agent: Agent::new(client, AldaRunner::new(alda_path.clone())),
            runner: AldaRunner::new(alda_path),
        })
    }
}

pub async fn run_repl(project_dir: PathBuf, settings: ReplSettings) -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    run_repl_with_io(project_dir, settings, stdin.lock(), stdout.lock()).await
}

async fn run_repl_with_io<R: BufRead, W: Write>(
    project_dir: PathBuf,
    settings: ReplSettings,
    mut reader: R,
    mut writer: W,
) -> Result<()> {
    let mut project = open_project(project_dir, &mut reader, &mut writer)?;
    apply_settings(&mut project, settings)?;
    let mut services: Option<Services> = None;
    writeln!(
        writer,
        "项目：{}（当前版本：{}）\n输入 /help 查看命令。",
        project.project_name,
        project.current_version()
    )?;

    loop {
        write!(writer, "> ")?;
        writer.flush()?;
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }

        let command = match parse_command(&line) {
            Ok(command) => command,
            Err(error) => {
                writeln!(writer, "错误：{error}")?;
                continue;
            }
        };
        if command == ReplCommand::Quit {
            break;
        }

        if let Err(error) = execute_command(command, &mut project, &mut services, &mut writer).await
        {
            writeln!(writer, "错误：{error:#}")?;
        }
    }
    Ok(())
}

fn apply_settings(project: &mut Project, settings: ReplSettings) -> Result<()> {
    if settings.mode.is_none()
        && settings.target_duration_secs.is_none()
        && settings.included_instruments.is_empty()
        && settings.excluded_instruments.is_empty()
    {
        return Ok(());
    }

    let mode = settings.mode.unwrap_or_else(|| project.mode().to_string());
    let target_duration_secs = settings
        .target_duration_secs
        .or(project.target_duration_secs());
    let included_instruments = if settings.included_instruments.is_empty() {
        project.included_instruments().to_vec()
    } else {
        settings.included_instruments
    };
    let excluded_instruments = if settings.excluded_instruments.is_empty() {
        project.excluded_instruments().to_vec()
    } else {
        settings.excluded_instruments
    };
    project.configure(
        &mode,
        target_duration_secs,
        included_instruments,
        excluded_instruments,
    )
}

// The command table is intentionally linear; each arm delegates domain work.
#[allow(clippy::too_many_lines)]
async fn execute_command<W: Write>(
    command: ReplCommand,
    project: &mut Project,
    services: &mut Option<Services>,
    writer: &mut W,
) -> Result<()> {
    match command {
        ReplCommand::Help => print_help(writer)?,
        ReplCommand::History => print_history(project, writer)?,
        ReplCommand::StrategyShow => {
            if project.creative_strategy().is_empty() {
                writeln!(writer, "项目未设置创作策略；当前使用内置默认策略。")?;
            } else {
                writeln!(
                    writer,
                    "项目创作策略（优先于冲突的内置默认）：\n{}",
                    project.creative_strategy()
                )?;
            }
        }
        ReplCommand::StrategySet(strategy) => {
            project.set_creative_strategy(&strategy)?;
            writeln!(writer, "已替换项目创作策略。")?;
        }
        ReplCommand::StrategyClear => {
            project.set_creative_strategy("")?;
            writeln!(writer, "已清除项目创作策略；将使用内置默认策略。")?;
        }
        ReplCommand::Restore(version) => {
            project.restore_version(version)?;
            writeln!(writer, "已恢复版本 {version}；历史文件未被覆盖。")?;
        }
        ReplCommand::Play => {
            let services = load_services(services)?;
            services.runner.play(&project.current_version_path()?)?;
            writeln!(writer, "正在播放版本 {}。", project.current_version())?;
        }
        ReplCommand::Stop => {
            let services = load_services(services)?;
            services.runner.stop()?;
            writeln!(writer, "已请求停止播放。")?;
        }
        ReplCommand::Export => {
            let services = load_services(services)?;
            let alda_path = project.export_alda()?;
            let midi_path = project.midi_export_path()?;
            services
                .runner
                .export_midi(&project.current_version_path()?, &midi_path)?;
            writeln!(
                writer,
                "已导出：\n  {}\n  {}",
                alda_path.display(),
                midi_path.display()
            )?;
        }
        ReplCommand::Reload => {
            let services = load_services(services)?;
            let candidate_path = project.root().join("current.alda");
            let candidate = std::fs::read_to_string(&candidate_path)
                .with_context(|| format!("无法读取 {}", candidate_path.display()))?;
            let checks = services
                .runner
                .validate_async(
                    candidate_path,
                    project.included_instruments().to_vec(),
                    project.excluded_instruments().to_vec(),
                    project
                        .target_duration_secs()
                        .map(|seconds| seconds * 1000.0),
                    10.0,
                )
                .await?;
            print_checks(&checks, writer)?;
            if checks.iter().any(|check| check.status == CheckStatus::Fail) {
                if project.current_version() > 0 {
                    project.restore_version(project.current_version())?;
                }
                bail!("人工修改未通过检查，未采用且已恢复当前有效版本");
            }
            let version = project.save_version(&candidate, "人工编辑并显式重新采用", &checks)?;
            writeln!(writer, "人工修改已采用为版本 {version}。")?;
        }
        ReplCommand::Continue => {
            let services = load_services(services)?;
            let result = services
                .agent
                .continue_generation(ContinueRequest {
                    conversation: project.conversation.clone(),
                    target_duration_secs: project.target_duration_secs(),
                    included_instruments: project.included_instruments().to_vec(),
                    excluded_instruments: project.excluded_instruments().to_vec(),
                    max_rounds: 3,
                })
                .await?;
            apply_result(project, result, "继续自动修正", writer)?;
        }
        ReplCommand::NaturalLanguage(feedback) => {
            let services = load_services(services)?;
            project.record_requirement(feedback.clone())?;
            let result = if conversation_awaits_input(&project.conversation) {
                let mut conversation = project.conversation.clone();
                conversation.push(Message {
                    role: "user".to_string(),
                    content: Some(feedback.clone()),
                    tool_calls: None,
                    tool_call_id: None,
                });
                services
                    .agent
                    .continue_generation(ContinueRequest {
                        conversation,
                        target_duration_secs: project.target_duration_secs(),
                        included_instruments: project.included_instruments().to_vec(),
                        excluded_instruments: project.excluded_instruments().to_vec(),
                        max_rounds: 3,
                    })
                    .await?
            } else if project.current_version() == 0 {
                services
                    .agent
                    .create(CreationRequest {
                        source_material: project.source_material.clone(),
                        instructions: feedback.clone(),
                        creative_strategy: project.creative_strategy().to_string(),
                        mode: if project.mode() == "improv" {
                            CreationMode::Improvisation
                        } else {
                            CreationMode::FullPiece
                        },
                        target_duration_secs: project.target_duration_secs(),
                        included_instruments: project.included_instruments().to_vec(),
                        excluded_instruments: project.excluded_instruments().to_vec(),
                        max_rounds: 3,
                    })
                    .await?
            } else {
                services
                    .agent
                    .modify(ModifyRequest {
                        source_material: project.source_material.clone(),
                        current_alda: project.version_code(project.current_version())?,
                        feedback: feedback.clone(),
                        creative_strategy: project.creative_strategy().to_string(),
                        mode: if project.mode() == "improv" {
                            CreationMode::Improvisation
                        } else {
                            CreationMode::FullPiece
                        },
                        target_duration_secs: project.target_duration_secs(),
                        included_instruments: project.included_instruments().to_vec(),
                        excluded_instruments: project.excluded_instruments().to_vec(),
                        max_rounds: 3,
                    })
                    .await?
            };
            apply_result(project, result, &feedback, writer)?;
        }
        ReplCommand::Empty | ReplCommand::Quit => {}
    }
    Ok(())
}

fn apply_result<W: Write>(
    project: &mut Project,
    result: CreationResult,
    summary: &str,
    writer: &mut W,
) -> Result<()> {
    print_checks(&result.checks, writer)?;
    let interpretation = if project.interpretation.is_empty() {
        result.interpretation.clone()
    } else if result.interpretation.is_empty() {
        project.interpretation.clone()
    } else {
        format!("{}\n\n{}", project.interpretation, result.interpretation)
    };

    if result.needs_input {
        project.update_context(interpretation, result.conversation)?;
        writeln!(writer, "需要补充信息；请直接回答上面的澄清问题。")?;
    } else if result.success {
        let code = result
            .alda_code
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("成功结果缺少 Alda 代码"))?;
        let version = project.save_version(code, summary, &result.checks)?;
        project.update_context(interpretation, Vec::new())?;
        writeln!(writer, "已保存有效版本 {version}（{} 轮）。", result.rounds)?;
    } else {
        project.update_context(interpretation, result.conversation)?;
        writeln!(
            writer,
            "{} 轮后仍未通过；当前有效版本未改变。可输入 /continue 继续修正。",
            result.rounds
        )?;
    }
    Ok(())
}

fn conversation_awaits_input(conversation: &[Message]) -> bool {
    conversation.last().is_some_and(|message| {
        message.role == "assistant"
            && message.tool_calls.is_none()
            && message
                .content
                .as_deref()
                .is_some_and(|content| !content.trim().is_empty())
    })
}

fn open_project<R: BufRead, W: Write>(
    project_dir: PathBuf,
    reader: &mut R,
    writer: &mut W,
) -> Result<Project> {
    if project_dir.join("project.json").exists() {
        return Project::load_or_create(project_dir, "existing-project", "");
    }

    let name = project_dir
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty() && *name != ".")
        .unwrap_or("project")
        .to_string();
    writeln!(writer, "新项目，请粘贴素材；单独输入一行 . 结束：")?;
    let mut source = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line.trim_end() == "." {
            break;
        }
        source.push_str(&line);
    }
    if source.trim().is_empty() {
        bail!("新项目素材不能为空");
    }
    Project::load_or_create(project_dir, &name, source.trim())
}

fn load_services(services: &mut Option<Services>) -> Result<&Services> {
    if services.is_none() {
        *services = Some(Services::load()?);
    }
    services
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("无法初始化运行服务"))
}

fn parse_command(input: &str) -> Result<ReplCommand> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(ReplCommand::Empty);
    }
    if !input.starts_with('/') {
        return Ok(ReplCommand::NaturalLanguage(input.to_string()));
    }
    if input == "/strategy" {
        return Ok(ReplCommand::StrategyShow);
    }
    if let Some(strategy) = input.strip_prefix("/strategy ") {
        let strategy = strategy.trim();
        if strategy.is_empty() {
            bail!("用法：/strategy [策略文本|clear]");
        }
        return if strategy.eq_ignore_ascii_case("clear") {
            Ok(ReplCommand::StrategyClear)
        } else {
            Ok(ReplCommand::StrategySet(strategy.to_string()))
        };
    }
    let mut parts = input.split_whitespace();
    match parts.next().unwrap_or_default() {
        "/help" => Ok(ReplCommand::Help),
        "/play" => Ok(ReplCommand::Play),
        "/stop" => Ok(ReplCommand::Stop),
        "/export" => Ok(ReplCommand::Export),
        "/history" => Ok(ReplCommand::History),
        "/reload" => Ok(ReplCommand::Reload),
        "/continue" => Ok(ReplCommand::Continue),
        "/quit" => Ok(ReplCommand::Quit),
        "/restore" => {
            let version = parts
                .next()
                .ok_or_else(|| anyhow::anyhow!("用法：/restore <版本号>"))?
                .parse::<u32>()
                .context("版本号必须是正整数")?;
            if parts.next().is_some() || version == 0 {
                bail!("用法：/restore <版本号>");
            }
            Ok(ReplCommand::Restore(version))
        }
        command => bail!("未知命令：{command}；输入 /help 查看可用命令"),
    }
}

fn print_help<W: Write>(writer: &mut W) -> Result<()> {
    writeln!(
        writer,
        "/play             播放当前有效版本\n/stop             停止播放\n/export           导出 Alda 与 MIDI\n/history          查看有效版本\n/restore N        恢复版本 N\n/reload           校验并采用手工编辑的 current.alda\n/continue         继续上一轮失败的自动修正\n/strategy         查看项目创作策略\n/strategy <文本>  替换项目创作策略\n/strategy clear   清除项目创作策略\n/quit             退出"
    )?;
    Ok(())
}

fn print_history<W: Write>(project: &Project, writer: &mut W) -> Result<()> {
    if project.versions().is_empty() {
        writeln!(writer, "尚无有效版本。")?;
    }
    for (version, metadata) in project.versions() {
        let marker = if *version == project.current_version() {
            "*"
        } else {
            " "
        };
        writeln!(
            writer,
            "{marker} {version:04}  {}  {}",
            metadata.created_at, metadata.summary
        )?;
    }
    Ok(())
}

fn print_checks<W: Write>(checks: &[crate::alda::AldaCheck], writer: &mut W) -> Result<()> {
    for check in checks {
        writeln!(
            writer,
            "  {} {}：{}",
            check.status, check.name, check.detail
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands_and_natural_language() {
        assert_eq!(
            parse_command("/restore 12").unwrap(),
            ReplCommand::Restore(12)
        );
        assert_eq!(parse_command("/reload").unwrap(), ReplCommand::Reload);
        assert_eq!(
            parse_command("/strategy").unwrap(),
            ReplCommand::StrategyShow
        );
        assert_eq!(
            parse_command("/strategy  明亮欢快，增加随机性").unwrap(),
            ReplCommand::StrategySet("明亮欢快，增加随机性".to_string())
        );
        assert_eq!(
            parse_command("/strategy clear").unwrap(),
            ReplCommand::StrategyClear
        );
        assert_eq!(
            parse_command("让结尾更明亮").unwrap(),
            ReplCommand::NaturalLanguage("让结尾更明亮".to_string())
        );
        assert!(parse_command("/restore ../2").is_err());
        assert!(parse_command("/unknown").is_err());
    }

    #[test]
    fn only_plain_assistant_reply_is_treated_as_pending_clarification() {
        let question = Message {
            role: "assistant".to_string(),
            content: Some("你希望整体重构还是只改中段？".to_string()),
            tool_calls: None,
            tool_call_id: None,
        };
        assert!(conversation_awaits_input(&[question]));

        let tool_result = Message {
            role: "tool".to_string(),
            content: Some("所有检查通过".to_string()),
            tool_calls: None,
            tool_call_id: Some("call_1".to_string()),
        };
        assert!(!conversation_awaits_input(&[tool_result]));
    }

    #[tokio::test]
    async fn history_and_restore_work_without_provider_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let input = b"material\n.\n/history\n/quit\n";
        let mut output = Vec::new();
        run_repl_with_io(
            directory.path().join("project"),
            ReplSettings::default(),
            &input[..],
            &mut output,
        )
        .await
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("尚无有效版本"));
    }

    #[tokio::test]
    async fn strategy_commands_persist_without_loading_provider() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("project");
        let input =
            b"material\n.\n/strategy bright and playful\n/strategy\n/strategy clear\n/quit\n";
        let mut output = Vec::new();
        run_repl_with_io(
            root.clone(),
            ReplSettings::default(),
            &input[..],
            &mut output,
        )
        .await
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("bright and playful"));
        let project = Project::load_or_create(root, "ignored", "ignored").unwrap();
        assert_eq!(project.creative_strategy(), "");
    }

    #[test]
    fn repl_settings_are_validated_and_persisted_by_project() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("project");
        let mut project = Project::load_or_create(root.clone(), "project", "material").unwrap();
        apply_settings(
            &mut project,
            ReplSettings {
                mode: Some("improv".to_string()),
                target_duration_secs: Some(60.0),
                included_instruments: vec!["piano".to_string()],
                excluded_instruments: vec!["violin".to_string()],
            },
        )
        .unwrap();

        let reloaded = Project::load_or_create(root, "ignored", "ignored").unwrap();
        assert_eq!(reloaded.mode(), "improv");
        assert_eq!(reloaded.target_duration_secs(), Some(60.0));
        assert_eq!(reloaded.included_instruments(), ["piano"]);
        assert_eq!(reloaded.excluded_instruments(), ["violin"]);
    }
}
