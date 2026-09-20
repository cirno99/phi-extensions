//! 边界解析 —— 对应 acp-kernel `src/boundaries.ts`。
//!
//! 把 `compress` 调用里的 `mNNNNN` / `bN` 边界解析为消息数组下标区间。

use std::collections::BTreeMap;

use crate::prune::{is_rendered_summary_message, summary_message_id};
use crate::refs::{index_to_ref, ref_to_index};
use crate::state::{active_blocks, block_by_id};
use crate::types::{CompressionBlock, CompressionState, CoreMessage};

/// 边界类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundaryKind {
    /// 消息 ref（`mNNNNN`）。
    Message,
    /// 块 id（`bN`）。
    Block,
}

/// 解析后的边界。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedBoundary {
    /// 类型。
    pub kind: BoundaryKind,
    /// 数字部分。
    pub numeric_id: u32,
    /// 规范化后的原文（小写）。
    pub raw: String,
}

/// 解析边界文本。非法返回 `None`。
pub fn parse_boundary(reference: &str) -> Option<ParsedBoundary> {
    let normalized = reference.trim().to_ascii_lowercase();
    if let Some(index) = ref_to_index(&normalized) {
        return Some(ParsedBoundary {
            kind: BoundaryKind::Message,
            numeric_id: index,
            raw: normalized,
        });
    }
    if let Some(rest) = normalized.strip_prefix('b') {
        if !rest.is_empty() && rest.len() <= 9 && rest.bytes().all(|b| b.is_ascii_digit()) {
            let numeric_id: u32 = rest.parse().ok()?;
            if numeric_id >= 1 {
                return Some(ParsedBoundary {
                    kind: BoundaryKind::Block,
                    numeric_id,
                    raw: normalized,
                });
            }
        }
    }
    None
}

/// 边界错误类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundaryErrorKind {
    /// ref 从未存在过（拼写错误 / 跨会话）。
    Unknown,
    /// 已被块消费。
    Consumed,
}

/// 边界解析失败。
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct BoundaryNotFoundError {
    /// 失败类型。
    pub kind: BoundaryErrorKind,
    /// 失败端（start / end）。
    pub endpoint: &'static str,
    /// 说明。
    pub message: String,
}

/// 解析后的区间。
#[derive(Debug, Clone, Default)]
pub struct ResolvedRange {
    /// 起始数组下标。
    pub start_index: usize,
    /// 结束数组下标。
    pub end_index: usize,
    /// 区间内的消息 id（排除渲染摘要）。
    pub message_ids: Vec<String>,
    /// 嵌套的活跃块 id。
    pub nested_block_ids: Vec<String>,
    /// 边界类型。
    pub boundary_kind: Option<BoundaryKind>,
    /// 吸附说明。
    pub snapped_boundaries: Vec<String>,
}

/// 解析 start/end 边界为区间。
pub fn resolve_boundaries(
    start_ref: &str,
    end_ref: &str,
    messages: &[CoreMessage],
    state: &CompressionState,
) -> Result<ResolvedRange, BoundaryNotFoundError> {
    let start = parse_boundary(start_ref);
    let end = parse_boundary(end_ref);
    let (Some(start), Some(end)) = (start, end) else {
        return Err(BoundaryNotFoundError {
            kind: BoundaryErrorKind::Unknown,
            endpoint: "start",
            message: format!(
                "Invalid boundary ref(s): startId=\"{start_ref}\", endId=\"{end_ref}\". Use mNNNNN or bN."
            ),
        });
    };

    let index_by_id: BTreeMap<&str, usize> = messages
        .iter()
        .enumerate()
        .map(|(i, m)| (m.id.as_str(), i))
        .collect();

    let mut snapped = Vec::new();
    let start_anchor = resolve_anchor_index(&start, state, &index_by_id, "start")?;
    if let Some(note) = start_anchor.snapped {
        snapped.push(note);
    }
    let end_anchor = resolve_anchor_index(&end, state, &index_by_id, "end")?;
    if let Some(note) = end_anchor.snapped {
        snapped.push(note);
    }

    let (start_index, end_index) = if start_anchor.index <= end_anchor.index {
        (start_anchor.index, end_anchor.index)
    } else {
        (end_anchor.index, start_anchor.index)
    };

    let message_ids: Vec<String> = messages[start_index..=end_index]
        .iter()
        .filter(|m| !is_rendered_summary_message(m))
        .map(|m| m.id.clone())
        .collect();

    let boundary_kind = if start.kind == BoundaryKind::Block || end.kind == BoundaryKind::Block {
        BoundaryKind::Block
    } else {
        BoundaryKind::Message
    };

    let mut nested_block_ids = Vec::new();
    for block in active_blocks(state) {
        if block_visible_in_range(block, &index_by_id, start_index, end_index)
            && !nested_block_ids.contains(&block.block_id)
        {
            nested_block_ids.push(block.block_id.clone());
        }
    }

    Ok(ResolvedRange {
        start_index,
        end_index,
        message_ids,
        nested_block_ids,
        boundary_kind: Some(boundary_kind),
        snapped_boundaries: snapped,
    })
}

