use alda_agent::deepseek::{
    DeepSeekClient, FunctionCallArgs, FunctionDef, Message, StreamEvent, Tool, ToolCallMsg,
};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct RoleStats {
    pub model_calls: usize,
    pub protocol_recoveries: usize,
    pub revisions: usize,
}

pub struct RoleSession {
    client: DeepSeekClient,
    messages: Vec<Message>,
    tool: Tool,
    tool_name: String,
    stats: RoleStats,
}

impl RoleSession {
    pub fn new(
        client: DeepSeekClient,
        system: impl Into<String>,
        request: impl Into<String>,
        tool_name: &str,
        description: &str,
        schema: serde_json::Value,
    ) -> Self {
        Self {
            client,
            messages: vec![
                message("system", system.into()),
                message("user", request.into()),
            ],
            tool: Tool {
                ty: "function".to_string(),
                function: FunctionDef {
                    name: tool_name.to_string(),
                    description: description.to_string(),
                    parameters: schema,
                },
            },
            tool_name: tool_name.to_string(),
            stats: RoleStats::default(),
        }
    }

    pub const fn stats(&self) -> RoleStats {
        self.stats
    }

    pub fn feedback(&mut self, feedback: impl Into<String>) {
        self.stats.revisions += 1;
        self.messages.push(message("user", feedback.into()));
    }

    pub async fn submit<T>(&mut self, validate: impl Fn(&T) -> Result<()>) -> Result<T>
    where
        T: DeserializeOwned,
    {
        loop {
            self.stats.model_calls += 1;
            let events = self
                .client
                .chat_stream(self.messages.clone(), Some(vec![self.tool.clone()]))
                .await?;
            match decode_submission(&events, &self.tool_name).and_then(|(call, value)| {
                let parsed =
                    serde_json::from_str::<T>(&value).context("工具参数不符合强类型 JSON 协议")?;
                validate(&parsed)?;
                Ok((call, parsed))
            }) {
                Ok((call, parsed)) => {
                    let tool_call_id = call
                        .tool_calls
                        .as_ref()
                        .and_then(|calls| calls.first())
                        .map(|call| call.id.clone())
                        .expect("decoded submission contains one tool call");
                    self.messages.push(call);
                    self.messages.push(Message {
                        role: "tool".to_string(),
                        content: Some("宿主已接收本次提交。".to_string()),
                        tool_calls: None,
                        tool_call_id: Some(tool_call_id),
                    });
                    return Ok(parsed);
                }
                Err(error) if self.stats.protocol_recoveries == 0 => {
                    self.stats.protocol_recoveries = 1;
                    self.messages.push(message(
                        "user",
                        format!(
                            "上次提交未通过宿主协议：{error:#}。请只调用 {} 一次并提交完整修正 JSON。",
                            self.tool_name
                        ),
                    ));
                }
                Err(error) => return Err(error.context("角色协议恢复后仍无效")),
            }
        }
    }
}

fn decode_submission(events: &[StreamEvent], expected_name: &str) -> Result<(Message, String)> {
    let calls = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ToolCall {
                id,
                name,
                arguments,
            } => Some((id, name, arguments)),
            _ => None,
        })
        .collect::<Vec<_>>();
    if calls.len() != 1 {
        bail!("角色必须且只能调用一个提交工具，实际 {} 个", calls.len());
    }
    let (id, name, arguments) = calls[0];
    if name != expected_name {
        bail!("角色调用了未授权工具 {name:?}");
    }
    let id = id.clone().unwrap_or_else(|| "role_submission".to_string());
    Ok((
        Message {
            role: "assistant".to_string(),
            content: None,
            tool_calls: Some(vec![ToolCallMsg {
                id,
                ty: "function".to_string(),
                function: FunctionCallArgs {
                    name: name.clone(),
                    arguments: arguments.clone(),
                },
            }]),
            tool_call_id: None,
        },
        arguments.clone(),
    ))
}

fn message(role: &str, content: String) -> Message {
    Message {
        role: role.to_string(),
        content: Some(content),
        tool_calls: None,
        tool_call_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactly_one_expected_tool_is_required() {
        let no_calls = vec![StreamEvent::Done {
            finish_reason: "stop".to_string(),
        }];
        assert!(decode_submission(&no_calls, "submit").is_err());
        let wrong = vec![StreamEvent::ToolCall {
            id: Some("x".to_string()),
            name: "other".to_string(),
            arguments: "{}".to_string(),
        }];
        assert!(decode_submission(&wrong, "submit").is_err());
    }
}
