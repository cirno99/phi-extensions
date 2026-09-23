//! 推荐引擎 —— 对应 acp-kernel `src/recommend.ts`。
//!
//! 纯函数：计算软保护区、可压缩范围与阈值合并。不产生副作用。

use std::collections::{BTreeMap, BTreeSet};

use crate::protected::{
    collect_latest_protected, collect_protected_tool_call_ids, is_message_latest_protected,
    is_message_protected_with_pairing, is_never_preserve_recent,
};
use crate::prune::SUMMARY_HEADER;
use crate::tokenize::MessageTokenIndex;
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
    tokens: &MessageTokenIndex<'_>,
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
        visible.push((reference.clone(), tokens.tokens(message)));
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

/// 助手「动作」消息：助手文本或工具调用。
fn is_assistant_act(message: &CoreMessage) -> bool {
    message.role == Role::Assistant
        && matches!(
            message.content_type,
            ContentType::Text | ContentType::ToolCall
        )
}

/// 供压缩完整性使用的原子 turn 分组（对应 TS `computeTurnGroups`）。
///
/// 一个 turn = 一段推理运行 + 紧随其后的助手文本/工具调用突发 + 与突发中工具调用
/// 按 `toolCallId` 配对的全部工具结果。严格回显推理的供应商（DeepSeek 思考模式：
/// “thinking 模式下的 reasoning_content 必须回传”）会拒绝「助手工具调用回合存活、
/// 但其推理丢失」的重建请求，而所有 OpenAI 线协议供应商都会拒绝「调用已消失、结果
/// 还在」的请求。因此 turn 对折叠是原子的：要么全折，要么全不折。
///
/// 分组基于相邻关系，与 `adjust_boundaries_for_reasoning_pairs` 一致：推理运行与
/// 紧邻其后的助手突发配对。没有前置推理的突发，仍与其兄弟调用/结果成组。不属于任何
/// turn 的消息不进入任何组。
pub fn compute_turn_groups(messages: &[CoreMessage]) -> Vec<Vec<String>> {
    let mut result_id_by_call_id: BTreeMap<&str, &str> = BTreeMap::new();
    for message in messages {
        if message.content_type != ContentType::ToolResult || message.id.is_empty() {
            continue;
        }
        if let Some(call_id) = message.tool_call_id.as_deref() {
            result_id_by_call_id
                .entry(call_id)
                .or_insert(message.id.as_str());
        }
    }

    let mut grouped: BTreeSet<&str> = BTreeSet::new();
    let mut groups: Vec<Vec<String>> = Vec::new();
    for i in 0..messages.len() {
        let msg = &messages[i];
        if msg.id.is_empty() || grouped.contains(msg.id.as_str()) {
            continue;
        }
        if msg.content_type != ContentType::Reasoning && !is_assistant_act(msg) {
            continue;
        }

        let mut reasoning_start = i;
        if msg.content_type == ContentType::Reasoning {
            while reasoning_start > 0
                && messages[reasoning_start - 1].content_type == ContentType::Reasoning
            {
                reasoning_start -= 1;
            }
        } else {
            let mut s = i;
            while s > 0 && is_assistant_act(&messages[s - 1]) {
                s -= 1;
            }
            reasoning_start = s;
            while reasoning_start > 0
                && messages[reasoning_start - 1].content_type == ContentType::Reasoning
            {
                reasoning_start -= 1;
            }
        }
        let mut burst_start = reasoning_start;
        while burst_start < messages.len()
            && messages[burst_start].content_type == ContentType::Reasoning
        {
            burst_start += 1;
        }
        if burst_start >= messages.len() || !is_assistant_act(&messages[burst_start]) {
            // 孤立推理运行（无伴随突发）：无配对约束。
            continue;
        }
        let mut burst_end = burst_start;
        while burst_end + 1 < messages.len() && is_assistant_act(&messages[burst_end + 1]) {
            burst_end += 1;
        }

        let mut members: BTreeSet<&str> = BTreeSet::new();
        for m in &messages[reasoning_start..=burst_end] {
            if m.id.is_empty() {
                continue;
            }
            members.insert(m.id.as_str());
            if m.role == Role::Assistant && m.content_type == ContentType::ToolCall {
                if let Some(call_id) = m.tool_call_id.as_deref() {
                    if let Some(rid) = result_id_by_call_id.get(call_id) {
                        members.insert(rid);
                    }
                }
            }
        }
        for id in &members {
            grouped.insert(id);
        }
        groups.push(members.into_iter().map(|s| s.to_string()).collect());
    }
    groups
}

/// 折叠完整性门的结果：必须改为保持可见的 id，以及涉及的 turn / 配对数量。
pub struct IntegrityWithdrawals {
    /// 必须保持可见的 id。
    pub withdrawn: BTreeSet<String>,
    /// 被撤回的 turn 数。
    pub split_turn_count: usize,
    /// 被撤回的工具配对数。
    pub split_pair_count: usize,
}

