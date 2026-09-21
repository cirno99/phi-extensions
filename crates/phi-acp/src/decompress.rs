//! 解压 —— 对应 acp-kernel `src/decompress.ts`。检索见 [`crate::search`]。

use std::collections::BTreeSet;

use crate::prune::SUMMARY_HEADER;
use crate::state::block_by_id;
use crate::types::{CompressionBlock, CompressionState, ContentType, CoreMessage, Role};

/// 解析块 id 参数（`b3` / `3` / `B3`）。
pub fn parse_block_id_arg(arg: &str) -> Option<String> {
    let trimmed = arg.trim().to_ascii_lowercase();
    let digits = trimmed.strip_prefix('b').unwrap_or(&trimmed);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u64 = digits.parse().ok()?;
    if n == 0 {
        return None;
    }
    Some(format!("b{n}"))
}

/// 按 id 查找块。
pub fn decompress<'a>(block_id: &str, state: &'a CompressionState) -> Option<&'a CompressionBlock> {
    block_by_id(state, block_id)
}

/// 把一个块的内容收集为可读字符串，**不修改状态**。
///
/// 这是「缓存安全」的解压原语：块保持压缩（折叠），摘要留在原位，完整内容以
/// 文本返回给调用方。与「失活 + 重建」不同，它不改动消息数组前缀，因此不破坏
/// 上游 prompt 缓存，也不产生 expand/re-fold 循环。
///
/// - `full = false`（默认）：上溯一层。本块直接压缩的原文完整渲染；被嵌套的
///   活跃子块覆盖的消息保持折叠，改渲染子块的摘要。
/// - `full = true`：递归所有层级，渲染每一条有效原始消息。
///
/// 覆盖不到任何消息时返回 `(String::new(), 0)`。
pub fn collect_block_content(
    state: &CompressionState,
    block: &CompressionBlock,
    messages: &[CoreMessage],
    full: bool,
) -> (String, usize) {
    let target_ids: BTreeSet<&str> = block
        .effective_message_ids
        .iter()
        .map(String::as_str)
        .collect();

    if full {
        let msgs: Vec<&CoreMessage> = messages
            .iter()
            .filter(|m| target_ids.contains(m.id.as_str()))
            .collect();
        if msgs.is_empty() {
            return (String::new(), 0);
        }
        let text = msgs
            .iter()
            .map(|m| format_message(m))
            .collect::<Vec<_>>()
            .join("\n\n");
        return (text, msgs.len());
    }

    // 上溯一层：被嵌套活跃子块覆盖的消息保持折叠（显示其摘要），本块自己的
    // 直接消息完整显示。
    let mut nested_children: Vec<&CompressionBlock> = Vec::new();
    let mut nested_covered: BTreeSet<&str> = BTreeSet::new();
    for child_id in &block.direct_block_ids {
        let Some(child) = block_by_id(state, child_id) else {
            continue;
        };
        if !child.active {
            continue;
        }
        nested_children.push(child);
        for id in &child.effective_message_ids {
            nested_covered.insert(id.as_str());
        }
    }

    let mut parts: Vec<String> = Vec::new();
    for child in &nested_children {
        let label = match &child.topic {
            Some(topic) => format!("{}: {topic}", child.block_id),
            None => child.block_id.clone(),
        };
        parts.push(format!("{SUMMARY_HEADER} — {label}\n{}", child.summary));
    }

    let mut direct_count = 0usize;
    for message in messages {
        if target_ids.contains(message.id.as_str()) && !nested_covered.contains(message.id.as_str())
        {
            parts.push(format_message(message));
            direct_count += 1;
        }
    }

    let count = direct_count + nested_children.len();
    if count == 0 {
        return (String::new(), 0);
    }
    (parts.join("\n\n"), count)
}

/// 把一条消息渲染为 `[role • tool]\ntext` 形态。
fn format_message(message: &CoreMessage) -> String {
    let text = message.text_str();
    match (&message.tool_name, message.content_type) {
        (Some(tool), ct) if ct != ContentType::Text => {
            format!("[{} • {tool}]\n{text}", role_label(message.role))
        }
        _ => format!("[{}]\n{text}", role_label(message.role)),
    }
}

/// 角色的小写文本（与上游 `role` 字段一致）。
fn role_label(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
        Role::Tool => "tool",
    }
}

/// 内联返回的最大字符数：超过则写入临时文件（对应上游 decompress 的 10000 字符阈值）。
pub const DECOMPRESS_INLINE_LIMIT: usize = 10_000;

