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
            ConversationState::AwaitingInput | ConversationState::RequestPending
        ) {
            self.pending_candidate = false;
        }
    }

    pub fn set_pending_candidate(&mut self, pending_candidate: bool) {
        self.pending_candidate = pending_candidate;
    }
}