/// 折叠完整性门（对应 TS `computeIntegrityWithdrawals`）：给定一次折叠要从可见流里
/// 移除的 id（`folded_ids`），返回必须改为**保持可见**的 id。
///
/// - INV1 —— 存活的助手工具调用必须保留其推理运行：若某 turn 的推理被折叠、而同一
///   turn 的某个调用存活，则整个 turn 撤回（全部成员保持可见）。
/// - INV2 —— 工具调用与其结果是一次交换：被折叠拆开的配对一起撤回。
///
/// 两条不变式相互影响：为 INV2 撤回一个调用，会让某 turn 的推理仍被折叠、而它的调用
/// 现在存活——正是 INV1 的分裂；为 INV1 撤回一个 turn，又可能使某个结果落单。单趟
/// 固定顺序会漏掉这两者，因此重复到不再变化；每个 turn / 配对只处理一次、只报告一次。
///
/// 反向保持允许（#564）：推理与文本保持可见、而调用与其结果被折叠，仍是合法流。
pub fn compute_integrity_withdrawals(
    messages: &[CoreMessage],
    folded_ids: &BTreeSet<String>,
) -> IntegrityWithdrawals {
    let mut remaining: BTreeSet<&str> = folded_ids.iter().map(|s| s.as_str()).collect();
    let mut withdrawn: BTreeSet<String> = BTreeSet::new();
    let mut handled_turns: BTreeSet<usize> = BTreeSet::new();
    let mut handled_pairs: BTreeSet<&str> = BTreeSet::new();

    let mut reasoning_ids: BTreeSet<&str> = BTreeSet::new();
    let mut call_ids: BTreeSet<&str> = BTreeSet::new();
    let mut call_id_by_message_id: BTreeMap<&str, &str> = BTreeMap::new();
    let mut result_id_by_call_id: BTreeMap<&str, &str> = BTreeMap::new();
    for m in messages {
        if m.id.is_empty() {
            continue;
        }
        if m.content_type == ContentType::Reasoning {
            reasoning_ids.insert(m.id.as_str());
        }
        if m.role == Role::Assistant && m.content_type == ContentType::ToolCall {
            call_ids.insert(m.id.as_str());
            if let Some(call_id) = m.tool_call_id.as_deref() {
                call_id_by_message_id.insert(m.id.as_str(), call_id);
            }
        }
        if m.content_type == ContentType::ToolResult {
            if let Some(call_id) = m.tool_call_id.as_deref() {
                result_id_by_call_id.entry(call_id).or_insert(m.id.as_str());
            }
        }
    }

    let groups = compute_turn_groups(messages);
    let mut changed = true;
    while changed {
        changed = false;

        for (g, group) in groups.iter().enumerate() {
            if handled_turns.contains(&g) {
                continue;
            }
            let fold_has_reasoning = group
                .iter()
                .any(|id| remaining.contains(id.as_str()) && reasoning_ids.contains(id.as_str()));
            if !fold_has_reasoning {
                continue;
            }
            let kept_has_call = group
                .iter()
                .any(|id| !remaining.contains(id.as_str()) && call_ids.contains(id.as_str()));
            if !kept_has_call {
                continue;
            }
            handled_turns.insert(g);
            for id in group {
                remaining.remove(id.as_str());
                withdrawn.insert(id.clone());
            }
            changed = true;
        }

        for m in messages {
            if m.id.is_empty() || !call_ids.contains(m.id.as_str()) {
                continue;
            }
            let Some(call_id) = call_id_by_message_id.get(m.id.as_str()).copied() else {
                continue;
            };
            if handled_pairs.contains(call_id) {
                continue;
            }
            let Some(result_id) = result_id_by_call_id.get(call_id).copied() else {
                continue;
            };
            if remaining.contains(m.id.as_str()) == remaining.contains(result_id) {
                continue;
            }
            handled_pairs.insert(call_id);
            remaining.remove(m.id.as_str());
            remaining.remove(result_id);
            withdrawn.insert(m.id.clone());
            withdrawn.insert(result_id.to_string());
            changed = true;
        }
    }

    IntegrityWithdrawals {
        withdrawn,
        split_turn_count: handled_turns.len(),
        split_pair_count: handled_pairs.len(),
    }
}

