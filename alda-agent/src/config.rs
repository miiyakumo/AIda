use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

const MODEL_CONFIG_FILE: &str = "model.json";

#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ModelConfig {
    model: String,
    base_url: String,
    api_key: String,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedModelConfig {
    pub model: String,
    pub base_url: String,
    pub api_key: String,
}

impl std::fmt::Debug for ModelConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ModelConfig")
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .field("api_key", &self.has_api_key().then_some("<redacted>"))
            .finish()
    }
}

impl std::fmt::Debug for ResolvedModelConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolvedModelConfig")
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

impl ModelConfig {
    pub fn load(project_root: &Path) -> Result<Self> {
        let path = project_root.join(MODEL_CONFIG_FILE);
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = fs::read_to_string(&path)
            .with_context(|| format!("无法读取项目模型配置 {}", path.display()))?;
        let config: Self = serde_json::from_str(&content)
            .with_context(|| format!("无法解析项目模型配置 {}", path.display()))?;
        config.validate_present_values()?;
        Ok(config)
    }

    pub fn save(&self, project_root: &Path) -> Result<()> {
        self.validate_present_values()?;
        let contents = serde_json::to_vec_pretty(self).context("无法序列化项目模型配置")?;
        write_private_atomic(&project_root.join(MODEL_CONFIG_FILE), &contents)
    }

    pub fn set_model(&mut self, value: &str) -> Result<()> {
        self.model = required(value, "模型名称")?;
        Ok(())
    }

    pub fn set_base_url(&mut self, value: &str) -> Result<()> {
        let value = required(value, "模型 URL")?;
        validate_base_url(&value)?;
        self.base_url = value;
        Ok(())
    }

    pub fn set_api_key(&mut self, value: &str) -> Result<()> {
        self.api_key = required(value, "模型密钥")?;
        Ok(())
    }

    #[must_use]
    pub fn model(&self) -> Option<&str> {
        (!self.model.is_empty()).then_some(self.model.as_str())
    }

    #[must_use]
    pub fn base_url(&self) -> Option<&str> {
        (!self.base_url.is_empty()).then_some(self.base_url.as_str())
    }

    #[must_use]
    pub fn has_api_key(&self) -> bool {
        !self.api_key.is_empty()
    }

    pub fn resolve(&self) -> Result<ResolvedModelConfig> {
        let mut missing = Vec::new();
        if self.model.is_empty() {
            missing.push("model");
        }
        if self.base_url.is_empty() {
            missing.push("url");
        }
        if self.api_key.is_empty() {
            missing.push("key");
        }
        if !missing.is_empty() {
            bail!("项目模型配置不完整：缺少 {}", missing.join("、"));
        }
        Ok(ResolvedModelConfig {
            model: self.model.clone(),
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
        })
    }

    fn validate_present_values(&self) -> Result<()> {
        if !self.base_url.is_empty() {
            validate_base_url(&self.base_url)?;
        }
        if self.model.trim() != self.model || self.api_key.trim() != self.api_key {
            bail!("项目模型配置包含未规范化的首尾空白");
        }
        Ok(())
    }
}

fn required(value: &str, name: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        bail!("{name}不能为空");
    }
    Ok(value.to_string())
}

fn validate_base_url(value: &str) -> Result<()> {
    let url = reqwest::Url::parse(value).context("模型 URL 无效")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("模型 URL 必须是包含主机名的 http 或 https 地址");
    }
    Ok(())
}

fn write_private_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("无效文件路径：{}", path.display()))?
        .to_string_lossy();
    let temporary = path.with_file_name(format!(".{file_name}.tmp-{}", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options
            .open(&temporary)
            .with_context(|| format!("无法创建临时配置 {}", temporary.display()))?;
        file.write_all(contents).context("无法写入项目模型配置")?;
        file.sync_all().context("无法同步项目模型配置")?;
        fs::rename(&temporary, path)
            .with_context(|| format!("无法更新项目模型配置 {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_configuration_persists_and_resolves_only_when_complete() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = ModelConfig::default();
        config.set_model("example-model").unwrap();
        config.save(directory.path()).unwrap();
        assert!(
            config
                .resolve()
                .unwrap_err()
                .to_string()
                .contains("url、key")
        );

        let mut loaded = ModelConfig::load(directory.path()).unwrap();
        loaded.set_base_url("https://api.example.com/v1").unwrap();
        loaded.set_api_key("secret-test-value").unwrap();
        loaded.save(directory.path()).unwrap();

        let resolved = ModelConfig::load(directory.path())
            .unwrap()
            .resolve()
            .unwrap();
        assert_eq!(resolved.model, "example-model");
        assert_eq!(resolved.base_url, "https://api.example.com/v1");
        assert_eq!(resolved.api_key, "secret-test-value");
    }

    #[test]
    fn invalid_urls_are_rejected_without_changing_config() {
        let mut config = ModelConfig::default();
        assert!(config.set_base_url("file:///tmp/model").is_err());
        assert_eq!(config.base_url(), None);
    }

    #[cfg(unix)]
    #[test]
    fn persisted_model_config_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let mut config = ModelConfig::default();
        config.set_api_key("secret-test-value").unwrap();
        config.save(directory.path()).unwrap();
        let mode = fs::metadata(directory.path().join(MODEL_CONFIG_FILE))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn debug_output_redacts_api_key() {
        let mut config = ModelConfig::default();
        config.set_model("example-model").unwrap();
        config.set_base_url("https://api.example.com").unwrap();
        config.set_api_key("secret-test-value").unwrap();

        let unresolved_debug = format!("{config:?}");
        let resolved_debug = format!("{:?}", config.resolve().unwrap());
        assert!(!unresolved_debug.contains("secret-test-value"));
        assert!(!resolved_debug.contains("secret-test-value"));
        assert!(unresolved_debug.contains("<redacted>"));
        assert!(resolved_debug.contains("<redacted>"));
    }
}
