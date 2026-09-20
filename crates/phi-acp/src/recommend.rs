//! 推荐引擎 —— 对应 acp-kernel `src/recommend.ts`。
//!
//! 纯函数：计算软保护区、可压缩范围与阈值合并。不产生副作用。

use std::collections::BTreeSet;

use crate::protected::{
    collect_latest_protected, collect_protected_tool_call_ids, is_message_latest_protected,
    is_message_protected_with_pairing, is_never_preserve_recent,
};
use crate::prune::SUMMARY_HEADER;
use crate::tokenize::count_message_tokens;
use crate::types::{
    CompressibleRange, CompressionState, Config, ContentType, ContextRanges, CoreMessage,
    ProtectedRange, Role,
};

/// 消息是否属于工具（调用或结果）。
pub fn is_tool_message(message: &CoreMessage) -> bool {
    matches!(
        message.content_type,
        ContentType::ToolCall | ContentType::ToolResult
    )
}

/// 是否为合成 / 已 prune 的占位消息。
fn is_synthetic_or_pruned(message: &CoreMessage, state: &CompressionState) -> bool {
    if message.text_str().starts_with(SUMMARY_HEADER) {
        return true;
    }
    state
        .blocks
        .iter()
        .any(|b| b.active && b.effective_message_ids.iter().any(|id| id == &message.id))
}

/// 计算软保护区 ref 集合（最近 N 条 + 最近 N token + 最后一条用户消息）。
pub fn compute_protected_refs(
    messages: &[CoreMessage],
    state: &CompressionState,
    config: &Config,
) -> BTreeSet<String> {
    let preserve_n = config.preserve_recent_messages;
    let preserve_tokens = config.preserve_recent_tokens;

    let mut result = BTreeSet::new();
    let mut visible: Vec<(String, u64)> = Vec::new();

    for message in messages {
        if is_synthetic_or_pruned(message, state) || is_never_preserve_recent(message) {
            continue;
        }
        let Some(reference) = state.message_refs.by_raw.get(&message.id) else {
            continue;
        };
        if reference == crate::refs::BLOCKED_REF {
            continue;
        }
        visible.push((reference.clone(), count_message_tokens(message)));
    }

    if preserve_n > 0 {
        let start = visible.len().saturating_sub(preserve_n);
        for (reference, _) in &visible[start..] {
            result.insert(reference.clone());
        }
    }

    if preserve_tokens > 0 {
        let mut accum = 0u64;
        for (reference, tokens) in visible.iter().rev() {
            if accum >= preserve_tokens {
                break;
            }
            result.insert(reference.clone());
            accum += tokens;
        }
    }

    if preserve_n > 0 {
        for message in messages.iter().rev() {
            if message.role != Role::User || is_synthetic_or_pruned(message, state) {
                continue;
            }
            if let Some(reference) = state.message_refs.by_raw.get(&message.id) {
                if reference != crate::refs::BLOCKED_REF {
                    result.insert(reference.clone());
                }
            }
            break;
        }
    }

    result
}

/// 计算应从可压缩集合中撤回的消息 id（避免拆分工具配对 / 推理运行）。
///
/// 简化自 TS 的 `computeIntegrityWithdrawals`：把候选集合视为一个「整体」，
/// 若工具调用与结果不全在集合内、或推理运行与其伴随消息不全在集合内，
/// 则把相关消息全部撤回。
pub fn compute_integrity_withdrawals(
    messages: &[CoreMessage],
    candidates: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut withdrawn = BTreeSet::new();

    // 工具配对：调用与结果必须同进同出。
    let mut call_ids_in: BTreeSet<&str> = BTreeSet::new();
    let mut result_ids_in: BTreeSet<&str> = BTreeSet::new();
    for message in messages {
        let Some(call_id) = message.tool_call_id.as_deref() else {
            continue;
        };
        if !candidates.contains(&message.id) {
            continue;
        }
        match message.content_type {
            ContentType::ToolCall => {
                call_ids_in.insert(call_id);
            }
            ContentType::ToolResult => {
                result_ids_in.insert(call_id);
            }
            _ => {}
        }
    }
    for message in messages {
        let Some(call_id) = message.tool_call_id.as_deref() else {
            continue;
        };
        let split = match message.content_type {
            ContentType::ToolCall => !result_ids_in.contains(call_id),
            ContentType::ToolResult => !call_ids_in.contains(call_id),
            _ => false,
        };
        if split && candidates.contains(&message.id) {
            withdrawn.insert(message.id.clone());
        }
    }

    // 推理运行：run 内所有消息要么全在集合内，要么全不在。
    let mut i = 0;
    while i < messages.len() {
        if messages[i].content_type != ContentType::Reasoning {
            i += 1;
            continue;
        }
        let start = i;
        let mut j = i;
        while j + 1 < messages.len() && messages[j + 1].content_type == ContentType::Reasoning {
            j += 1;
        }
        let companion = messages.get(j + 1);
        let mut run: Vec<&CoreMessage> = messages[start..=j].iter().collect();
        if let Some(c) = companion {
            if c.role == Role::Assistant {
                run.push(c);
            }
        }
        let any_in = run.iter().any(|m| candidates.contains(&m.id));
        let all_in = run.iter().all(|m| candidates.contains(&m.id));
        if any_in && !all_in {
            for m in &run {
                if candidates.contains(&m.id) {
                    withdrawn.insert(m.id.clone());
                }
            }
        }
        i = j + 1;
    }

    withdrawn
}

