use std::fs;
use std::path::Path;

#[derive(Debug)]
pub struct Config {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
}

impl Config {
    /// 从 `../.env` 读取配置。缺失字段返回错误。
    pub fn from_env_file() -> anyhow::Result<Self> {
        let env_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join(".env");
        Self::from_file(&env_path)
    }

    fn from_file(path: &Path) -> anyhow::Result<Self> {
        let content = fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("无法读取 .env 文件 ({}): {}", path.display(), e))?;

        let mut api_key = None;
        let mut base_url = None;
        let mut model = None;

        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                let key = key.trim();
                let value = value.trim().trim_matches('"').trim_matches('\'');
                match key {
                    "ALDA_AGENT_API_KEY" | "api-key" => api_key = Some(value.to_string()),
                    "ALDA_AGENT_BASE_URL" | "base_url" => base_url = Some(value.to_string()),
                    "ALDA_AGENT_MODEL" | "model" => model = Some(value.to_string()),
                    _ => {}
                }
            }
        }

        Ok(Config {
            api_key: api_key.ok_or_else(|| anyhow::anyhow!(".env 中缺少 ALDA_AGENT_API_KEY"))?,
            base_url: base_url.unwrap_or_else(|| "https://api.deepseek.com".to_string()),
            model: model.unwrap_or_else(|| "deepseek-chat".to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_config() {
        let dir = tempfile::tempdir().unwrap();
        let env_path = dir.path().join(".env");
        fs::write(&env_path, "ALDA_AGENT_API_KEY=sk-test-key\n").unwrap();
        let config = Config::from_file(&env_path).unwrap();
        assert_eq!(config.api_key, "sk-test-key");
        assert_eq!(config.base_url, "https://api.deepseek.com");
        assert_eq!(config.model, "deepseek-chat");
    }

    #[test]
    fn test_parse_full_config() {
        let dir = tempfile::tempdir().unwrap();
        let env_path = dir.path().join(".env");
        fs::write(
            &env_path,
            "ALDA_AGENT_API_KEY=sk-abc\nALDA_AGENT_BASE_URL=https://api.example.com\nALDA_AGENT_MODEL=example-model\n",
        )
        .unwrap();
        let config = Config::from_file(&env_path).unwrap();
        assert_eq!(config.api_key, "sk-abc");
        assert_eq!(config.base_url, "https://api.example.com");
        assert_eq!(config.model, "example-model");
    }

    #[test]
    fn test_parse_with_quotes_and_comments() {
        let dir = tempfile::tempdir().unwrap();
        let env_path = dir.path().join(".env");
        fs::write(
            &env_path,
            "# 注释行\nALDA_AGENT_API_KEY=\"sk-quoted\"\n\nALDA_AGENT_BASE_URL='https://api.quoted.com'\n",
        )
        .unwrap();
        let config = Config::from_file(&env_path).unwrap();
        assert_eq!(config.api_key, "sk-quoted");
        assert_eq!(config.base_url, "https://api.quoted.com");
    }

    #[test]
    fn test_missing_api_key() {
        let dir = tempfile::tempdir().unwrap();
        let env_path = dir.path().join(".env");
        fs::write(&env_path, "ALDA_AGENT_BASE_URL=https://api.example.com\n").unwrap();
        let result = Config::from_file(&env_path);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("ALDA_AGENT_API_KEY")
        );
    }
}
