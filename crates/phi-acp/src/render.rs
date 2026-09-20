//! ref 标签渲染 —— 对应 acp-kernel `src/render-refs.ts`。
//!
//! 在消息文本前注入 `<acp tokens=".." type="..">mNNNNN</acp>` 标签，供模型在
//! `compress` 调用里引用。token 数写入 `tokenSnapshot` 后固定不变，避免前缀缓存
//! 抖动。

use crate::refs::BLOCKED_REF;
use crate::tokenize::thinking_token_value;
use crate::types::{CompressionState, ContentType, CoreMessage};

/// 渲染策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RenderStrategy {
    /// 给所有已映射消息打标签。
    #[default]
    All,
    /// 只给用户 / 助手文本打标签。
    TextOnly,
    /// 完全不改文本。
    None,
}

fn format_tokens(tokens: u64) -> String {
    if tokens < 1000 {
        tokens.to_string()
    } else if tokens < 10_000 {
        format!("{:.1}K", tokens as f64 / 1000.0)
    } else {
        format!("{}K", (tokens as f64 / 1000.0).round() as u64)
    }
}

fn classify_type(message: &CoreMessage) -> String {
    match message.content_type {
        ContentType::ToolCall | ContentType::ToolResult => message
            .tool_name
            .clone()
            .unwrap_or_else(|| "tool".to_string()),
        ContentType::Text => "text".to_string(),
        ContentType::Reasoning => "reasoning".to_string(),
    }
}

fn acp_tag(reference: &str, tokens: u64, ty: &str) -> String {
    format!(
        "<acp tokens=\"{}\" type=\"{ty}\">{reference}</acp>",
        format_tokens(tokens)
    )
}

/// 去掉消息自带的旧标签（幂等渲染）。
fn strip_own_tag(text: &str, reference: &str) -> String {
    let open = "<acp ";
    let close = "</acp>";
    if let Some(start) = text.find(open) {
        if start == 0 {
            if let Some(rel_end) = text.find(close) {
                let end = rel_end + close.len();
                let tag = &text[..end];
                if tag.contains(reference) {
                    let rest = &text[end..];
                    return rest.strip_prefix('\n').unwrap_or(rest).to_string();
                }
            }
        }
    }
    text.to_string()
}

fn render_message(
    message: &CoreMessage,
    state: &CompressionState,
    snapshot: &mut std::collections::BTreeMap<String, u64>,
    strategy: RenderStrategy,
    count: &dyn Fn(&str) -> u64,
) -> CoreMessage {
    let Some(reference) = state.message_refs.by_raw.get(&message.id) else {
        return message.clone();
    };
    if reference == BLOCKED_REF || strategy == RenderStrategy::None {
        return message.clone();
    }
    if strategy == RenderStrategy::TextOnly && message.content_type != ContentType::Text {
        return message.clone();
    }

    let clean = strip_own_tag(message.text_str(), reference);
    let text_tokens = *snapshot
        .entry(reference.clone())
        .or_insert_with(|| count(&clean));
    let tokens = text_tokens + thinking_token_value(message.thinking_tokens);
    let prefix = format!("{}\n", acp_tag(reference, tokens, &classify_type(message)));
    let text = if clean.is_empty() {
        prefix
    } else {
        format!("{prefix}{clean}")
    };
    CoreMessage {
        text: Some(text),
        ..message.clone()
    }
}

/// 渲染可见 ref 标签（实时模式，每次重算 token）。
pub fn render_visible_refs(
    messages: &[CoreMessage],
    state: &CompressionState,
    strategy: RenderStrategy,
) -> Vec<CoreMessage> {
    let mut snapshot = state.token_snapshot.clone();
    messages
        .iter()
        .map(|m| {
            render_message(
                m,
                state,
                &mut snapshot,
                strategy,
                &crate::tokenize::count_tokens,
            )
        })
        .collect()
}

/// 渲染并返回快照（首次写入后固定）。
pub fn render_with_snapshot(
    messages: &[CoreMessage],
    state: &CompressionState,
    strategy: RenderStrategy,
) -> (Vec<CoreMessage>, std::collections::BTreeMap<String, u64>) {
    let mut snapshot = state.token_snapshot.clone();
    let rendered = messages
        .iter()
        .map(|m| {
            render_message(
                m,
                state,
                &mut snapshot,
                strategy,
                &crate::tokenize::count_tokens,
            )
        })
        .collect();
    (rendered, snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refs::{assign_refs, AssignRefsOptions};

    fn state_with_refs(messages: &[CoreMessage]) -> CompressionState {
        let mut state = crate::state::create_initial_state();
        let existing = state.message_refs.clone();
        let result = assign_refs(
            messages,
            AssignRefsOptions {
                existing: &existing,
                next_index: 1,
                is_protected: None,
            },
        );
        state.message_refs = result.map;
        state
    }

    #[test]
    fn render_should_prefix_tag() {
        let messages = vec![CoreMessage::text("a", crate::types::Role::User, "hello")];
        let state = state_with_refs(&messages);
        let rendered = render_visible_refs(&messages, &state, RenderStrategy::All);
        assert!(rendered[0].text_str().starts_with("<acp "));
        assert!(rendered[0].text_str().contains("m00001"));
        assert!(rendered[0].text_str().ends_with("hello"));
    }

    #[test]
    fn render_should_be_idempotent() {
        let messages = vec![CoreMessage::text("a", crate::types::Role::User, "hello")];
        let state = state_with_refs(&messages);
        let once = render_visible_refs(&messages, &state, RenderStrategy::All);
        let twice = render_visible_refs(&once, &state, RenderStrategy::All);
        assert_eq!(once[0].text_str(), twice[0].text_str());
    }

    #[test]
    fn render_none_should_leave_text() {
        let messages = vec![CoreMessage::text("a", crate::types::Role::User, "hello")];
        let state = state_with_refs(&messages);
        let rendered = render_visible_refs(&messages, &state, RenderStrategy::None);
        assert_eq!(rendered[0].text_str(), "hello");
    }

    #[test]
    fn text_only_should_skip_tool_messages() {
        let messages = vec![CoreMessage {
            id: "t".into(),
            role: crate::types::Role::Tool,
            content_type: ContentType::ToolResult,
            text: Some("output".into()),
            ..Default::default()
        }];
        let state = state_with_refs(&messages);
        let rendered = render_visible_refs(&messages, &state, RenderStrategy::TextOnly);
        assert_eq!(rendered[0].text_str(), "output");
    }
}
