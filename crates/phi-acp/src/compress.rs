//! 压缩内核 —— 对应 acp-kernel `src/compress.ts`（`createCore`）。
//!
//! 这是「压缩算法内核」的主体：`process_turn` 跑节点管线，`apply_compression`
//! 应用模型写出的摘要。

use std::collections::{BTreeMap, BTreeSet};

use crate::block_map::format_created_blocks;
use crate::boundaries::{resolve_boundaries, BoundaryErrorKind, ResolvedRange};
use crate::nudge::{compute_context_breakdown, decide_nudge, NudgeInput};
use crate::protected::{
    collect_latest_protected, is_message_latest_protected, is_message_protected,
};
use crate::prune::{is_summary_message_id, prune};
use crate::recommend::{
    build_compressible_ranges, compute_protected_refs, merge_ranges_to_threshold,
};
use crate::refs::{assign_refs, highest_used_index, AssignRefsOptions};
use crate::render::{render_with_snapshot, RenderStrategy};
use crate::state::{
    active_blocks, advance_survival, allocate_block_id, allocate_run_id, block_by_id,
};
use crate::tokenize::count_message_tokens;
use crate::truncate::{truncate_large_tool_outputs, TruncateOptions};
use crate::types::{
    ApplyCompressionOutcome, ApplyResult, CompressRangeSpec, CompressionBlock, CompressionState,
    CompressionTier, Config, ContentType, CoreMessage, ProcessTurnOutcome, Recommendation,
    StatusReport, TerminalEscapeSignal,
};

/// 深拷贝状态（避免改动调用方传入的状态）。
pub fn clone_state(state: &CompressionState) -> CompressionState {
    state.clone()
}

fn collect_coverage(state: &CompressionState) -> BTreeSet<String> {
    let mut coverage = BTreeSet::new();
    for block in active_blocks(state) {
        for id in &block.effective_message_ids {
            coverage.insert(id.clone());
        }
    }
    coverage
}

fn resolve_target_tier(
    state: &CompressionState,
    nested_block_ids: &[String],
    is_block_boundary: bool,
) -> CompressionTier {
    if !is_block_boundary || nested_block_ids.is_empty() {
        return 1;
    }
    let mut min_tier: CompressionTier = 3;
    for id in nested_block_ids {
        if let Some(block) = block_by_id(state, id) {
            if block.tier < min_tier {
                min_tier = block.tier;
            }
        }
    }
    min_tier
}

/// 从可压缩集合中剔除受保护的工具消息（及其配对结果）。
fn filter_protected_tool_messages(
    direct_ids: &[String],
    messages: &[CoreMessage],
    config: &Config,
) -> Vec<String> {
    let mut protected_call_ids: BTreeSet<String> = BTreeSet::new();
    let latest = collect_latest_protected(messages, config);
    for id in &latest.call_ids {
        protected_call_ids.insert(id.clone());
    }
    for message in messages {
        if is_message_protected(message, config) {
            if let Some(call_id) = message.tool_call_id.as_deref() {
                protected_call_ids.insert(call_id.to_string());
            }
        }
    }

    let mut removed: BTreeSet<&str> = BTreeSet::new();
    for id in direct_ids {
        let Some(message) = messages.iter().find(|m| &m.id == id) else {
            continue;
        };
        if is_message_protected(message, config) || is_message_latest_protected(message, &latest) {
            removed.insert(id.as_str());
            if let Some(call_id) = message.tool_call_id.as_deref() {
                protected_call_ids.insert(call_id.to_string());
            }
        }
    }
    for id in direct_ids {
        if removed.contains(id.as_str()) {
            continue;
        }
        let Some(message) = messages.iter().find(|m| &m.id == id) else {
            continue;
        };
        if message.content_type == ContentType::ToolResult {
            if let Some(call_id) = message.tool_call_id.as_deref() {
                if protected_call_ids.contains(call_id) {
                    removed.insert(id.as_str());
                }
            }
        }
    }
    direct_ids
        .iter()
        .filter(|id| !removed.contains(id.as_str()))
        .cloned()
        .collect()
}

