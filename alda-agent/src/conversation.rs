use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Conversation {
    messages: Vec<ConversationMessage>,
    state: ConversationState,
    #[serde(default, skip_serializing_if = "is_false")]
    pending_candidate: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !value
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationMessage {
    pub role: ConversationRole,
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ConversationToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationRole {
    User,
    Assistant,
    System,
    Tool,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationState {
    #[default]
    Ready,
    AwaitingInput,
    RevisionAvailable,
    RequestPending,
}

impl Conversation {
    #[must_use]
    pub fn messages(&self) -> &[ConversationMessage] {
        &self.messages
    }

    #[must_use]
    pub fn state(&self) -> ConversationState {
        self.state
    }

    #[must_use]
    pub fn pending_candidate(&self) -> bool {
        self.pending_candidate
    }

    #[must_use]
    pub fn first_request(&self) -> Option<&str> {
        self.messages
            .iter()
            .find(|message| message.role == ConversationRole::User)
            .and_then(|message| message.content.as_deref())
    }

    #[must_use]
    pub fn last_user_message(&self) -> Option<&str> {
        self.messages
            .iter()
            .rev()
            .find(|message| message.role == ConversationRole::User)
            .and_then(|message| message.content.as_deref())
    }

    pub fn add_user_message(&mut self, content: String) {
        self.messages.push(ConversationMessage {
            role: ConversationRole::User,
            content: Some(content),
            tool_calls: Vec::new(),
            tool_call_id: None,
        });
    }

    pub fn add_assistant_message(&mut self, content: String) {
        if !content.trim().is_empty() {
            self.messages.push(ConversationMessage {
                role: ConversationRole::Assistant,
                content: Some(content),
                tool_calls: Vec::new(),
                tool_call_id: None,
            });
        }
    }

    pub fn replace_messages(&mut self, messages: Vec<ConversationMessage>) {
        self.messages = messages;
    }

    /// Remove provider protocol traces from projects written by older builds.
    ///
    /// Tool calls and tool results are useful only while one model request is
    /// running. Durable project history keeps user messages and successful
    /// semantic `submit_result` messages instead of replaying raw arguments,
    /// failed candidates and model scratch text on every later request.
    pub fn compact_provider_trace(&mut self) -> bool {
        let has_trace = self.messages.iter().any(|message| {
            matches!(
                message.role,
                ConversationRole::System | ConversationRole::Tool
            ) || !message.tool_calls.is_empty()
                || message.tool_call_id.is_some()
        });
        if !has_trace {
            return false;
        }

        let accepted_tool_ids = self
            .messages
            .iter()
            .filter(|message| message.role == ConversationRole::Tool)
            .filter(|message| {
                message.content.as_deref().is_some_and(|content| {
                    content.contains("宿主已接收文本结果") || content.contains("所有检查通过")
                })
            })
            .filter_map(|message| message.tool_call_id.as_deref())
            .collect::<std::collections::HashSet<_>>();
        let mut compacted = Vec::new();
        for message in &self.messages {
            if message.role == ConversationRole::User {
                if let Some(content) = message.content.as_deref().filter(|value| !value.is_empty())
                {
                    compacted.push(ConversationMessage::semantic(
                        ConversationRole::User,
                        content.to_string(),
                    ));
                }
                continue;
            }
            if message.role != ConversationRole::Assistant {
                continue;
            }
            for call in &message.tool_calls {
                if call.name != "submit_result" || !accepted_tool_ids.contains(call.id.as_str()) {
                    continue;
                }
                if let Some(content) = submitted_message(&call.arguments) {
                    compacted.push(ConversationMessage::semantic(
                        ConversationRole::Assistant,
                        content,
                    ));
                }
            }
        }
        self.messages = compacted;
        true
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        for message in &self.messages {
            if message.content.as_deref().is_none_or(str::is_empty) && message.tool_calls.is_empty()
            {
                anyhow::bail!("project.json 损坏：对话消息缺少内容");
            }
        }
        Ok(())
    }

    pub fn set_state(&mut self, state: ConversationState) {
        self.state = state;
        if !matches!(
            state,
            ConversationState::AwaitingInput
                | ConversationState::RevisionAvailable
                | ConversationState::RequestPending
        ) {
            self.pending_candidate = false;
        }
    }

    pub fn set_pending_candidate(&mut self, pending_candidate: bool) {
        self.pending_candidate = pending_candidate;
    }
}

impl ConversationMessage {
    fn semantic(role: ConversationRole, content: String) -> Self {
        Self {
            role,
            content: Some(content),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
}

fn submitted_message(arguments: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()?
        .get("message")?
        .as_str()
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_trace_compacts_to_semantic_messages() {
        let mut conversation = Conversation {
            messages: vec![
                ConversationMessage::semantic(ConversationRole::User, "写一首曲子".into()),
                ConversationMessage::semantic(ConversationRole::Assistant, "大段内部推演".into()),
                ConversationMessage {
                    role: ConversationRole::Assistant,
                    content: Some("更多推演".into()),
                    tool_calls: vec![ConversationToolCall {
                        id: "call-1".into(),
                        name: "submit_result".into(),
                        arguments: serde_json::json!({
                            "kind": "candidate",
                            "message": "完整候选已生成",
                            "alda_code": "piano: c"
                        })
                        .to_string(),
                    }],
                    tool_call_id: None,
                },
                ConversationMessage {
                    role: ConversationRole::Tool,
                    content: Some("✅ 所有检查通过".into()),
                    tool_calls: Vec::new(),
                    tool_call_id: Some("call-1".into()),
                },
            ],
            state: ConversationState::Ready,
            pending_candidate: false,
        };

        assert!(conversation.compact_provider_trace());
        assert_eq!(conversation.messages.len(), 2);
        assert_eq!(
            conversation.messages[1].content.as_deref(),
            Some("完整候选已生成")
        );
        assert!(
            conversation
                .messages
                .iter()
                .all(|message| { message.tool_calls.is_empty() && message.tool_call_id.is_none() })
        );
    }

    #[test]
    fn semantic_history_is_not_rewritten() {
        let mut conversation = Conversation::default();
        conversation.add_user_message("素材".into());
        conversation.add_assistant_message("已收到".into());
        assert!(!conversation.compact_provider_trace());
        assert_eq!(conversation.messages.len(), 2);
    }
}