/// 构建可压缩 / 受保护范围。
pub fn build_compressible_ranges(
    messages: &[CoreMessage],
    state: &CompressionState,
    config: &Config,
    protected_zone_refs: &BTreeSet<String>,
    tokens: &MessageTokenIndex<'_>,
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
                tokens.tokens(message),
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
            tokens: tokens.tokens(message),
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
    let withdrawn = compute_integrity_withdrawals(messages, &candidate_ids).withdrawn;
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

/// 可推荐范围的最小 token 数（对应 TS `VIABLE_RANGE_MIN_TOKENS`）。
///
/// 低于此值的范围是碎片残留（一条 16 token 的 ack、一行工具结果）：模型写不出
/// 一个满足 ≥50 字符的摘要，而且一次包含它的**批量** compress 会被内核**整批**
/// 拒绝（内核校验整批）——线上观察到过「14 个范围里夹一个 16 token 范围 → 每次
/// 批量尝试都以 Summary too short 失败」。因此所有**展示**范围的面（注入的提醒、
/// acp_status、/acp 面板）都要先过这层过滤。
pub const VIABLE_RANGE_MIN_TOKENS: u64 = 200;

/// 丢弃低于 `VIABLE_RANGE_MIN_TOKENS` 的碎片范围。
pub fn viable_ranges(ranges: &[CompressibleRange]) -> Vec<CompressibleRange> {
    ranges
        .iter()
        .filter(|r| r.tokens >= VIABLE_RANGE_MIN_TOKENS)
        .cloned()
        .collect()
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
    fn range(tokens: u64) -> CompressibleRange {
        CompressibleRange {
            start_ref: "m00001".into(),
            end_ref: "m00002".into(),
            tokens,
            ..Default::default()
        }
    }

    #[test]
    fn viable_ranges_drops_fragments_below_floor() {
        // 边界：正好等于下限保留，低于下限丢弃，空输入为空。
        let ranges = vec![range(16), range(199), range(200), range(5000)];
        let kept = viable_ranges(&ranges);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].tokens, 200);
        assert_eq!(kept[1].tokens, 5000);
        assert_eq!(VIABLE_RANGE_MIN_TOKENS, 200);
        assert!(viable_ranges(&[]).is_empty());
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
        let tokens = crate::tokenize::MessageTokenIndex::build(&messages);
        let refs = compute_protected_refs(&messages, &state, &config, &tokens);
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
        let tokens = crate::tokenize::MessageTokenIndex::build(&messages);
        let ranges =
            build_compressible_ranges(&messages, &state, &config, &BTreeSet::new(), &tokens);
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
        let withdrawn = compute_integrity_withdrawals(&messages, &candidates).withdrawn;
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
        assert!(compute_integrity_withdrawals(&messages, &candidates)
            .withdrawn
            .is_empty());
    }

    fn reasoning(id: &str) -> CoreMessage {
        CoreMessage {
            id: id.into(),
            role: Role::Assistant,
            content_type: ContentType::Reasoning,
            ..Default::default()
        }
    }

    fn tool_call(id: &str, call_id: &str) -> CoreMessage {
        CoreMessage {
            id: id.into(),
            role: Role::Assistant,
            content_type: ContentType::ToolCall,
            tool_call_id: Some(call_id.into()),
            ..Default::default()
        }
    }

    fn tool_result(id: &str, call_id: &str) -> CoreMessage {
        CoreMessage {
            id: id.into(),
            role: Role::Tool,
            content_type: ContentType::ToolResult,
            tool_call_id: Some(call_id.into()),
            ..Default::default()
        }
    }

    #[test]
    fn turn_groups_should_pair_reasoning_with_burst_and_results() {
        let messages = vec![reasoning("th"), tool_call("c", "x"), tool_result("r", "x")];
        let groups = compute_turn_groups(&messages);
        assert_eq!(groups.len(), 1);
        let group: BTreeSet<&str> = groups[0].iter().map(|s| s.as_str()).collect();
        let expected: BTreeSet<&str> = ["th", "c", "r"].into_iter().collect();
        assert_eq!(group, expected);
    }

    #[test]
    fn integrity_should_withdraw_turn_when_reasoning_folds_but_call_kept() {
        // INV1：推理被折叠、同一 turn 的调用存活 → 整个 turn 撤回。
        let messages = vec![reasoning("th"), tool_call("c", "x"), tool_result("r", "x")];
        let folded: BTreeSet<String> = ["th".to_string()].into_iter().collect();
        let result = compute_integrity_withdrawals(&messages, &folded);
        assert!(result.withdrawn.contains("th"));
        assert!(result.withdrawn.contains("c"));
        assert!(result.withdrawn.contains("r"));
        assert_eq!(result.split_turn_count, 1);
    }

    #[test]
    fn integrity_should_keep_reverse_direction() {
        // #564：推理+文本存活、调用与结果折叠 → 合法流，不撤回。
        let messages = vec![reasoning("th"), tool_call("c", "x"), tool_result("r", "x")];
        let folded: BTreeSet<String> = ["c".to_string(), "r".to_string()].into_iter().collect();
        let result = compute_integrity_withdrawals(&messages, &folded);
        assert!(result.withdrawn.is_empty(), "{:?}", result.withdrawn);
    }
}