/// 渲染一次解压的输出。
///
/// 内容不超过 [`DECOMPRESS_INLINE_LIMIT`] 时直接内联；超过时写入临时文件（路径按
/// block id 固定，重复解压覆盖而非累积）并返回路径提示；写入失败时回退为截断预览。
pub fn render_decompress_output(header: &str, block_id: &str, body: &str) -> String {
    let body_chars = body.chars().count();
    if body_chars <= DECOMPRESS_INLINE_LIMIT {
        return format!("{header}\n{body}");
    }
    let safe_id: String = block_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let path = std::env::temp_dir().join(format!("acp-decompress-{safe_id}.txt"));
    match std::fs::write(&path, body) {
        Ok(()) => format!(
            "{header}\nContent ({body_chars} chars) written to: {}\nUse the read tool to access it.",
            path.display()
        ),
        Err(_) => format!("{header}\n{}...", body.chars().take(4000).collect::<String>()),
    }
}

/// 查找与给定消息集合重叠的活跃块。
pub fn find_blocks_overlapping_messages<'a>(
    message_ids: &[String],
    state: &'a CompressionState,
) -> Vec<&'a CompressionBlock> {
    let set: std::collections::BTreeSet<&str> = message_ids.iter().map(String::as_str).collect();
    state
        .blocks
        .iter()
        .filter(|b| {
            b.active
                && b.effective_message_ids
                    .iter()
                    .any(|id| set.contains(id.as_str()))
        })
        .collect()
}

/// 用户显式解压：把块标记为 `expanded` 并失活，使其不被重新激活。
///
/// 返回是否命中。
pub fn deactivate_block(state: &mut CompressionState, block_id: &str) -> bool {
    if let Some(block) = state.blocks.iter_mut().find(|b| b.block_id == block_id) {
        block.active = false;
        block.expanded = Some(true);
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(id: &str, topic: &str, summary: &str) -> CompressionBlock {
        CompressionBlock {
            block_id: id.into(),
            active: true,
            topic: Some(topic.into()),
            summary: summary.into(),
            ..Default::default()
        }
    }

    #[test]
    fn parse_block_id_should_normalize() {
        assert_eq!(parse_block_id_arg("b3").as_deref(), Some("b3"));
        assert_eq!(parse_block_id_arg(" 12 ").as_deref(), Some("b12"));
        assert_eq!(parse_block_id_arg("B7").as_deref(), Some("b7"));
        assert_eq!(parse_block_id_arg("b0"), None);
        assert_eq!(parse_block_id_arg("x"), None);
    }

    #[test]
    fn deactivate_should_mark_expanded() {
        let mut state = crate::state::create_initial_state();
        state.blocks.push(block("b1", "t", "s"));
        assert!(deactivate_block(&mut state, "b1"));
        assert!(!state.blocks[0].active);
        assert_eq!(state.blocks[0].expanded, Some(true));
    }

    fn parent_and_child() -> (CompressionState, Vec<CoreMessage>) {
        let mut state = crate::state::create_initial_state();
        let mut child = block("b1", "auth", "child summary");
        child.effective_message_ids = vec!["m1".into()];
        state.blocks.push(child);
        let mut parent = block("b2", "root", "parent summary");
        parent.direct_block_ids = vec!["b1".into()];
        parent.effective_message_ids = vec!["m1".into()];
        state.blocks.push(parent);
        let messages = vec![CoreMessage::text("m1", Role::User, "original text")];
        (state, messages)
    }

    /// 回归：T2/T3 块（direct_message_ids 为空、由子块继承）以前会解压出空预览。
    #[test]
    fn collect_block_content_should_show_nested_child_summaries_one_tier_up() {
        let (state, messages) = parent_and_child();
        let (text, count) = collect_block_content(&state, &state.blocks[1], &messages, false);
        assert_eq!(count, 1);
        assert!(text.contains("child summary"), "{text}");
        assert!(
            !text.contains("original text"),
            "默认只上溯一层：原始消息应保持折叠"
        );
    }

    #[test]
    fn collect_block_content_full_should_return_original_messages() {
        let (state, messages) = parent_and_child();
        let (text, count) = collect_block_content(&state, &state.blocks[1], &messages, true);
        assert_eq!(count, 1);
        assert!(text.contains("original text"), "{text}");
        assert!(text.contains("[user]"), "{text}");
    }

    #[test]
    fn collect_block_content_should_return_zero_when_nothing_covered() {
        let state = crate::state::create_initial_state();
        let b = block("b9", "t", "s");
        let (text, count) = collect_block_content(&state, &b, &[], false);
        assert_eq!(count, 0);
        assert!(text.is_empty());
    }

    #[test]
    fn render_decompress_output_should_inline_small_body() {
        assert_eq!(render_decompress_output("H", "b1", "small"), "H\nsmall");
    }

    #[test]
    fn render_decompress_output_should_write_large_body_to_file() {
        let big = "x".repeat(DECOMPRESS_INLINE_LIMIT + 1);
        let out = render_decompress_output("[Block b1 content — 1 item(s)]", "b1", &big);
        assert!(out.contains("written to:"), "{out}");
        assert!(out.contains("acp-decompress-b1.txt"), "{out}");
        let _ = std::fs::remove_file(std::env::temp_dir().join("acp-decompress-b1.txt"));
    }
}