/// 构建可压缩 / 受保护范围。
pub fn build_compressible_ranges(
    messages: &[CoreMessage],
    state: &CompressionState,
    config: &Config,
    protected_zone_refs: &BTreeSet<String>,
) -> ContextRanges {
    struct CompressibleInfo {
        id: String,
        reference: String,
        gap_before: bool,
        tokens: u64,
        chars: usize,
        is_tool: bool,
        is_user: bool,
        index: usize,
    }

    let protected_call_ids = collect_protected_tool_call_ids(messages, config);
    let latest = collect_latest_protected(messages, config);

    let mut compressible_msgs: Vec<CompressibleInfo> = Vec::new();
    let mut protected_msgs: Vec<(String, bool, u64, Vec<String>, usize)> = Vec::new();

    let mut skip_since_compressible = false;
    let mut skip_since_protected = false;

    for (index, message) in messages.iter().enumerate() {
        let Some(reference) = state.message_refs.by_raw.get(&message.id) else {
            continue;
        };
        if reference == crate::refs::BLOCKED_REF {
            continue;
        }
        if is_synthetic_or_pruned(message, state) {
            skip_since_compressible = true;
            skip_since_protected = true;
            continue;
        }

        if is_message_protected_with_pairing(message, config, &protected_call_ids)
            || is_message_latest_protected(message, &latest)
        {
            protected_msgs.push((
                reference.clone(),
                skip_since_protected,
                count_message_tokens(message),
                message.tool_name.iter().cloned().collect(),
                index,
            ));
            skip_since_protected = false;
            skip_since_compressible = true;
            continue;
        }

        if protected_zone_refs.contains(reference) {
            skip_since_compressible = true;
            skip_since_protected = true;
            continue;
        }

        compressible_msgs.push(CompressibleInfo {
            id: message.id.clone(),
            reference: reference.clone(),
            gap_before: skip_since_compressible,
            tokens: count_message_tokens(message),
            chars: message.text_str().chars().count(),
            is_tool: is_tool_message(message),
            is_user: message.role == Role::User,
            index,
        });
        skip_since_compressible = false;
        skip_since_protected = true;
    }

    // 撤回会拆分配对 / 推理运行的消息。
    let candidate_ids: BTreeSet<String> = compressible_msgs.iter().map(|i| i.id.clone()).collect();
    let withdrawn = compute_integrity_withdrawals(messages, &candidate_ids);
    if !withdrawn.is_empty() {
        let mut kept: Vec<CompressibleInfo> = Vec::new();
        let mut gap_pending = false;
        for info in compressible_msgs {
            if withdrawn.contains(&info.id) {
                gap_pending = true;
                continue;
            }
            let mut info = info;
            if gap_pending {
                info.gap_before = true;
                gap_pending = false;
            }
            kept.push(info);
        }
        compressible_msgs = kept;
    }

    // 组装可压缩组：真实数组间隙、或组内 ≥3 条后再遇用户消息时切分。
    let mut compressible: Vec<CompressibleRange> = Vec::new();
    let mut cur: Option<CompressibleRange> = None;
    for info in &compressible_msgs {
        let should_split = cur
            .as_ref()
            .is_some_and(|c| (info.is_user && c.count >= 3) || info.gap_before);
        if should_split {
            if let Some(c) = cur.take() {
                compressible.push(c);
            }
        }
        match &mut cur {
            None => {
                cur = Some(CompressibleRange {
                    start_ref: info.reference.clone(),
                    end_ref: info.reference.clone(),
                    count: 1,
                    tokens: info.tokens,
                    chars: Some(info.chars),
                    tool_pct: if info.is_tool { 100 } else { 0 },
                    text_pct: if info.is_tool { 0 } else { 100 },
                    user_msgs: Some(if info.is_user { 1 } else { 0 }),
                    start_index: Some(info.index),
                    end_index: Some(info.index),
                    ..Default::default()
                });
            }
            Some(c) => {
                c.end_ref = info.reference.clone();
                c.end_index = Some(info.index);
                c.count += 1;
                c.tokens += info.tokens;
                c.chars = Some(c.chars.unwrap_or(0) + info.chars);
                if info.is_user {
                    c.user_msgs = Some(c.user_msgs.unwrap_or(0) + 1);
                }
                if info.is_tool {
                    c.tool_pct = (c.tool_pct * (c.count - 1) as u32 + 100) / c.count as u32;
                } else {
                    c.tool_pct = c.tool_pct * (c.count - 1) as u32 / c.count as u32;
                }
                c.text_pct = 100 - c.tool_pct;
            }
        }
    }
    if let Some(c) = cur.take() {
        compressible.push(c);
    }

    // 受保护组（连续）。
    let mut protected_ranges: Vec<ProtectedRange> = Vec::new();
    let mut pcur: Option<ProtectedRange> = None;
    for (reference, gap_before, tokens, tools, index) in protected_msgs {
        if pcur.is_some() && gap_before {
            protected_ranges.push(pcur.take().unwrap());
        }
        match &mut pcur {
            None => {
                pcur = Some(ProtectedRange {
                    start_ref: reference.clone(),
                    end_ref: reference,
                    count: 1,
                    tokens,
                    tools,
                    start_index: Some(index),
                    end_index: Some(index),
                });
            }
            Some(c) => {
                c.end_ref = reference;
                c.end_index = Some(index);
                c.count += 1;
                c.tokens += tokens;
                for tool in tools {
                    if !c.tools.contains(&tool) {
                        c.tools.push(tool);
                    }
                }
            }
        }
    }
    if let Some(c) = pcur.take() {
        protected_ranges.push(c);
    }

    ContextRanges {
        compressible: compressible.into_iter().filter(|r| r.tokens > 0).collect(),
        protected: protected_ranges,
    }
}

