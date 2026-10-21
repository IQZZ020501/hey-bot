//! OpenAI 兼容接口客户端。
//!
//! 只依赖 `/v1/chat/completions`，兼容本地部署与各类中转。

use serde::Deserialize;
use serde_json::json;

use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct AiConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub timeout_seconds: u64,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
}

impl AiConfig {
    /// 从环境变量或 `.env` 读取配置。
    ///
    /// 优先级：进程环境变量 > `.env` > 内置默认值。
    pub fn from_env() -> Result<Self> {
        let base_url = crate::config::optional("HEYBOT_AI_BASE_URL")
            .unwrap_or_else(|| "https://api.openai.com/v1".into());
        let api_key = crate::config::require("HEYBOT_AI_API_KEY")?;
        let model = crate::config::require("HEYBOT_AI_MODEL")?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            model,
            timeout_seconds: crate::config::number("HEYBOT_AI_TIMEOUT_SECONDS").unwrap_or(60),
            max_tokens: crate::config::number("HEYBOT_AI_MAX_TOKENS"),
            temperature: crate::config::number("HEYBOT_AI_TEMPERATURE"),
        })
    }
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: Message,
}

#[derive(Debug, Deserialize)]
struct Message {
    #[serde(default)]
    content: String,
}

/// 一次对话请求。
#[derive(Debug, Clone)]
pub struct AiClient {
    client: reqwest::Client,
    config: AiConfig,
}

impl AiClient {
    pub fn new(config: AiConfig) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_seconds))
            .build()
            .map_err(|error| Error::Ai(error.to_string()))?;
        Ok(Self { client, config })
    }

    /// 单轮对话。
    pub async fn complete(&self, system: &str, user: &str) -> Result<String> {
        let mut body = json!({
            "model": self.config.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
        });
        if let Some(max_tokens) = self.config.max_tokens {
            body["max_tokens"] = json!(max_tokens);
        }
        if let Some(temperature) = self.config.temperature {
            body["temperature"] = json!(temperature);
        }

        let response = self
            .client
            .post(format!("{}/chat/completions", self.config.base_url))
            .bearer_auth(&self.config.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|error| Error::Ai(error.to_string()))?;

        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|error| Error::Ai(error.to_string()))?;
        if !status.is_success() {
            return Err(Error::Ai(format!("AI 接口返回 HTTP {status}：{text}")));
        }

        let parsed: ChatResponse =
            serde_json::from_str(&text).map_err(|error| Error::Ai(error.to_string()))?;
        let content = parsed
            .choices
            .first()
            .map(|choice| choice.message.content.trim().to_string())
            .ok_or_else(|| Error::Ai("AI 接口没有返回内容。".into()))?;
        if content.is_empty() {
            return Err(Error::Ai("AI 接口返回了空内容。".into()));
        }
        Ok(content)
    }
}
