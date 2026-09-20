//! 视图重建（prune）—— 对应 acp-kernel `src/prune.ts`。
//!
//! 把被块覆盖的原始消息替换为渲染摘要，并清理孤立的工具调用 / 结果 / 推理，
//! 保证交给上游的消息序列始终合法（严格上游会拒绝半配对的 tool_calls）。

use crate::state::{active_blocks, covered_message_ids};
use crate::types::{CompressionState, ContentType, CoreMessage, Role};

/// 渲染摘要的头部文本。
pub const SUMMARY_HEADER: &str = "[Compressed conversation section]";

/// 渲染摘要 id 前缀。
const SUMMARY_ID_PREFIX: &str = "acp_summary_";

/// 块的渲染摘要消息 id。
pub fn summary_message_id(block_id: &str) -> String {
    format!("{SUMMARY_ID_PREFIX}{block_id}")
}

/// 是否为摘要消息 id。
pub fn is_summary_message_id(id: &str) -> bool {
    id.starts_with(SUMMARY_ID_PREFIX)
}

/// 是否为「渲染摘要」形态的消息（id 前缀 + system + text）。
pub fn is_rendered_summary_message(message: &CoreMessage) -> bool {
    is_summary_message_id(&message.id)
        && message.role == Role::System
        && message.content_type == ContentType::Text
}

/// 重建视图。
pub fn prune(messages: &[CoreMessage], state: &CompressionState) -> Vec<CoreMessage> {
    let covered = covered_message_ids(state);
    if covered.is_empty() {
        return messages.to_vec();
    }

    let first_user_index = messages.iter().position(|m| m.role == Role::User);

    let index_by_id: std::collections::BTreeMap<&str, usize> = messages
        .iter()
        .enumerate()
        .map(|(i, m)| (m.id.as_str(), i))
        .collect();

    let anchors = collect_summary_anchors(state, &index_by_id);

    let rebuilt = rebuild_messages(messages, &covered, first_user_index, &anchors);
    strip_orphaned_reasoning(&strip_orphaned_tool_results(&strip_orphaned_tool_calls(
        &rebuilt,
    )))
}

struct SummaryAnchor {
    block_id: String,
    summary: String,
    topic: Option<String>,
    insert_at: usize,
}

fn collect_summary_anchors(
    state: &CompressionState,
    index_by_id: &std::collections::BTreeMap<&str, usize>,
) -> Vec<SummaryAnchor> {
    let mut anchors = Vec::new();
    for block in active_blocks(state) {
        let summary_id = summary_message_id(&block.block_id);
        let insert_at = if let Some(index) = index_by_id.get(summary_id.as_str()) {
            *index
        } else {
            let mut earliest: Option<usize> = None;
            for id in &block.effective_message_ids {
                if let Some(index) = index_by_id.get(id.as_str()) {
                    if earliest.is_none() || *index < earliest.unwrap() {
                        earliest = Some(*index);
                    }
                }
            }
            earliest.unwrap_or(0)
        };
        anchors.push(SummaryAnchor {
            block_id: block.block_id.clone(),
            summary: block.summary.clone(),
            topic: block.topic.clone(),
            insert_at,
        });
    }
    anchors.sort_by_key(|a| a.insert_at);
    anchors
}

/// 摘要锚点不能落在一个 provider 校验为整体的单元内部。
fn pair_safe_anchor_index(messages: &[CoreMessage], index: usize) -> usize {
    let mut safe = index;
    // 连续的 assistant 核会合并成一条 wire 消息，锚点落在其内部会拆散推理运行。
    while safe > 0
        && safe < messages.len()
        && messages[safe - 1].role == Role::Assistant
        && messages[safe].role == Role::Assistant
    {
        safe -= 1;
    }

    // 工具结果回复其调用；锚点落在调用与结果之间会破坏配对。
    let mut result_index_by_call: std::collections::BTreeMap<&str, usize> = Default::default();
    for (at, message) in messages.iter().enumerate() {
        if message.content_type != ContentType::ToolResult {
            continue;
        }
        if let Some(call_id) = message.tool_call_id.as_deref() {
            result_index_by_call.entry(call_id).or_insert(at);
        }
    }
    for _ in 0..messages.len() {
        let mut moved = safe;
        for message in messages.iter().take(safe.min(messages.len())) {
            if message.role != Role::Assistant || message.content_type != ContentType::ToolCall {
                continue;
            }
            let Some(call_id) = message.tool_call_id.as_deref() else {
                continue;
            };
            if let Some(result_index) = result_index_by_call.get(call_id) {
                if *result_index >= safe && result_index + 1 > moved {
                    moved = result_index + 1;
                }
            }
        }
        if moved == safe {
            break;
        }
        safe = moved;
    }
    safe
}

fn rebuild_messages(
    messages: &[CoreMessage],
    covered: &std::collections::BTreeSet<String>,
    first_user_index: Option<usize>,
    anchors: &[SummaryAnchor],
) -> Vec<CoreMessage> {
    let mut safe_anchors: Vec<(usize, &SummaryAnchor)> = anchors
        .iter()
        .map(|a| (pair_safe_anchor_index(messages, a.insert_at), a))
        .collect();
    safe_anchors.sort_by_key(|(at, _)| *at);

    let anchored_summary_ids: std::collections::BTreeSet<String> = anchors
        .iter()
        .map(|a| summary_message_id(&a.block_id))
        .collect();

    let mut result = Vec::new();
    let mut pending = std::collections::VecDeque::from(safe_anchors);

    for (index, message) in messages.iter().enumerate() {
        while pending.front().map(|(at, _)| *at) == Some(index) {
            if let Some((_, anchor)) = pending.pop_front() {
                result.push(render_summary(anchor));
            }
        }
        if Some(index) == first_user_index {
            result.push(message.clone());
            continue;
        }
        if covered.contains(&message.id) {
            continue;
        }
        if is_rendered_summary_message(message) && anchored_summary_ids.contains(&message.id) {
            continue;
        }
        result.push(message.clone());
    }
    while let Some((_, anchor)) = pending.pop_front() {
        result.push(render_summary(anchor));
    }
    result
}