/// 消息边界时把范围扩张到完整的工具配对与推理运行。
fn apply_pair_boundary_adjustments(
    resolved: &ResolvedRange,
    messages: &[CoreMessage],
) -> Vec<String> {
    if resolved.boundary_kind == Some(crate::boundaries::BoundaryKind::Block) {
        return resolved.message_ids.clone();
    }
    let mut start = resolved.start_index;
    let mut end = resolved.end_index;
    for _ in 0..2 {
        let (s1, e1) = adjust_reasoning(start, end, messages);
        let (s2, e2) = adjust_tool_pairs(s1, e1, messages);
        if s2 == start && e2 == end {
            start = s2;
            end = e2;
            break;
        }
        start = s2;
        end = e2;
    }
    if start == resolved.start_index && end == resolved.end_index {
        return resolved.message_ids.clone();
    }
    messages[start..=end].iter().map(|m| m.id.clone()).collect()
}

fn adjust_reasoning(start: usize, end: usize, messages: &[CoreMessage]) -> (usize, usize) {
    let mut s = start;
    // 若范围内含 assistant 文本 / 工具调用，把其前置推理运行一并纳入。
    while s > 0 && messages[s - 1].content_type == ContentType::Reasoning {
        s -= 1;
    }
    (s, end)
}

fn adjust_tool_pairs(start: usize, end: usize, messages: &[CoreMessage]) -> (usize, usize) {
    let mut s = start;
    let mut e = end;
    let last = messages.len().saturating_sub(1);
    let mut changed = true;
    while changed {
        changed = false;
        // 若包含工具调用，纳入其配对结果。
        let lo = s;
        let hi = e.min(last);
        for i in lo..=hi {
            if messages[i].content_type != ContentType::ToolCall {
                continue;
            }
            let Some(call_id) = messages[i].tool_call_id.as_deref() else {
                continue;
            };
            if let Some(result_index) = messages.iter().position(|m| {
                m.content_type == ContentType::ToolResult
                    && m.tool_call_id.as_deref() == Some(call_id)
            }) {
                if result_index > e {
                    e = result_index;
                    changed = true;
                } else if result_index < s {
                    s = result_index;
                    changed = true;
                }
            }
        }
        // 若包含工具结果，纳入其配对调用。
        let lo = s;
        let hi = e.min(last);
        for i in lo..=hi {
            if messages[i].content_type != ContentType::ToolResult {
                continue;
            }
            let Some(call_id) = messages[i].tool_call_id.as_deref() else {
                continue;
            };
            if let Some(call_index) = messages.iter().position(|m| {
                m.content_type == ContentType::ToolCall
                    && m.tool_call_id.as_deref() == Some(call_id)
            }) {
                if call_index < s {
                    s = call_index;
                    changed = true;
                } else if call_index > e {
                    e = call_index;
                    changed = true;
                }
            }
        }
    }
    (s, e)
}

fn validate_compression_range(
    spec: &CompressRangeSpec,
    direct_count: usize,
    consumed_count: usize,
    config: &Config,
) -> Result<(), String> {
    let summary = spec.summary.trim();
    if summary.is_empty() {
        return Err(
            "Summary is empty — provide a meaningful summary of the compressed range.".into(),
        );
    }
    let cfg = &config.compress;
    if cfg.min_summary_length > 0 && summary.chars().count() < cfg.min_summary_length {
        return Err(format!(
            "Summary too short ({} chars, min {}). The summary must capture the compressed range's key information.",
            summary.chars().count(),
            cfg.min_summary_length
        ));
    }
    let effective_max = spec.summary_max_chars.unwrap_or(cfg.max_summary_length);
    if effective_max > 0 && summary.chars().count() > effective_max {
        return Err(format!(
            "Summary too long ({} chars, max {effective_max}). Strip noise — keep critical paths, decisions, errors, and code references.",
            summary.chars().count()
        ));
    }
    if direct_count == 0 && consumed_count == 0 {
        return Err(
            "Range contains no compressible messages — all are already covered by active blocks or protected."
                .into(),
        );
    }
    Ok(())
}

