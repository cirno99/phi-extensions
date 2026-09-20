//! 块跨度解析 —— 对应 acp-kernel `src/block-map.ts`。

use crate::refs::ref_to_index;
use crate::types::{BlockSpan, CompressionBlock, CompressionState, MessageRefMap};

fn is_message_ref(reference: &str) -> bool {
    ref_to_index(reference).is_some()
}

/// 解析块的当前 ref 跨度。
///
/// 优先使用存储的 `startRef`/`endRef`（创建时由请求范围写入）；仅当两者都是
/// 消息 ref 时才可信（块边界蒸馏块会存 `bN`）。否则回退到有效消息 ref 的最小/最大。
pub fn resolve_block_span(
    block: &CompressionBlock,
    by_raw: &std::collections::BTreeMap<String, String>,
) -> Option<(String, String)> {
    if let (Some(start), Some(end)) = (block.start_ref.as_deref(), block.end_ref.as_deref()) {
        if is_message_ref(start) && is_message_ref(end) {
            return Some((start.to_string(), end.to_string()));
        }
    }
    let mut refs: Vec<&str> = block
        .effective_message_ids
        .iter()
        .filter_map(|id| by_raw.get(id).map(String::as_str))
        .filter(|r| *r != crate::refs::BLOCKED_REF)
        .collect();
    if refs.is_empty() {
        return None;
    }
    refs.sort_by_key(|r| ref_to_index(r).unwrap_or(0));
    Some((refs[0].to_string(), refs[refs.len() - 1].to_string()))
}

/// 活跃块的 ref 跨度。
pub fn active_block_spans(state: &CompressionState) -> Vec<BlockSpan> {
    let by_raw = &state.message_refs.by_raw;
    let mut spans = Vec::new();
    for block in &state.blocks {
        if !block.active {
            continue;
        }
        if let Some((start_ref, end_ref)) = resolve_block_span(block, by_raw) {
            spans.push(BlockSpan {
                block_id: block.block_id.clone(),
                tier: block.tier,
                start_ref,
                end_ref,
            });
        }
    }
    spans
}

/// 格式化新建块，例如 `blocks: b3=m00044–m00097, b4=m00103–m00123`。
pub fn format_created_blocks(state: &CompressionState, new_blocks: &[CompressionBlock]) -> String {
    let by_raw = &state.message_refs.by_raw;
    let parts: Vec<String> = new_blocks
        .iter()
        .map(|block| match resolve_block_span(block, by_raw) {
            Some((start, end)) => format!("{}={start}–{end}", block.block_id),
            None => block.block_id.clone(),
        })
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("blocks: {}", parts.join(", "))
    }
}

/// 便捷：给 `MessageRefMap` 用。
pub fn active_block_spans_from_map(
    state: &CompressionState,
    _map: &MessageRefMap,
) -> Vec<BlockSpan> {
    active_block_spans(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_span_should_prefer_stored_message_refs() {
        let block = CompressionBlock {
            block_id: "b1".into(),
            start_ref: Some("m00005".into()),
            end_ref: Some("m00020".into()),
            effective_message_ids: vec!["x".into()],
            ..Default::default()
        };
        let by_raw = Default::default();
        assert_eq!(
            resolve_block_span(&block, &by_raw),
            Some(("m00005".into(), "m00020".into()))
        );
    }

    #[test]
    fn resolve_span_should_ignore_block_boundary_refs() {
        let mut by_raw = std::collections::BTreeMap::new();
        by_raw.insert("a".into(), "m00002".into());
        by_raw.insert("b".into(), "m00007".into());
        let block = CompressionBlock {
            block_id: "b1".into(),
            start_ref: Some("b1".into()),
            end_ref: Some("b2".into()),
            effective_message_ids: vec!["a".into(), "b".into()],
            ..Default::default()
        };
        assert_eq!(
            resolve_block_span(&block, &by_raw),
            Some(("m00002".into(), "m00007".into()))
        );
    }
}