fn range_chars(r: &CompressibleRange) -> usize {
    r.chars.unwrap_or((r.tokens * 4) as usize)
}

fn merge_batch(batch: &[CompressibleRange]) -> CompressibleRange {
    let first = &batch[0];
    let last = &batch[batch.len() - 1];
    let count: usize = batch.iter().map(|r| r.count).sum();
    let tokens: u64 = batch.iter().map(|r| r.tokens).sum();
    let chars: usize = batch.iter().map(range_chars).sum();
    let tool_pct = if count == 0 {
        0
    } else {
        batch
            .iter()
            .map(|r| r.tool_pct as usize * r.count)
            .sum::<usize>()
            .checked_div(count)
            .unwrap_or(0) as u32
    };
    let start_index = batch.iter().filter_map(|r| r.start_index).min();
    let end_index = batch.iter().filter_map(|r| r.end_index).max();
    CompressibleRange {
        start_ref: first.start_ref.clone(),
        end_ref: last.end_ref.clone(),
        count,
        tokens,
        chars: Some(chars),
        tool_pct,
        text_pct: 100 - tool_pct,
        user_msgs: Some(batch.iter().map(|r| r.user_msgs.unwrap_or(0)).sum()),
        dangerous: batch
            .iter()
            .any(|r| r.dangerous == Some(true))
            .then_some(true),
        start_index,
        end_index,
    }
}