struct SingleRangeInput<'a> {
    spec: &'a CompressRangeSpec,
    messages: &'a [CoreMessage],
    state: &'a mut CompressionState,
    run_id: &'a str,
    config: &'a Config,
    protected_message_ids: &'a BTreeSet<String>,
    pre_existing_coverage: &'a BTreeSet<String>,
}

fn apply_single_range(
    input: SingleRangeInput<'_>,
) -> Result<(u64, Vec<String>, Vec<String>), String> {
    let mut warnings: Vec<String> = Vec::new();
    let resolved = resolve_boundaries(
        &input.spec.start_ref,
        &input.spec.end_ref,
        input.messages,
        input.state,
    )
    .map_err(|e| e.message)?;

    let range_message_ids: Vec<String> = apply_pair_boundary_adjustments(&resolved, input.messages)
        .into_iter()
        .filter(|id| !is_summary_message_id(id))
        .collect();

    let is_block_boundary = resolved.boundary_kind == Some(crate::boundaries::BoundaryKind::Block);
    let target_tier =
        resolve_target_tier(input.state, &resolved.nested_block_ids, is_block_boundary);
    let output_tier: CompressionTier = if is_block_boundary {
        (target_tier + 1).min(3)
    } else {
        1
    };

    let consumed_block_ids: Vec<String> = resolved
        .nested_block_ids
        .iter()
        .filter(|id| {
            block_by_id(input.state, id)
                .map(|b| b.active && b.tier == target_tier)
                .unwrap_or(false)
        })
        .cloned()
        .collect();

    let mut effective: BTreeSet<String> = range_message_ids.iter().cloned().collect();
    for consumed_id in &consumed_block_ids {
        if let Some(consumed) = block_by_id(input.state, consumed_id) {
            for id in &consumed.effective_message_ids {
                effective.insert(id.clone());
            }
        }
    }

    let mut direct_ids: Vec<String> = effective
        .iter()
        .filter(|id| !input.pre_existing_coverage.contains(*id))
        .cloned()
        .collect();
    direct_ids.sort();

    let filtered = filter_protected_tool_messages(&direct_ids, input.messages, input.config);
    if filtered.len() < direct_ids.len() {
        let kept: BTreeSet<&String> = filtered.iter().collect();
        for id in &direct_ids {
            if !kept.contains(id) {
                effective.remove(id);
            }
        }
        direct_ids = filtered;
    }

    // 软保护：从范围内剔除保护区消息（而非整体失败）。
    let hit_protected: Vec<String> = direct_ids
        .iter()
        .filter(|id| {
            input
                .state
                .message_refs
                .by_raw
                .get(*id)
                .map(|r| input.protected_message_ids.contains(r))
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    if !hit_protected.is_empty() {
        let protected_set: BTreeSet<&String> = hit_protected.iter().collect();
        direct_ids.retain(|id| !protected_set.contains(id));
        for id in &hit_protected {
            effective.remove(id);
        }
        if direct_ids.is_empty() && consumed_block_ids.is_empty() {
            return Err(format!(
                "Range is entirely within the protected zone (the last {} messages and/or the most recent user message). Adjust startId/endId to older messages.",
                input.config.preserve_recent_messages
            ));
        }
        warnings.push(format!(
            "Excluded {} protected message(s) from compression range (recent/last-user zone).",
            hit_protected.len()
        ));
    }

    // 空改写活锁护栏。
    if !is_block_boundary && direct_ids.is_empty() && !consumed_block_ids.is_empty() {
        return Err(format!(
            "Range {}..{} contains no new compressible messages — every message in it is already covered by active block(s) {}. To rewrite or merge those blocks, reference them by block ID.",
            input.spec.start_ref,
            input.spec.end_ref,
            consumed_block_ids.join(", ")
        ));
    }

    validate_compression_range(
        input.spec,
        direct_ids.len(),
        consumed_block_ids.len(),
        input.config,
    )?;

    let mut compressed_tokens = 0u64;
    for id in &direct_ids {
        if let Some(message) = input.messages.iter().find(|m| &m.id == id) {
            compressed_tokens += count_message_tokens(message);
        }
    }
    for consumed_id in &consumed_block_ids {
        if let Some(consumed) = block_by_id(input.state, consumed_id) {
            compressed_tokens += crate::tokenize::count_tokens(&consumed.summary);
        }
    }

    let block_id = allocate_block_id(input.state);
    let block = CompressionBlock {
        block_id,
        run_id: input.run_id.to_string(),
        tier: output_tier,
        topic: input.spec.topic.clone(),
        summary: input.spec.summary.clone(),
        direct_message_ids: direct_ids,
        effective_message_ids: effective.iter().cloned().collect(),
        direct_block_ids: consumed_block_ids.clone(),
        compressed_tokens,
        created_at: crate::time_now_ms(),
        survived_count: 0,
        generation: crate::types::BlockGeneration::Young,
        active: true,
        compress_call_id: input.spec.compress_call_id.clone(),
        start_ref: Some(input.spec.start_ref.clone()),
        end_ref: Some(input.spec.end_ref.clone()),
        ..Default::default()
    };
    input.state.blocks.push(block);

    for consumed_id in &consumed_block_ids {
        if let Some(consumed) = input
            .state
            .blocks
            .iter_mut()
            .find(|b| &b.block_id == consumed_id)
        {
            consumed.active = false;
        }
    }

    Ok((compressed_tokens, warnings, consumed_block_ids))
}

/// 应用一批压缩范围。
pub fn apply_compression(
    ranges: &[CompressRangeSpec],
    messages: &[CoreMessage],
    state: &CompressionState,
    config: &Config,
    protected_message_ids: Option<&BTreeSet<String>>,
) -> ApplyCompressionOutcome {
    let mut work = clone_state(state);
    let run_id = allocate_run_id(&mut work);
    let mut blocks_created = 0usize;
    let mut tokens_compressed = 0u64;
    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    let protected = match protected_message_ids {
        Some(ids) => ids.clone(),
        None => compute_protected_refs(messages, state, config),
    };

    let pre_existing_coverage = collect_coverage(state);

    // 分类每个范围。
    enum Resolution {
        Ok(ResolvedRange),
        Consumed(String),
        Unknown(String),
        Invalid(String),
    }
    let mut classifications: Vec<Resolution> = Vec::with_capacity(ranges.len());
    let mut consumed_indices: Vec<usize> = Vec::new();
    for (i, spec) in ranges.iter().enumerate() {
        match resolve_boundaries(&spec.start_ref, &spec.end_ref, messages, &work) {
            Ok(resolved) => classifications.push(Resolution::Ok(resolved)),
            Err(e) => {
                let msg = format!("range {}..{}: {}", spec.start_ref, spec.end_ref, e.message);
                if e.kind == BoundaryErrorKind::Consumed {
                    consumed_indices.push(i);
                    classifications.push(Resolution::Consumed(msg));
                } else if e.kind == BoundaryErrorKind::Unknown {
                    classifications.push(Resolution::Unknown(msg));
                } else {
                    classifications.push(Resolution::Invalid(msg));
                }
            }
        }
    }

    let resolvable_count = classifications
        .iter()
        .filter(|r| matches!(r, Resolution::Ok(_)))
        .count();
    let unknown_count = classifications
        .iter()
        .filter(|r| matches!(r, Resolution::Unknown(_)))
        .count();

    // 重叠检测（按数组下标，最早者优先）。
    let mut spans: Vec<(usize, usize, usize)> = Vec::new();
    for (i, r) in classifications.iter().enumerate() {
        if let Resolution::Ok(resolved) = r {
            spans.push((resolved.start_index, resolved.end_index, i));
        }
    }
    spans.sort_by_key(|(start, _, _)| *start);
    let mut skip: BTreeSet<usize> = BTreeSet::new();
    let mut accepted_max = -1i64;
    for (start, end, i) in &spans {
        if (*start as i64) <= accepted_max {
            skip.insert(*i);
            warnings.push(format!(
                "Skipped range ({}..{}) — overlaps an earlier range in the batch; the earlier range takes precedence. Keep ranges disjoint.",
                ranges[*i].start_ref, ranges[*i].end_ref
            ));
            continue;
        }
        if (*end as i64) > accepted_max {
            accepted_max = *end as i64;
        }
    }

    // minCompressRange 门槛。
    if config.compress.min_compress_range > 0 && !ranges.is_empty() {
        let mut total_chars = 0usize;
        let mut has_block_boundary = false;
        let mut counted = 0usize;
        for (i, r) in classifications.iter().enumerate() {
            if skip.contains(&i) {
                continue;
            }
            if let Resolution::Ok(resolved) = r {
                if resolved.boundary_kind == Some(crate::boundaries::BoundaryKind::Block) {
                    has_block_boundary = true;
                    continue;
                }
                counted += 1;
                for id in &resolved.message_ids {
                    if let Some(m) = messages.iter().find(|m| &m.id == id) {
                        total_chars += m.text_str().chars().count();
                    }
                }
            }
        }
        if !has_block_boundary && total_chars < config.compress.min_compress_range {
            let gate = if resolvable_count == 0 && consumed_indices.is_empty() && unknown_count > 0
            {
                format!(
                    "None of the {} requested range(s) resolved — every ref is unknown to this session. Refs are per-session snapshots; run acp_status, then call the compress tool again using only the refs it reports.",
                    ranges.len()
                )
            } else if !consumed_indices.is_empty() {
                format!(
                    "Requested range(s) already compressed (e.g. {}..{}) — those refs no longer point to directly compressible content. Run acp_status, then retry with the CURRENT compressible ranges.",
                    ranges[consumed_indices[0]].start_ref, ranges[consumed_indices[0]].end_ref
                )
            } else {
                format!(
                    "Total compressible content too small ({total_chars} chars across {counted} range(s), min {}). Combine more messages into your range(s).",
                    config.compress.min_compress_range
                )
            };
            return ApplyCompressionOutcome {
                state: state.clone(),
                result: ApplyResult {
                    blocks_created: 0,
                    tokens_compressed: 0,
                    errors: vec![gate],
                    warnings: Vec::new(),
                },
            };
        }
    }

    for (i, spec) in ranges.iter().enumerate() {
        if skip.contains(&i) {
            continue;
        }
        match &classifications[i] {
            Resolution::Ok(resolved) => {
                warnings.extend(resolved.snapped_boundaries.iter().cloned());
                let outcome = apply_single_range(SingleRangeInput {
                    spec,
                    messages,
                    state: &mut work,
                    run_id: &run_id,
                    config,
                    protected_message_ids: &protected,
                    pre_existing_coverage: &pre_existing_coverage,
                });
                match outcome {
                    Ok((tokens, mut warns, _)) => {
                        blocks_created += 1;
                        tokens_compressed += tokens;
                        warnings.append(&mut warns);
                    }
                    Err(e) => {
                        errors.push(format!("range {}..{}: {e}", spec.start_ref, spec.end_ref));
                    }
                }
            }
            Resolution::Consumed(msg) => {
                warnings.push(format!(
                    "Skipped range ({}..{}) — already compressed ({msg}); nothing to compress.",
                    spec.start_ref, spec.end_ref
                ));
            }
            Resolution::Unknown(msg) | Resolution::Invalid(msg) => errors.push(msg.clone()),
        }
    }

    work.stats.compression_count += blocks_created as u64;
    work.stats.tokens_compressed += tokens_compressed;

    if blocks_created > 0 {
        work.nudge.last_per_message_nudge_tokens = 0;
        work.nudge.last_nudge_shown_tokens = 0;
        work.nudge.last_shown_by_tier.clear();
        work.terminal_streak = Some(0);
    }

    ApplyCompressionOutcome {
        state: work,
        result: ApplyResult {
            blocks_created,
            tokens_compressed,
            errors,
            warnings,
        },
    }
}

/// 同步块：失活被消费 / 消失的块。
pub fn sync_blocks(messages: &[CoreMessage], state: &CompressionState) -> CompressionState {
    let present: BTreeSet<&str> = messages.iter().map(|m| m.id.as_str()).collect();
    let mut result = clone_state(state);

    let mut consumed: BTreeSet<String> = BTreeSet::new();
    for block in &result.blocks {
        for id in &block.direct_block_ids {
            consumed.insert(id.clone());
        }
    }

    for block in &mut result.blocks {
        if consumed.contains(&block.block_id) {
            block.active = false;
            continue;
        }
        if block.expanded == Some(true) {
            block.active = false;
            continue;
        }
        block.active = true;
        let still_present = block
            .effective_message_ids
            .iter()
            .any(|id| present.contains(id.as_str()))
            || present.contains(crate::prune::summary_message_id(&block.block_id).as_str());
        if !still_present {
            block.active = false;
        }
    }

    // 按当前存在的 ref 裁剪 token 快照。
    let live_refs: BTreeSet<String> = messages
        .iter()
        .filter_map(|m| result.message_refs.by_raw.get(&m.id).cloned())
        .collect();
    if result.token_snapshot.len() != live_refs.len() {
        result
            .token_snapshot
            .retain(|reference, _| live_refs.contains(reference));
    }

    result
}

/// 隐藏已被消费的 compress 调用（其摘要已入块）。
fn hide_consumed_compress_calls(
    messages: &[CoreMessage],
    state: &CompressionState,
) -> Vec<CoreMessage> {
    let consumed_call_ids: BTreeSet<&str> = state
        .blocks
        .iter()
        .filter(|b| b.active)
        .filter_map(|b| b.compress_call_id.as_deref())
        .collect();
    if consumed_call_ids.is_empty() {
        return messages.to_vec();
    }
    let hidden_result_ids: BTreeSet<&str> = messages
        .iter()
        .filter(|m| m.content_type == ContentType::ToolCall)
        .filter(|m| m.tool_name.as_deref() == Some("compress"))
        .filter(|m| {
            m.tool_call_id
                .as_deref()
                .map(|id| consumed_call_ids.contains(id))
                .unwrap_or(false)
        })
        .filter_map(|m| m.tool_call_id.as_deref())
        .collect();
    messages
        .iter()
        .filter(|m| {
            !(m.content_type == ContentType::ToolResult
                && m.tool_call_id
                    .as_deref()
                    .map(|id| hidden_result_ids.contains(id))
                    .unwrap_or(false))
        })
        .cloned()
        .collect()
}

fn assign_refs_node(
    messages: &[CoreMessage],
    state: &CompressionState,
    config: &Config,
) -> CompressionState {
    let has_protection =
        !config.protected_tools.is_empty() || !config.protected_latest_tools.is_empty();
    let latest = if has_protection {
        Some(collect_latest_protected(messages, config))
    } else {
        None
    };
    let protected_fn = |m: &CoreMessage| -> bool {
        is_message_protected(m, config)
            || latest
                .as_ref()
                .map(|l| is_message_latest_protected(m, l))
                .unwrap_or(false)
    };
    let existing = state.message_refs.clone();
    let result = assign_refs(
        messages,
        AssignRefsOptions {
            existing: &existing,
            next_index: highest_used_index(&existing) + 1,
            is_protected: has_protection.then_some(&protected_fn as &dyn Fn(&CoreMessage) -> bool),
        },
    );
    let mut next = clone_state(state);
    next.message_refs = result.map;
    next
}

/// 跑完整节点管线（对应 `processTurn`）。
pub fn process_turn(
    messages: &[CoreMessage],
    state: &CompressionState,
    config: &Config,
    token_count: u64,
    render_tags: RenderStrategy,
) -> ProcessTurnOutcome {
    // 1. assign-refs
    let mut work = assign_refs_node(messages, state, config);

    // 2. sync-blocks + advance-survival
    work = sync_blocks(messages, &work);
    advance_survival(&mut work, config.promotion_threshold);

    // 3. prune
    let mut current = prune(messages, &work);

    // 4. hide-compress-calls
    current = hide_consumed_compress_calls(&current, &work);

    // 5. recommend
    let protected_refs = compute_protected_refs(&current, &work, config);
    let context_ranges = build_compressible_ranges(&current, &work, config, &protected_refs);
    let recommendation = Recommendation {
        nothing_to_compress: context_ranges.compressible.is_empty(),
        recommended_ranges: merge_ranges_to_threshold(
            &context_ranges.compressible,
            config.compress.min_compress_range,
        ),
        context_ranges,
    };

    // 6. nudge-inject
    let mut nudge = decide_nudge(NudgeInput {
        token_count,
        config,
        state: &work,
        messages: &current,
        recommendation: Some(&recommendation),
    });
    {
        let baseline = work.nudge.last_per_message_nudge_tokens;
        let growth = crate::nudge::resolve_adaptive_growth(config.model_context_limit, config);
        if baseline > 0 && token_count < baseline.saturating_sub(growth) {
            work.nudge.last_per_message_nudge_tokens = token_count;
            work.nudge.last_nudge_shown_tokens = 0;
            work.nudge.last_shown_by_tier.clear();
        }
        if work.nudge.last_per_message_nudge_tokens == 0 {
            work.nudge.last_per_message_nudge_tokens = token_count;
        }
        if nudge.should_inject {
            work.nudge.last_nudge_shown_tokens = token_count;
            if let Some(tier) = nudge.tier {
                work.nudge.last_shown_by_tier.insert(tier, token_count);
            }
        }
    }

    // 7. emergency-truncate
    let mut terminal_escape: Option<TerminalEscapeSignal> = None;
    let mut truncation_skipped: Option<String> = None;
    let usage = if config.model_context_limit > 0 {
        token_count as f64 / config.model_context_limit as f64
    } else {
        0.0
    };
    let prev_streak = work.terminal_streak.unwrap_or(0);
    if usage < config.truncate.threshold {
        if prev_streak > 0 {
            work.terminal_streak = Some(0);
        }
    } else {
        let trunc = truncate_large_tool_outputs(
            &current,
            token_count,
            config,
            &TruncateOptions {
                protect_recent_messages: config.preserve_recent_messages,
                include_text_messages: true,
                ..Default::default()
            },
        );
        current = trunc.messages;
        let min_benefit = nudge.breakdown.min_pressure_benefit;
        let max_pending = nudge.breakdown.max_pending;
        let no_viable = if min_benefit > 0 {
            max_pending < min_benefit
        } else {
            max_pending == 0
        };
        let stuck = no_viable && trunc.saved_tokens == 0;
        let streak = if stuck { prev_streak + 1 } else { 0 };
        work.terminal_streak = Some(streak);
        let escape_after = config.truncate.terminal_escape_after.unwrap_or(3);
        if stuck && escape_after > 0 && streak >= escape_after {
            terminal_escape = Some(TerminalEscapeSignal {
                message: format!(
                    "Usage at {}% ({token_count}/{} tokens) persists with nothing compressible above the benefit floor and no truncatable content: compression cannot reduce this context below the limit. Start a new session or use native compaction.",
                    (usage * 100.0).round(),
                    config.model_context_limit
                ),
                usage,
                token_count,
                model_context_limit: config.model_context_limit,
                stuck_events: streak,
            });
        }
        if trunc.saved_tokens == 0 {
            truncation_skipped = Some(if trunc.candidates_found == 0 {
                format!(
                    "emergency-truncate ran at {}% usage but found no truncatable content",
                    (usage * 100.0).round()
                )
            } else {
                format!(
                    "emergency-truncate found {} candidate(s) at {}% usage but none were large enough to save tokens",
                    trunc.candidates_found,
                    (usage * 100.0).round()
                )
            });
        }
    }

    // 8. render-refs
    let (rendered, snapshot) = render_with_snapshot(&current, &work, render_tags);
    if snapshot.len() != work.token_snapshot.len() {
        work.token_snapshot = snapshot;
    }

    // 填充提醒的上下文细分。
    let growth = nudge.breakdown.growth;
    let bd = compute_context_breakdown(&rendered, token_count, growth);
    nudge.context_usage = usage;

    let _ = bd;
    ProcessTurnOutcome {
        messages: rendered,
        state: work,
        nudge: Some(nudge),
        terminal_escape,
        truncation_skipped,
    }
}

/// 状态报告。
pub fn status(state: &CompressionState, token_count: u64, config: &Config) -> StatusReport {
    let active = active_blocks(state);
    let usage = if config.model_context_limit > 0 {
        token_count as f64 / config.model_context_limit as f64
    } else {
        0.0
    };
    let mut breakdown = BTreeMap::new();
    breakdown.insert("active".to_string(), active.len() as u64);
    breakdown.insert("total".to_string(), state.blocks.len() as u64);
    StatusReport {
        context_usage: usage,
        token_count,
        model_context_limit: config.model_context_limit,
        active_blocks: active.len(),
        total_blocks: state.blocks.len(),
        tokens_compressed: state.stats.tokens_compressed,
        breakdown,
    }
}

/// 格式化的已建块摘要（供工具输出）。
pub fn created_blocks_summary(state: &CompressionState, before: usize) -> String {
    let new_blocks: Vec<CompressionBlock> = state.blocks.iter().skip(before).cloned().collect();
    format_created_blocks(state, &new_blocks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Role;

    fn user(id: &str, text: &str) -> CoreMessage {
        CoreMessage::text(id, Role::User, text)
    }

    fn with_refs(messages: &[CoreMessage]) -> CompressionState {
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
    fn apply_should_create_block_and_cover_messages() {
        let big = "x".repeat(6000);
        let messages = vec![
            CoreMessage::text("a", Role::Assistant, big.clone()),
            CoreMessage::text("b", Role::Assistant, big),
            user("u", "last user"),
        ];
        let state = with_refs(&messages);
        let mut config = Config::default_for(200_000);
        config.preserve_recent_messages = 0;
        config.preserve_recent_tokens = 0;
        let spec = CompressRangeSpec {
            start_ref: "m00001".into(),
            end_ref: "m00002".into(),
            summary: "A sufficiently long summary that captures the essence of the two large assistant messages that were compressed here.".into(),
            ..Default::default()
        };
        let outcome = apply_compression(&[spec], &messages, &state, &config, None);
        assert_eq!(outcome.result.blocks_created, 1);
        assert!(
            outcome.result.errors.is_empty(),
            "{:?}",
            outcome.result.errors
        );
        assert_eq!(outcome.state.blocks.len(), 1);
        assert!(outcome.state.blocks[0]
            .effective_message_ids
            .contains(&"a".to_string()));
    }

    #[test]
    fn apply_should_reject_unknown_ref() {
        let messages = vec![user("u", "hi")];
        let state = with_refs(&messages);
        let config = Config::default_for(200_000);
        let spec = CompressRangeSpec {
            start_ref: "m00099".into(),
            end_ref: "m00099".into(),
            summary:
                "summary text that is long enough to pass the minimum length validation check ok"
                    .into(),
            ..Default::default()
        };
        let outcome = apply_compression(&[spec], &messages, &state, &config, None);
        assert_eq!(outcome.result.blocks_created, 0);
        assert!(!outcome.result.errors.is_empty());
    }

    #[test]
    fn process_turn_should_assign_refs_and_render() {
        let messages = vec![user("u", "hello world")];
        let state = crate::state::create_initial_state();
        let config = Config::default_for(200_000);
        let outcome = process_turn(&messages, &state, &config, 10, RenderStrategy::All);
        assert!(outcome.state.message_refs.by_raw.contains_key("u"));
        assert!(outcome.messages[0].text_str().contains("m00001"));
    }

    #[test]
    fn sync_should_deactivate_consumed_parent() {
        let mut state = crate::state::create_initial_state();
        state.blocks.push(CompressionBlock {
            block_id: "b1".into(),
            active: true,
            direct_block_ids: vec![],
            effective_message_ids: vec!["a".into()],
            ..Default::default()
        });
        state.blocks.push(CompressionBlock {
            block_id: "b2".into(),
            active: true,
            direct_block_ids: vec!["b1".into()],
            effective_message_ids: vec!["a".into()],
            ..Default::default()
        });
        let messages = vec![user("a", "x")];
        let synced = sync_blocks(&messages, &state);
        assert!(!synced.blocks[0].active);
        assert!(synced.blocks[1].active);
    }

    #[test]
    fn status_should_report_counts() {
        let state = crate::state::create_initial_state();
        let config = Config::default_for(1000);
        let report = status(&state, 250, &config);
        assert!((report.context_usage - 0.25).abs() < 1e-9);
        assert_eq!(report.total_blocks, 0);
    }
}
