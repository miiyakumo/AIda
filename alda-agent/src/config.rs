use crate::deepseek::ThinkingOptions;
use std::fs;
use std::path::Path;

#[derive(Debug)]
pub struct Config {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub thinking: ThinkingOptions,
}

impl Config {
    /// 从 `../.env` 读取配置。缺失字段返回错误。
    pub fn from_env_file() -> anyhow::Result<Self> {
        let env_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join(".env");
        Self::from_file(&env_path)
    }

    pub fn from_file(path: &Path) -> anyhow::Result<Self> {
        let content = fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("无法读取 .env 文件 ({}): {}", path.display(), e))?;

        let mut api_key = None;
        let mut base_url = None;
        let mut model = None;
        let mut thinking = None;
        let mut reasoning_effort = None;

        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                let key = key.trim();
                let value = value.trim().trim_matches('"').trim_matches('\'');
                match key {
                    "ALDA_AGENT_API_KEY" => api_key = Some(value.to_string()),
                    "ALDA_AGENT_BASE_URL" => base_url = Some(value.to_string()),
                    "ALDA_AGENT_MODEL" => model = Some(value.to_string()),
                    "ALDA_AGENT_THINKING" => thinking = Some(value.to_string()),
                    "ALDA_AGENT_REASONING_EFFORT" => reasoning_effort = Some(value.to_string()),
                    _ => {}
                }
            }
        }

        let required = |value: Option<String>, key: &str| {
            value
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!(".env 中缺少或未填写 {key}"))
        };

        let thinking =
            ThinkingOptions::from_config(thinking.as_deref(), reasoning_effort.as_deref())?;

        Ok(Config {
            api_key: required(api_key, "ALDA_AGENT_API_KEY")?,
            base_url: required(base_url, "ALDA_AGENT_BASE_URL")?,
            model: required(model, "ALDA_AGENT_MODEL")?,
            thinking,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_fields_are_required() {
        let dir = tempfile::tempdir().unwrap();
        let env_path = dir.path().join(".env");
        fs::write(&env_path, "ALDA_AGENT_API_KEY=sk-test-key\n").unwrap();
        let error = Config::from_file(&env_path).unwrap_err();
        assert!(error.to_string().contains("ALDA_AGENT_BASE_URL"));
    }

    #[test]
    fn test_parse_full_config() {
        let dir = tempfile::tempdir().unwrap();
        let env_path = dir.path().join(".env");
        fs::write(
            &env_path,
            "ALDA_AGENT_API_KEY=sk-abc\nALDA_AGENT_BASE_URL=https://api.example.com\nALDA_AGENT_MODEL=example-model\nALDA_AGENT_THINKING=enabled\nALDA_AGENT_REASONING_EFFORT=low\n",
        )
        .unwrap();
        let config = Config::from_file(&env_path).unwrap();
        assert_eq!(config.api_key, "sk-abc");
        assert_eq!(config.base_url, "https://api.example.com");
        assert_eq!(config.model, "example-model");
        assert_eq!(config.thinking.mode(), "enabled");
        assert_eq!(config.thinking.reasoning_effort(), Some("low"));
    }

    #[test]
    fn test_parse_with_quotes_and_comments() {
        let dir = tempfile::tempdir().unwrap();
        let env_path = dir.path().join(".env");
        fs::write(
            &env_path,
            "# 注释行\nALDA_AGENT_API_KEY=\"sk-quoted\"\n\nALDA_AGENT_BASE_URL='https://api.quoted.com'\nALDA_AGENT_MODEL='example-model'\n",
        )
        .unwrap();
        let config = Config::from_file(&env_path).unwrap();
        assert_eq!(config.api_key, "sk-quoted");
        assert_eq!(config.base_url, "https://api.quoted.com");
        assert_eq!(config.model, "example-model");
        assert_eq!(config.thinking.mode(), "disabled");
        assert_eq!(config.thinking.reasoning_effort(), None);
    }

    #[test]
    fn test_missing_api_key() {
        let dir = tempfile::tempdir().unwrap();
        let env_path = dir.path().join(".env");
        fs::write(
            &env_path,
            "ALDA_AGENT_BASE_URL=https://api.example.com\nALDA_AGENT_MODEL=example-model\n",
        )
        .unwrap();
        let result = Config::from_file(&env_path);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("ALDA_AGENT_API_KEY")
        );
    }

    #[test]
    fn test_empty_model_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let env_path = dir.path().join(".env");
        fs::write(
            &env_path,
            "ALDA_AGENT_API_KEY=test-key\nALDA_AGENT_BASE_URL=https://api.example.com\nALDA_AGENT_MODEL=\n",
        )
        .unwrap();

        let error = Config::from_file(&env_path).unwrap_err();
        assert!(error.to_string().contains("ALDA_AGENT_MODEL"));
    }

    #[test]
    fn legacy_aliases_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let env_path = dir.path().join(".env");
        fs::write(
            &env_path,
            "api-key=test-key\nbase_url=https://api.example.com\nmodel=example-model\n",
        )
        .unwrap();

        let error = Config::from_file(&env_path).unwrap_err();
        assert!(error.to_string().contains("ALDA_AGENT_API_KEY"));
    }

    #[test]
    fn invalid_thinking_settings_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let env_path = dir.path().join(".env");
        let required = "ALDA_AGENT_API_KEY=test-key\nALDA_AGENT_BASE_URL=https://api.example.com\nALDA_AGENT_MODEL=example-model\n";

        fs::write(&env_path, format!("{required}ALDA_AGENT_THINKING=maybe\n")).unwrap();
        assert!(
            Config::from_file(&env_path)
                .unwrap_err()
                .to_string()
                .contains("ALDA_AGENT_THINKING")
        );

        fs::write(
            &env_path,
            format!("{required}ALDA_AGENT_THINKING=disabled\nALDA_AGENT_REASONING_EFFORT=low\n"),
        )
        .unwrap();
        assert!(
            Config::from_file(&env_path)
                .unwrap_err()
                .to_string()
                .contains("不能设置")
        );

        fs::write(
            &env_path,
            format!("{required}ALDA_AGENT_THINKING=enabled\nALDA_AGENT_REASONING_EFFORT=extreme\n"),
        )
        .unwrap();
        assert!(
            Config::from_file(&env_path)
                .unwrap_err()
                .to_string()
                .contains("ALDA_AGENT_REASONING_EFFORT")
        );
    }
}