/// 合并相邻范围，使每个返回批次单独就跨过 `min_chars`。
pub fn merge_ranges_to_threshold(
    ranges: &[CompressibleRange],
    min_chars: usize,
) -> Vec<CompressibleRange> {
    if min_chars == 0 || ranges.is_empty() {
        return ranges.to_vec();
    }
    let mut result: Vec<CompressibleRange> = Vec::new();
    let mut batch: Vec<CompressibleRange> = Vec::new();
    let mut batch_chars = 0usize;
    for r in ranges {
        batch.push(r.clone());
        batch_chars += range_chars(r);
        if batch_chars >= min_chars {
            result.push(merge_batch(&batch));
            batch.clear();
            batch_chars = 0;
        }
    }
    if !batch.is_empty() {
        if let Some(prev) = result.pop() {
            let mut merged_batch = vec![prev];
            merged_batch.extend(batch);
            result.push(merge_batch(&merged_batch));
        } else {
            // 整批都没跨过阈值：仍要返回它（而不是空 vec）。
            //
            // 旧实现只在 `!result.is_empty()` 时并入，于是首轮会话（累计字符 <
            // minCompressRange）会得到空的 recommended_ranges，T1 待压缩量恒为 0，
            // nudge 永不触发——压缩永远开不了头。
            result.push(merge_batch(&batch));
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(id: &str, text: &str) -> CoreMessage {
        CoreMessage::text(id, Role::User, text)
    }
    fn assistant(id: &str, text: &str) -> CoreMessage {
        CoreMessage::text(id, Role::Assistant, text)
    }

    fn with_refs(messages: &[CoreMessage]) -> CompressionState {
        let mut state = crate::state::create_initial_state();
        let existing = state.message_refs.clone();
        let result = crate::refs::assign_refs(
            messages,
            crate::refs::AssignRefsOptions {
                existing: &existing,
                next_index: 1,
                is_protected: None,
            },
        );
        state.message_refs = result.map;
        state
    }

    #[test]
    fn protected_refs_should_include_last_user_message() {
        let messages = vec![
            user("u1", "first"),
            assistant("a1", "reply"),
            user("u2", "second"),
        ];
        let state = with_refs(&messages);
        let config = Config::default_for(1000);
        let refs = compute_protected_refs(&messages, &state, &config);
        // 默认 preserveRecentMessages=5，全部保护。
        assert!(refs.contains("m00003"));
    }

    #[test]
    fn build_compressible_should_group_contiguous() {
        let messages = vec![
            assistant("a1", "one"),
            assistant("a2", "two"),
            assistant("a3", "three"),
        ];
        let state = with_refs(&messages);
        let mut config = Config::default_for(1000);
        config.preserve_recent_messages = 0;
        config.preserve_recent_tokens = 0;
        let ranges = build_compressible_ranges(&messages, &state, &config, &BTreeSet::new());
        assert_eq!(ranges.compressible.len(), 1);
        assert_eq!(ranges.compressible[0].count, 3);
    }

    #[test]
    fn merge_ranges_should_fold_small_tail_into_previous() {
        let ranges = vec![
            CompressibleRange {
                start_ref: "m00001".into(),
                end_ref: "m00001".into(),
                count: 1,
                tokens: 10,
                chars: Some(4000),
                ..Default::default()
            },
            CompressibleRange {
                start_ref: "m00002".into(),
                end_ref: "m00002".into(),
                count: 1,
                tokens: 1,
                chars: Some(10),
                ..Default::default()
            },
        ];
        let merged = merge_ranges_to_threshold(&ranges, 1000);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].start_ref, "m00001");
        assert_eq!(merged[0].end_ref, "m00002");
    }

    /// 回归：整批都未跨过阈值时也必须返回它，否则首轮会话永远不会被提醒压缩。
    #[test]
    fn merge_ranges_should_keep_a_batch_below_threshold() {
        let ranges = vec![CompressibleRange {
            start_ref: "m00001".into(),
            end_ref: "m00002".into(),
            count: 2,
            tokens: 100,
            chars: Some(100),
            ..Default::default()
        }];
        let merged = merge_ranges_to_threshold(&ranges, 5000);
        assert_eq!(merged.len(), 1, "低于阈值的一批不应被吞掉");
        assert_eq!(merged[0].start_ref, "m00001");
    }

    #[test]
    fn integrity_should_withdraw_split_tool_pair() {
        let messages = vec![
            CoreMessage {
                id: "c".into(),
                role: Role::Assistant,
                content_type: ContentType::ToolCall,
                tool_call_id: Some("x".into()),
                ..Default::default()
            },
            CoreMessage {
                id: "r".into(),
                role: Role::Tool,
                content_type: ContentType::ToolResult,
                tool_call_id: Some("x".into()),
                ..Default::default()
            },
        ];
        let candidates: BTreeSet<String> = ["c".to_string()].into_iter().collect();
        let withdrawn = compute_integrity_withdrawals(&messages, &candidates);
        assert!(withdrawn.contains("c"));
    }

    #[test]
    fn integrity_should_keep_complete_tool_pair() {
        let messages = vec![
            CoreMessage {
                id: "c".into(),
                role: Role::Assistant,
                content_type: ContentType::ToolCall,
                tool_call_id: Some("x".into()),
                ..Default::default()
            },
            CoreMessage {
                id: "r".into(),
                role: Role::Tool,
                content_type: ContentType::ToolResult,
                tool_call_id: Some("x".into()),
                ..Default::default()
            },
        ];
        let candidates: BTreeSet<String> = ["c".to_string(), "r".to_string()].into_iter().collect();
        assert!(compute_integrity_withdrawals(&messages, &candidates).is_empty());
    }
}