struct AnchorResolution {
    index: usize,
    snapped: Option<String>,
}

fn resolve_anchor_index(
    boundary: &ParsedBoundary,
    state: &CompressionState,
    index_by_id: &BTreeMap<&str, usize>,
    endpoint: &'static str,
) -> Result<AnchorResolution, BoundaryNotFoundError> {
    let label = if endpoint == "start" {
        "startId"
    } else {
        "endId"
    };

    if boundary.kind == BoundaryKind::Message {
        let mut raw_id = state.message_refs.by_ref.get(&boundary.raw).cloned();
        if raw_id.is_none() {
            if let Some(padded) = index_to_ref(boundary.numeric_id) {
                raw_id = state.message_refs.by_ref.get(&padded).cloned();
            }
        }
        let Some(raw_id) = raw_id else {
            return Err(BoundaryNotFoundError {
                kind: BoundaryErrorKind::Unknown,
                endpoint,
                message: format!(
                    "{label}=\"{}\" does not exist in this session (typo or wrong session) — run acp_status for current refs.",
                    boundary.raw
                ),
            });
        };
        if let Some(index) = index_by_id.get(raw_id.as_str()) {
            return Ok(AnchorResolution {
                index: *index,
                snapped: None,
            });
        }
        if let Some(index) = active_owner_anchor(state, std::slice::from_ref(&raw_id), index_by_id)
        {
            return Ok(AnchorResolution {
                index,
                snapped: Some(format!(
                    "{label}=\"{}\" refers to a message already compressed into an active block — anchored to the active block covering it instead.",
                    boundary.raw
                )),
            });
        }
        return Err(BoundaryNotFoundError {
            kind: BoundaryErrorKind::Consumed,
            endpoint,
            message: format!(
                "{label}=\"{}\" not found in visible context (likely consumed by an existing block).",
                boundary.raw
            ),
        });
    }

    let block_id = format!("b{}", boundary.numeric_id);
    let Some(block) = block_by_id(state, &block_id) else {
        return Err(BoundaryNotFoundError {
            kind: BoundaryErrorKind::Unknown,
            endpoint,
            message: format!(
                "{label}=\"{block_id}\" does not exist in this session (typo or wrong session) — run acp_status for current refs."
            ),
        });
    };
    if block.active {
        if let Some(anchor) = visible_block_anchor(block, index_by_id) {
            return Ok(AnchorResolution {
                index: anchor,
                snapped: None,
            });
        }
    }
    if let Some(index) = active_owner_anchor(state, &block.effective_message_ids, index_by_id) {
        return Ok(AnchorResolution {
            index,
            snapped: Some(format!(
                "{label}=\"{block_id}\" was consumed by a higher-tier block — anchored to the active block covering its content instead."
            )),
        });
    }
    Err(BoundaryNotFoundError {
        kind: BoundaryErrorKind::Consumed,
        endpoint,
        message: format!(
            "{label}=\"{block_id}\" not found in visible context (block distilled/consumed by a higher-tier block)."
        ),
    })
}

