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
    Play(Option<u32>),
    Stop,
    Check(ScoreTarget),
    Export {
        version: Option<u32>,
        format: ExportFormat,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScoreTarget {
    Version(Option<u32>),
    File(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Alda,
    Midi,
    All,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProjectAction {
    Overview,
    Versions,
    Switch(u32),
    Adopt(PathBuf),
    Config(ConfigAction),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConfigAction {
    Show,
    Mode(String),
    Duration(Option<f64>),
    Include(Vec<String>),
    Exclude(Vec<String>),
    Strategy(Option<String>),
    Model(String),
    Url(String),
    ApiKey(Option<String>),
}

pub const TOP_LEVEL_COMMANDS: &[&str] = &["/alda", "/project", "/help", "/quit"];
pub const ALDA_COMMANDS: &[&str] = &["play", "stop", "check", "export"];
pub const PROJECT_COMMANDS: &[&str] = &["versions", "switch", "adopt", "config"];
pub const CONFIG_COMMANDS: &[&str] = &[
    "model", "url", "key", "mode", "duration", "include", "exclude", "strategy",
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
    match words.as_slice() {
        ["/quit"] => Ok(UserAction::Quit),
        ["/help", rest @ ..] => Ok(UserAction::Help(
            rest.iter().map(ToString::to_string).collect(),
        )),
        ["/project"] => Ok(UserAction::Project(ProjectAction::Overview)),
        ["/project", "versions"] => Ok(UserAction::Project(ProjectAction::Versions)),
        ["/project", "switch", version] => Ok(UserAction::Project(ProjectAction::Switch(
            parse_version(version)?,
        ))),
        ["/project", "adopt", path] => Ok(UserAction::Project(ProjectAction::Adopt(
            PathBuf::from(path),
        ))),
        ["/project", "config"] => Ok(UserAction::Project(ProjectAction::Config(
            ConfigAction::Show,
        ))),
        ["/project", "config", "url"] => {
            bail!("缺少 API Base URL；用法：/project config url https://api.example.com")
        }
        ["/project", "config", "model"] => {
            bail!("缺少模型名称；用法：/project config model MODEL_NAME")
        }
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
        ["/project", "config", "strategy", "default"] => Ok(UserAction::Project(
            ProjectAction::Config(ConfigAction::Strategy(None)),
        )),
        ["/project", "config", "strategy", strategy @ ..] if !strategy.is_empty() => {
            Ok(UserAction::Project(ProjectAction::Config(
                ConfigAction::Strategy(Some(strategy.join(" "))),
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
        ["/alda", "play"] => Ok(UserAction::Alda(AldaAction::Play(None))),
        ["/alda", "play", version] => Ok(UserAction::Alda(AldaAction::Play(Some(parse_version(
            version,
        )?)))),
        ["/alda", "stop"] => Ok(UserAction::Alda(AldaAction::Stop)),
        ["/alda", "check"] => Ok(UserAction::Alda(AldaAction::Check(ScoreTarget::Version(
            None,
        )))),
        ["/alda", "check", "--file", path] => Ok(UserAction::Alda(AldaAction::Check(
            ScoreTarget::File(PathBuf::from(path)),
        ))),
        ["/alda", "check", version] => Ok(UserAction::Alda(AldaAction::Check(
            ScoreTarget::Version(Some(parse_version(version)?)),
        ))),
        ["/alda", "export", rest @ ..] => parse_export(rest).map(UserAction::Alda),
        [old @ ("/play" | "/history" | "/restore" | "/reload" | "/continue" | "/strategy")] => {
            bail!("旧命令 {old} 已删除；输入 /help 查看新命令")
        }
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
    let mut version = None;
    let mut format = ExportFormat::All;
    let mut index = 0;
    while index < words.len() {
        if words[index] == "--format" {
            index += 1;
            format = match words.get(index).copied() {
                Some("alda") => ExportFormat::Alda,
                Some("midi") => ExportFormat::Midi,
                Some("all") => ExportFormat::All,
                _ => bail!("导出格式必须是 alda、midi 或 all"),
            };
        } else if version.is_none() {
            version = Some(parse_version(words[index])?);
        } else {
            bail!("用法：/alda export [VERSION] [--format alda|midi|all]");
        }
        index += 1;
    }
    Ok(AldaAction::Export { version, format })
}

#[must_use]
pub fn help(path: &[String]) -> String {
    match path.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => "自然语言输入用于创作、修改或回答澄清。\n/alda ...     校验、播放、停止和导出\n/project ...  查看版本和修改项目设置\n/help ...     查看分层帮助\n/quit         退出".to_string(),
        ["alda"] => "/alda play [VERSION]\n/alda stop\n/alda check [VERSION]\n/alda check --file PATH\n/alda export [VERSION] [--format alda|midi|all]".to_string(),
        ["project"] => "/project\n/project versions\n/project switch VERSION\n/project adopt PATH\n/project config ...".to_string(),
        ["project", "config"] => "/project config\n/project config model NAME\n/project config url URL\n/project config key             # 隐藏输入，不进入历史\n/project config mode full|improv\n/project config duration SECONDS|none\n/project config include INST...|none\n/project config exclude INST...|none\n/project config strategy TEXT|default".to_string(),
        ["alda", "export"] => "用法：/alda export [VERSION] [--format alda|midi|all]\n默认导出当前版本的 Alda 和 MIDI。\n示例：/alda export v2 --format midi".to_string(),
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
            UserAction::Alda(AldaAction::Play(Some(2)))
        );
        assert_eq!(
            parse("/project").unwrap(),
            UserAction::Project(ProjectAction::Overview)
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
        assert!(
            parse("/project config url")
                .unwrap_err()
                .to_string()
                .contains("API Base URL")
        );
    }
}