fn render_summary(anchor: &SummaryAnchor) -> CoreMessage {
    let body = anchor.summary.trim();
    let topic_line = match &anchor.topic {
        Some(topic) if !topic.is_empty() => format!("{SUMMARY_HEADER} — {topic}"),
        _ => SUMMARY_HEADER.to_string(),
    };
    let text = if body.is_empty() {
        topic_line
    } else {
        format!("{topic_line}\n{body}")
    };
    CoreMessage {
        id: summary_message_id(&anchor.block_id),
        role: Role::System,
        content_type: ContentType::Text,
        text: Some(text),
        ..Default::default()
    }
}

fn strip_orphaned_tool_results(messages: &[CoreMessage]) -> Vec<CoreMessage> {
    let known: std::collections::BTreeSet<&str> = messages
        .iter()
        .filter(|m| m.content_type == ContentType::ToolCall)
        .filter_map(|m| m.tool_call_id.as_deref())
        .collect();
    messages
        .iter()
        .filter(|m| {
            m.content_type != ContentType::ToolResult
                || m.tool_call_id.is_none()
                || known.contains(m.tool_call_id.as_deref().unwrap_or(""))
        })
        .cloned()
        .collect()
}

fn strip_orphaned_tool_calls(messages: &[CoreMessage]) -> Vec<CoreMessage> {
    let known: std::collections::BTreeSet<&str> = messages
        .iter()
        .filter(|m| m.content_type == ContentType::ToolResult)
        .filter_map(|m| m.tool_call_id.as_deref())
        .collect();
    messages
        .iter()
        .filter(|m| {
            m.content_type != ContentType::ToolCall
                || m.tool_call_id.is_none()
                || m.tool_name.as_deref() == Some("compress")
                || known.contains(m.tool_call_id.as_deref().unwrap_or(""))
        })
        .cloned()
        .collect()
}

fn strip_orphaned_reasoning(messages: &[CoreMessage]) -> Vec<CoreMessage> {
    let mut drop = std::collections::BTreeSet::new();
    let mut i = 0;
    while i < messages.len() {
        if drop.contains(&i) || messages[i].content_type != ContentType::Reasoning {
            i += 1;
            continue;
        }
        let mut j = i;
        while j + 1 < messages.len() && messages[j + 1].content_type == ContentType::Reasoning {
            j += 1;
        }
        let has_companion = messages.get(j + 1).is_some_and(|c| {
            c.role == Role::Assistant
                && (c.content_type == ContentType::Text || c.content_type == ContentType::ToolCall)
        });
        if !has_companion {
            for k in i..=j {
                drop.insert(k);
            }
        }
        i = j + 1;
    }
    if drop.is_empty() {
        return messages.to_vec();
    }
    messages
        .iter()
        .enumerate()
        .filter(|(i, _)| !drop.contains(i))
        .map(|(_, m)| m.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::CompressionBlock;

    fn block(id: &str, covered: &[&str]) -> CompressionBlock {
        CompressionBlock {
            block_id: id.into(),
            active: true,
            summary: "summary text".into(),
            effective_message_ids: covered.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn prune_should_noop_without_blocks() {
        let messages = vec![CoreMessage::text("a", Role::User, "hi")];
        let state = crate::state::create_initial_state();
        assert_eq!(prune(&messages, &state).len(), 1);
    }

    #[test]
    fn prune_should_replace_covered_with_summary() {
        let messages = vec![
            CoreMessage::text("a", Role::User, "keep me"),
            CoreMessage::text("b", Role::Assistant, "old"),
            CoreMessage::text("c", Role::Assistant, "old2"),
        ];
        let mut state = crate::state::create_initial_state();
        state.blocks.push(block("b1", &["b", "c"]));
        let pruned = prune(&messages, &state);
        // 首条用户消息始终保留；被覆盖的两条合并为一条摘要。
        assert_eq!(pruned.len(), 2);
        assert_eq!(pruned[0].id, "a");
        assert!(is_rendered_summary_message(&pruned[1]));
    }

    #[test]
    fn strip_orphaned_tool_results_drops_unanswered() {
        let messages = vec![
            CoreMessage {
                id: "r".into(),
                role: Role::Tool,
                content_type: ContentType::ToolResult,
                tool_call_id: Some("gone".into()),
                ..Default::default()
            },
            CoreMessage::text("t", Role::User, "hi"),
        ];
        assert_eq!(strip_orphaned_tool_results(&messages).len(), 1);
    }

    #[test]
    fn strip_orphaned_reasoning_drops_without_companion() {
        let messages = vec![
            CoreMessage {
                id: "r".into(),
                role: Role::Assistant,
                content_type: ContentType::Reasoning,
                ..Default::default()
            },
            CoreMessage::text("t", Role::User, "hi"),
        ];
        assert_eq!(strip_orphaned_reasoning(&messages).len(), 1);
    }
}