/// 被消费的锚点吸附到当前拥有其内容的活跃块。
fn active_owner_anchor(
    state: &CompressionState,
    owned_ids: &[String],
    index_by_id: &BTreeMap<&str, usize>,
) -> Option<usize> {
    if owned_ids.is_empty() {
        return None;
    }
    let owned: std::collections::BTreeSet<&String> = owned_ids.iter().collect();
    let mut best: Option<usize> = None;
    for block in &state.blocks {
        if !block.active {
            continue;
        }
        let inherited = inherited_content_ids(state, block);
        if !owned.iter().any(|id| inherited.contains(id.as_str())) {
            continue;
        }
        if let Some(anchor) = visible_block_anchor(block, index_by_id) {
            if best.is_none() || anchor < best.unwrap() {
                best = Some(anchor);
            }
        }
    }
    best
}

/// 块通过消费其它块而继承的内容 id。
fn inherited_content_ids(
    state: &CompressionState,
    block: &CompressionBlock,
) -> std::collections::BTreeSet<String> {
    let mut ids = std::collections::BTreeSet::new();
    for child_id in &block.direct_block_ids {
        if let Some(child) = block_by_id(state, child_id) {
            for id in &child.effective_message_ids {
                ids.insert(id.clone());
            }
        }
    }
    ids
}

/// 块的可见锚点下标：优先渲染摘要，其次最早可见原文。
pub fn visible_block_anchor(
    block: &CompressionBlock,
    index_by_id: &BTreeMap<&str, usize>,
) -> Option<usize> {
    let summary_id = summary_message_id(&block.block_id);
    if let Some(index) = index_by_id.get(summary_id.as_str()) {
        return Some(*index);
    }
    earliest_index_of_ids(&block.effective_message_ids, index_by_id)
}

/// 块的任一可见表示落在区间内。
pub fn block_visible_in_range(
    block: &CompressionBlock,
    index_by_id: &BTreeMap<&str, usize>,
    start_index: usize,
    end_index: usize,
) -> bool {
    let summary_id = summary_message_id(&block.block_id);
    if let Some(index) = index_by_id.get(summary_id.as_str()) {
        if *index >= start_index && *index <= end_index {
            return true;
        }
    }
    match earliest_index_of_ids(&block.effective_message_ids, index_by_id) {
        Some(index) => index >= start_index && index <= end_index,
        None => false,
    }
}

fn earliest_index_of_ids(ids: &[String], index_by_id: &BTreeMap<&str, usize>) -> Option<usize> {
    let mut earliest: Option<usize> = None;
    for id in ids {
        if let Some(index) = index_by_id.get(id.as_str()) {
            if earliest.is_none() || *index < earliest.unwrap() {
                earliest = Some(*index);
            }
        }
    }
    earliest
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refs::{assign_refs, AssignRefsOptions};
    use crate::types::Role;

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
    fn parse_boundary_should_distinguish_message_and_block() {
        assert_eq!(
            parse_boundary("m00005").unwrap().kind,
            BoundaryKind::Message
        );
        assert_eq!(parse_boundary("b3").unwrap().kind, BoundaryKind::Block);
        assert!(parse_boundary("foo").is_none());
        assert!(parse_boundary("b0").is_none());
    }

    #[test]
    fn resolve_boundaries_should_span_message_range() {
        let messages = vec![
            CoreMessage::text("a", Role::User, "one"),
            CoreMessage::text("b", Role::Assistant, "two"),
            CoreMessage::text("c", Role::User, "three"),
        ];
        let state = state_with_refs(&messages);
        let range = resolve_boundaries("m00001", "m00002", &messages, &state).unwrap();
        assert_eq!(range.start_index, 0);
        assert_eq!(range.end_index, 1);
        assert_eq!(range.message_ids, vec!["a", "b"]);
    }

    #[test]
    fn resolve_boundaries_should_swap_reversed_range() {
        let messages = vec![
            CoreMessage::text("a", Role::User, "one"),
            CoreMessage::text("b", Role::Assistant, "two"),
        ];
        let state = state_with_refs(&messages);
        let range = resolve_boundaries("m00002", "m00001", &messages, &state).unwrap();
        assert_eq!(range.start_index, 0);
        assert_eq!(range.end_index, 1);
    }

    #[test]
    fn resolve_boundaries_should_report_unknown_ref() {
        let messages = vec![CoreMessage::text("a", Role::User, "one")];
        let state = state_with_refs(&messages);
        let err = resolve_boundaries("m00099", "m00001", &messages, &state).unwrap_err();
        assert_eq!(err.kind, BoundaryErrorKind::Unknown);
    }
}
