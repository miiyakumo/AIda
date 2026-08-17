use anyhow::{Result, bail};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub enum UserAction {
    Agent(String),
    Alda(AldaAction),
    Project(ProjectAction),
    Help(Vec<String>),
    Quit,
    Empty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AldaAction {
    Play(ScoreTarget),
    PlaySection {
        target: ScoreTarget,
        section_id: String,
        context_secs: u32,
    },
    Stop,
    Check(ScoreTarget),
    Export {
        target: ScoreTarget,
        format: ExportFormat,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScoreTarget {
    Version(Option<u32>),
    Working,
    File(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Alda,
    Midi,
    Wav,
    All,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProjectAction {
    Overview,
    Instructions,
    Skills,
    SkillEnable(String),
    SkillDisable(String),
    Versions,
    Switch(u32),
    Adopt(PathBuf),
    Accept,
    Discard,
    Config(ConfigAction),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConfigAction {
    Show,
    Mode(String),
    Duration(Option<f64>),
    Include(Vec<String>),
    Exclude(Vec<String>),
    Model(String),
    PromptModel,
    Url(String),
    PromptUrl,
    ApiKey(Option<String>),
}

pub const TOP_LEVEL_COMMANDS: &[&str] = &["/alda", "/project", "/help", "/quit"];
pub const ALDA_COMMANDS: &[&str] = &["play", "stop", "check", "export"];
pub const PROJECT_COMMANDS: &[&str] = &[
    "instructions",
    "skills",
    "versions",
    "switch",
    "adopt",
    "accept",
    "discard",
    "config",
];
pub const SKILL_COMMANDS: &[&str] = &["enable", "disable"];
pub const CONFIG_COMMANDS: &[&str] = &[
    "model", "url", "key", "mode", "duration", "include", "exclude",
];

pub fn parse(input: &str) -> Result<UserAction> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(UserAction::Empty);
    }
    if !input.starts_with('/') {
        return Ok(UserAction::Agent(input.to_string()));
    }
    let words = input.split_whitespace().collect::<Vec<_>>();
    if words.first() == Some(&"/alda") {
        return parse_alda(&words[1..]).map(UserAction::Alda);
    }
    match words.as_slice() {
        ["/quit"] => Ok(UserAction::Quit),
        ["/help", rest @ ..] => Ok(UserAction::Help(
            rest.iter().map(ToString::to_string).collect(),
        )),
        ["/project"] => Ok(UserAction::Project(ProjectAction::Overview)),
        ["/project", "instructions"] => Ok(UserAction::Project(ProjectAction::Instructions)),
        ["/project", "skills"] => Ok(UserAction::Project(ProjectAction::Skills)),
        ["/project", "skills", "enable", id] => Ok(UserAction::Project(
            ProjectAction::SkillEnable((*id).to_string()),
        )),
        ["/project", "skills", "disable", id] => Ok(UserAction::Project(
            ProjectAction::SkillDisable((*id).to_string()),
        )),
        ["/project", "versions"] => Ok(UserAction::Project(ProjectAction::Versions)),
        ["/project", "switch", version] => Ok(UserAction::Project(ProjectAction::Switch(
            parse_version(version)?,
        ))),
        ["/project", "adopt", path] => Ok(UserAction::Project(ProjectAction::Adopt(
            PathBuf::from(path),
        ))),
        ["/project", "accept"] => Ok(UserAction::Project(ProjectAction::Accept)),
        ["/project", "discard"] => Ok(UserAction::Project(ProjectAction::Discard)),
        ["/project", "config"] => Ok(UserAction::Project(ProjectAction::Config(
            ConfigAction::Show,
        ))),
        ["/project", "config", "url"] => Ok(UserAction::Project(ProjectAction::Config(
            ConfigAction::PromptUrl,
        ))),
        ["/project", "config", "model"] => Ok(UserAction::Project(ProjectAction::Config(
            ConfigAction::PromptModel,
        ))),
        ["/project", "config", "mode", mode @ ("full" | "improv")] => Ok(UserAction::Project(
            ProjectAction::Config(ConfigAction::Mode((*mode).to_string())),
        )),
        ["/project", "config", "duration", "none"] => Ok(UserAction::Project(
            ProjectAction::Config(ConfigAction::Duration(None)),
        )),
        ["/project", "config", "duration", duration] => Ok(UserAction::Project(
            ProjectAction::Config(ConfigAction::Duration(Some(duration.parse().map_err(
                |_| anyhow::anyhow!("时长必须是正数；用法：/project config duration SECONDS|none"),
            )?))),
        )),
        ["/project", "config", "include", instruments @ ..] if !instruments.is_empty() => {
            Ok(UserAction::Project(ProjectAction::Config(
                ConfigAction::Include(parse_instruments(instruments)),
            )))
        }
        ["/project", "config", "exclude", instruments @ ..] if !instruments.is_empty() => {
            Ok(UserAction::Project(ProjectAction::Config(
                ConfigAction::Exclude(parse_instruments(instruments)),
            )))
        }
        ["/project", "config", "model", model] => Ok(UserAction::Project(ProjectAction::Config(
            ConfigAction::Model((*model).to_string()),
        ))),
        ["/project", "config", "url", url] => Ok(UserAction::Project(ProjectAction::Config(
            ConfigAction::Url((*url).to_string()),
        ))),
        ["/project", "config", "key"] => Ok(UserAction::Project(ProjectAction::Config(
            ConfigAction::ApiKey(None),
        ))),
        ["/project", "config", "key", ..] => {
            bail!("不要在命令中输入模型密钥；请只输入 /project config key，再通过隐藏输入设置")
        }
        [old @ ("/play" | "/history" | "/restore" | "/reload" | "/continue" | "/strategy")] => {
            bail!("旧命令 {old} 已删除；输入 /help 查看新命令")
        }
        _ => bail!("未知或参数不完整的命令；输入 /help 查看用法"),
    }
}

fn parse_alda(words: &[&str]) -> Result<AldaAction> {
    match words {
        ["play"] => Ok(AldaAction::Play(ScoreTarget::Version(None))),
        ["play", "work"] => Ok(AldaAction::Play(ScoreTarget::Working)),
        ["play", version] => Ok(AldaAction::Play(ScoreTarget::Version(Some(parse_version(
            version,
        )?)))),
        ["play", "work", "section", section_id] => Ok(AldaAction::PlaySection {
            target: ScoreTarget::Working,
            section_id: (*section_id).to_string(),
            context_secs: 10,
        }),
        ["play", "current", "section", section_id] => Ok(AldaAction::PlaySection {
            target: ScoreTarget::Version(None),
            section_id: (*section_id).to_string(),
            context_secs: 10,
        }),
        ["stop"] => Ok(AldaAction::Stop),
        ["check"] => Ok(AldaAction::Check(ScoreTarget::Version(None))),
        ["check", "work"] => Ok(AldaAction::Check(ScoreTarget::Working)),
        ["check", "--file", path] => Ok(AldaAction::Check(ScoreTarget::File(PathBuf::from(path)))),
        ["check", version] => Ok(AldaAction::Check(ScoreTarget::Version(Some(
            parse_version(version)?,
        )))),
        ["export", rest @ ..] => parse_export(rest),
        _ => bail!("未知或参数不完整的命令；输入 /help 查看用法"),
    }
}

#[must_use]
pub fn contains_inline_api_key(input: &str) -> bool {
    let words = input.split_whitespace().collect::<Vec<_>>();
    matches!(words.as_slice(), ["/project", "config", "key", _, ..])
}

fn parse_version(value: &str) -> Result<u32> {
    let value = value.strip_prefix('v').unwrap_or(value);
    value
        .parse::<u32>()
        .ok()
        .filter(|version| *version > 0)
        .ok_or_else(|| anyhow::anyhow!("版本必须是正整数，例如 v2"))
}

fn parse_instruments(values: &[&str]) -> Vec<String> {
    if values == ["none"] {
        Vec::new()
    } else {
        values.iter().map(ToString::to_string).collect()
    }
}

fn parse_export(words: &[&str]) -> Result<AldaAction> {
    let mut target = ScoreTarget::Version(None);
    let mut has_target = false;
    let mut format = ExportFormat::All;
    let mut index = 0;
    while index < words.len() {
        if words[index] == "--format" {
            index += 1;
            format = match words.get(index).copied() {
                Some("alda") => ExportFormat::Alda,
                Some("midi") => ExportFormat::Midi,
                Some("wav") => ExportFormat::Wav,
                Some("all") => ExportFormat::All,
                _ => bail!("导出格式必须是 alda、midi、wav 或 all"),
            };
        } else if !has_target {
            target = match words[index] {
                "current" => ScoreTarget::Version(None),
                "work" => ScoreTarget::Working,
                version => ScoreTarget::Version(Some(parse_version(version)?)),
            };
            has_target = true;
        } else {
            bail!("用法：/alda export [current|work|VERSION] [--format alda|midi|wav|all]");
        }
        index += 1;
    }
    Ok(AldaAction::Export { target, format })
}

#[must_use]
pub fn help(path: &[String]) -> String {
    match path.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => "自然语言输入用于讨论、规划和发展工作乐谱。\n/alda ...     校验、播放、停止和导出\n/project ...  接受候选、查看版本和修改设置\n/help ...     查看分层帮助\n/quit         退出".to_string(),
        ["alda"] => "/alda play [VERSION|work]\n/alda stop\n/alda check [VERSION|work]\n/alda check --file PATH\n/alda export [current|work|VERSION] [--format alda|midi|wav|all]".to_string(),
        ["project"] => "/project\n/project instructions\n/project skills [enable|disable QUALIFIED_ID]\n/project accept\n/project discard\n/project versions\n/project switch VERSION\n/project adopt PATH\n/project config ...".to_string(),
        ["project", "skills"] => "/project skills\n/project skills enable user:NAME|project:NAME\n/project skills disable user:NAME|project:NAME".to_string(),
        ["project", "config"] => "/project config\n/project config model NAME\n/project config url URL\n/project config key             # 隐藏输入，不进入历史\n/project config mode full|improv\n/project config duration SECONDS|none\n/project config include INST...|none\n/project config exclude INST...|none".to_string(),
        ["alda", "export"] => "用法：/alda export [current|work|VERSION] [--format alda|midi|wav|all]\n默认一键导出当前版本的 Alda、MIDI 和 WAV。\n示例：/alda export work --format wav".to_string(),
        _ => "没有该帮助主题；输入 /help 查看入口".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_grouped_commands_and_rejects_old_ones() {
        assert_eq!(
            parse("/alda play v2").unwrap(),
            UserAction::Alda(AldaAction::Play(ScoreTarget::Version(Some(2))))
        );
        assert_eq!(
            parse("/project").unwrap(),
            UserAction::Project(ProjectAction::Overview)
        );
        assert_eq!(
            parse("/project accept").unwrap(),
            UserAction::Project(ProjectAction::Accept)
        );
        assert_eq!(
            parse("/project skills enable project:lyric-writing").unwrap(),
            UserAction::Project(ProjectAction::SkillEnable(
                "project:lyric-writing".to_string()
            ))
        );
        assert_eq!(
            parse("/project instructions").unwrap(),
            UserAction::Project(ProjectAction::Instructions)
        );
        assert_eq!(
            parse("/alda play work").unwrap(),
            UserAction::Alda(AldaAction::Play(ScoreTarget::Working))
        );
        assert_eq!(
            parse("/alda play work section climax").unwrap(),
            UserAction::Alda(AldaAction::PlaySection {
                target: ScoreTarget::Working,
                section_id: "climax".to_string(),
                context_secs: 10,
            })
        );
        assert_eq!(
            parse("/alda play current section coda").unwrap(),
            UserAction::Alda(AldaAction::PlaySection {
                target: ScoreTarget::Version(None),
                section_id: "coda".to_string(),
                context_secs: 10,
            })
        );
        assert_eq!(
            parse("/alda export work --format wav").unwrap(),
            UserAction::Alda(AldaAction::Export {
                target: ScoreTarget::Working,
                format: ExportFormat::Wav,
            })
        );
        assert!(parse("/play").unwrap_err().to_string().contains("已删除"));
        assert_eq!(
            parse("/project config model example-model").unwrap(),
            UserAction::Project(ProjectAction::Config(ConfigAction::Model(
                "example-model".to_string()
            )))
        );
        assert_eq!(
            parse("/project config key").unwrap(),
            UserAction::Project(ProjectAction::Config(ConfigAction::ApiKey(None)))
        );
        assert!(parse("/project config key secret").is_err());
        assert!(contains_inline_api_key(" /project config key secret"));
        assert!(!contains_inline_api_key("/project config key"));
        assert_eq!(
            parse("/project config url").unwrap(),
            UserAction::Project(ProjectAction::Config(ConfigAction::PromptUrl))
        );
        assert_eq!(
            parse("/project config model").unwrap(),
            UserAction::Project(ProjectAction::Config(ConfigAction::PromptModel))
        );
    }
}
