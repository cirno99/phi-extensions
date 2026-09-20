//! 解压 / 检索 —— 对应 acp-kernel `src/decompress.ts` 与 `src/search.ts`。

use memchr::memmem;

use crate::state::{active_blocks, block_by_id};
use crate::types::{CompressionBlock, CompressionState, CoreMessage};

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

/// 收集一个块直接压缩的原文消息（按给定消息列表过滤）。
pub fn collect_block_content<'a>(
    block: &CompressionBlock,
    messages: &'a [CoreMessage],
) -> Vec<&'a CoreMessage> {
    let ids: std::collections::BTreeSet<&str> = block
        .direct_message_ids
        .iter()
        .map(String::as_str)
        .collect();
    messages
        .iter()
        .filter(|m| ids.contains(m.id.as_str()))
        .collect()
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

/// 构建恢复内容预览（截断到 `max_chars`）。
pub fn build_restored_content_preview(
    block: &CompressionBlock,
    messages: &[CoreMessage],
    max_chars: usize,
) -> String {
    let mut out = String::new();
    for message in collect_block_content(block, messages) {
        if out.chars().count() >= max_chars {
            break;
        }
        let text = message.text_str();
        out.push_str(text);
        out.push('\n');
    }
    if out.chars().count() > max_chars {
        out = out.chars().take(max_chars).collect();
        out.push('…');
    }
    out
}

/// 检索结果。
#[derive(Debug, Clone)]
pub struct ScoredBlock {
    /// 块。
    pub block: CompressionBlock,
    /// 相关度。
    pub score: f64,
}

/// 计算某个词在文本中出现的次数（非重叠）。
fn count_occurrences(haystack: &str, needle: &str) -> usize {
    if haystack.is_empty() || needle.is_empty() {
        return 0;
    }
    // memchr::memmem 使用 SIMD 子串搜索，比逐字符扫描更适合长摘要。
    let haystack_bytes = haystack.as_bytes();
    let needle_bytes = needle.as_bytes();
    let mut count = 0;
    let mut start = 0;
    while start + needle_bytes.len() <= haystack_bytes.len() {
        match memmem::find(&haystack_bytes[start..], needle_bytes) {
            Some(pos) => {
                count += 1;
                start += pos + needle_bytes.len();
            }
            None => break,
        }
    }
    count
}

/// 块相关度打分。
fn score_relevance(block: &CompressionBlock, terms: &[String]) -> f64 {
    let topic = block.topic.clone().unwrap_or_default().to_lowercase();
    let summary = block.summary.to_lowercase();
    let mut score = 0.0f64;
    for term in terms {
        let topic_hits = count_occurrences(&topic, term);
        if topic_hits > 0 {
            score += (topic_hits as f64 * 0.15).min(0.45);
        }
        let summary_hits = count_occurrences(&summary, term);
        if summary_hits > 0 {
            score += (summary_hits as f64 * 0.04).min(0.2);
        }
    }
    score.min(1.0)
}

/// 用 memchr 快速把查询切成小写词元（ASCII 分隔）。
pub fn tokenize_query(query: &str) -> Vec<String> {
    let lowered = query.to_lowercase();
    let mut terms = Vec::new();
    let mut current = String::new();
    for ch in lowered.chars() {
        if ch.is_whitespace() || ch.is_ascii_punctuation() {
            if !current.is_empty() {
                terms.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        terms.push(current);
    }
    terms
}

/// 检索活跃块（相关度 > 0.1，降序）。
pub fn search_blocks(query: &str, state: &CompressionState) -> Vec<CompressionBlock> {
    let terms = tokenize_query(query);
    if terms.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<ScoredBlock> = active_blocks(state)
        .into_iter()
        .map(|block| ScoredBlock {
            score: score_relevance(block, &terms),
            block: block.clone(),
        })
        .filter(|entry| entry.score > 0.1)
        .collect();
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.into_iter().map(|entry| entry.block).collect()
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

    #[test]
    fn search_should_rank_topic_matches() {
        let mut state = crate::state::create_initial_state();
        state.blocks.push(block("b1", "auth token", "nothing here"));
        state.blocks.push(block("b2", "misc", "unrelated content"));
        let hits = search_blocks("auth token", &state);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].block_id, "b1");
    }

    #[test]
    fn search_should_return_empty_for_blank_query() {
        let mut state = crate::state::create_initial_state();
        state.blocks.push(block("b1", "auth", "auth"));
        assert!(search_blocks("   ", &state).is_empty());
    }
}
