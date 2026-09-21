//! 检索 —— 对应 acp-kernel `src/search/`（`index.ts` 的公开入口）。
//!
//! 对统一文档集（块摘要 + 历史消息）打分并返回排序结果。模型用检索廉价定位被
//! 压缩折叠成摘要的细节，再解压拥有它的块拿全文——这就是上游的
//! **search → decompress 闭环**。
//!
//! # 两个数据源
//!
//! | 源 | 检索文本 | ref | 何时 |
//! |---|---|---|---|
//! | 块 | 摘要（+ topic） | `b3` | **活跃与失活块都检索**（上游的 inactive fix） |
//! | 消息 | 会话日志里的原文 | `m00350` | 被压缩折叠掉的细节 |
//!
//! # 与旧实现的差异（为什么换掉）
//!
//! 本扩展此前的 `acp_search` 是 acp-kernel `compress.ts::scoreRelevance` 的逐字
//! 移植——即上游的**遗留**子串路径（`core.search`）。它有三个已知短板，上游
//! 在 `search/SEARCH.md` 里逐条点名：
//! 1. 只看**活跃**块（失活块里的历史再也搜不到）；
//! 2. 只做子串计数，短词过度命中（`the` ≈ `theater`），不懂形态（`compressed`
//!    搜不到 `compression`）、不懂错拼、不懂 CJK 词边界；
//! 3. 不检索消息。
//!
//! 本模块改为上游推荐的生产路径 [`search_blocks`]（`hybrid` = 0.7·BM25 + 0.3·fuzzy），
//! 上游自测 MRR 0.898 vs 遗留路径 0.797。

pub mod algorithms;
pub mod doc_cache;
pub mod registry;
pub mod stemmer;
pub mod tokenizer;
pub mod types;

use std::cmp::Ordering;
use std::collections::HashMap;

use crate::refs::BLOCKED_REF;
use crate::state::active_blocks;
use crate::tokenize::count_message_tokens;
use crate::truncate::{clamp_prefix, clamp_window};
use crate::types::{CompressionState, ContentType, CoreMessage, Role};

pub use types::{
    MessageRole, ScoredDoc, SearchAlgorithm, SearchDoc, SearchDocKind, SearchOptions, SearchResult,
    DEFAULT_ALGORITHM, DEFAULT_LIMIT, DEFAULT_MIN_SCORE, DEFAULT_PREVIEW_LENGTH,
};

/// 由状态里的**全部**块（活跃与失活）构造检索文档。
pub fn block_docs(state: &CompressionState) -> Vec<SearchDoc> {
    state
        .blocks
        .iter()
        .map(|block| SearchDoc {
            kind: SearchDocKind::Block,
            reference: block.block_id.clone(),
            text: format!(
                "{} {}",
                block.topic.clone().unwrap_or_default(),
                block.summary
            ),
            title: block
                .topic
                .clone()
                .unwrap_or_else(|| block.block_id.clone()),
            role: None,
            block_id: Some(block.block_id.clone()),
            tier: Some(block.tier),
            tokens: Some(block.compressed_tokens),
        })
        .collect()
}

/// 由宿主历史消息构造检索文档。
///
/// phi 扩展拿不到宿主的完整会话日志，只有自己观测到的消息视图（用户输入 +
/// 工具调用/结果）。每条消息的 ref 来自 `state.message_refs`；若它已被某个活跃
/// 块覆盖，`block_id` 指向那个块，模型据此知道该解压哪个块看全文。
pub fn message_docs(messages: &[CoreMessage], state: &CompressionState) -> Vec<SearchDoc> {
    // 反查：被覆盖的消息 id → 覆盖它的活跃块（最外层优先，先到先得）。
    let mut owner: HashMap<&str, (&str, u8)> = HashMap::new();
    for block in active_blocks(state) {
        for id in &block.effective_message_ids {
            owner
                .entry(id.as_str())
                .or_insert((&block.block_id, block.tier));
        }
    }

    messages
        .iter()
        .filter_map(|message| {
            let reference = state.message_refs.by_raw.get(&message.id)?;
            if reference == BLOCKED_REF {
                return None;
            }
            // 摘要消息不是「历史原文」，它已由块自身代表。
            if message.role == Role::System || message.content_type == ContentType::Reasoning {
                return None;
            }
            let text = message.text_str().to_string();
            if text.trim().is_empty() {
                return None;
            }
            let role = match message.role {
                Role::User => MessageRole::User,
                Role::Assistant => MessageRole::Assistant,
                Role::Tool | Role::System => MessageRole::Tool,
            };
            let (block_id, tier) = match owner.get(message.id.as_str()) {
                Some((block_id, tier)) => (Some((*block_id).to_string()), Some(*tier)),
                None => (None, None),
            };
            Some(SearchDoc {
                kind: SearchDocKind::Message,
                reference: reference.clone(),
                title: format!("{}: {}", role.as_str(), clamp_prefix(&text, 60)),
                text,
                role: Some(role),
                block_id,
                tier,
                tokens: Some(count_message_tokens(message)),
            })
        })
        .collect()
}

/// 按角色乘权重。
fn apply_role_weight(
    scored: Vec<ScoredDoc>,
    docs: &[SearchDoc],
    weights: types::RoleWeights,
) -> Vec<ScoredDoc> {
    if docs.is_empty() {
        return scored;
    }
    let doc_by_ref: HashMap<&str, &SearchDoc> = docs
        .iter()
        .map(|doc| (doc.reference.as_str(), doc))
        .collect();
    scored
        .into_iter()
        .map(|entry| {
            let Some(doc) = doc_by_ref.get(entry.reference.as_str()) else {
                return entry;
            };
            let weight = match doc.kind {
                SearchDocKind::Message => match doc.role {
                    Some(MessageRole::User) => weights.user,
                    Some(MessageRole::Assistant) => weights.assistant,
                    _ => weights.tool,
                },
                SearchDocKind::Block => weights.block,
            };
            ScoredDoc {
                reference: entry.reference,
                score: entry.score * weight,
            }
        })
        .collect()
}

/// 对文档集执行一次检索（对应上游 `searchBlocks`）。
pub fn search_blocks(
    docs: &[SearchDoc],
    query: &str,
    options: &SearchOptions,
) -> Vec<SearchResult> {
    let limit = options.limit.unwrap_or(DEFAULT_LIMIT);
    let preview_length = options.preview_length.unwrap_or(DEFAULT_PREVIEW_LENGTH);
    let min_score = options.min_score.unwrap_or(DEFAULT_MIN_SCORE);
    let algorithm_name = options.algorithm.as_deref().unwrap_or(DEFAULT_ALGORITHM);
    let weights = options.role_weights.unwrap_or_default();

    let Some(algorithm) = registry::get_search_algorithm(algorithm_name) else {
        return Vec::new();
    };
    if docs.is_empty() {
        return Vec::new();
    }

    let weighted = apply_role_weight(algorithm.score(docs, query), docs, weights);
    let doc_by_ref: HashMap<&str, &SearchDoc> = docs
        .iter()
        .map(|doc| (doc.reference.as_str(), doc))
        .collect();

    let mut results: Vec<SearchResult> = weighted
        .into_iter()
        .filter_map(|entry| {
            let doc = doc_by_ref.get(entry.reference.as_str())?;
            Some(SearchResult {
                kind: doc.kind,
                reference: doc.reference.clone(),
                block_id: doc.block_id.clone(),
                tier: doc.tier.unwrap_or(1),
                score: entry.score,
                title: doc.title.clone(),
                preview: make_preview(&doc.text, query, preview_length),
                role: doc.role,
                tokens: doc.tokens,
            })
        })
        .filter(|result| result.score >= min_score)
        .collect();
    results.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(Ordering::Equal)
    });
    results.truncate(limit);
    results
}

/// 便捷入口：一次检索当前状态的块 + 观测到的消息。
pub fn search_state(
    state: &CompressionState,
    messages: &[CoreMessage],
    query: &str,
    options: &SearchOptions,
) -> Vec<SearchResult> {
    let mut docs = block_docs(state);
    docs.extend(message_docs(messages, state));
    search_blocks(&docs, query, options)
}

/// 以查询词首次命中处为中心的预览（对应上游 `makePreview`）。
///
/// 找不到命中时退回文本开头。上游用 UTF-16 下标同时索引 `lower` 与 `text`
/// （大小写折叠改变长度时会错位）；这里统一用**字符**下标，常规输入下与上游
/// 逐字一致。
fn make_preview(text: &str, query: &str, len: usize) -> String {
    if text.is_empty() {
        return String::new();
    }
    let terms: Vec<String> = query
        .to_lowercase()
        .split_whitespace()
        .filter(|term| term.chars().count() > 1)
        .map(str::to_string)
        .collect();
    if terms.is_empty() {
        return clamp_prefix(text, len);
    }

    let lower = text.to_lowercase();
    let mut hit_char_index: Option<usize> = None;
    for term in &terms {
        if let Some(byte_index) = lower.find(term.as_str()) {
            hit_char_index = Some(lower[..byte_index].chars().count());
            break;
        }
    }
    let Some(hit_index) = hit_char_index else {
        return clamp_prefix(text, len);
    };

    let total_chars = text.chars().count();
    let half = (len / 2).saturating_sub(10);
    let start = hit_index.saturating_sub(half);
    let end = total_chars.min(start + len);
    let prefix = if start > 0 { "…" } else { "" };
    let suffix = if end < total_chars { "…" } else { "" };
    format!("{prefix}{}{suffix}", clamp_window(text, start, end).trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::CompressionBlock;

    fn block(id: &str, topic: &str, summary: &str, active: bool, tier: u8) -> CompressionBlock {
        CompressionBlock {
            block_id: id.to_string(),
            topic: Some(topic.to_string()),
            summary: summary.to_string(),
            active,
            tier,
            compressed_tokens: 100,
            ..Default::default()
        }
    }

    fn state_with(blocks: Vec<CompressionBlock>) -> CompressionState {
        CompressionState {
            blocks,
            ..Default::default()
        }
    }

    fn doc(reference: &str, text: &str) -> SearchDoc {
        SearchDoc {
            kind: SearchDocKind::Block,
            reference: reference.to_string(),
            text: text.to_string(),
            title: reference.to_string(),
            role: None,
            block_id: Some(reference.to_string()),
            tier: Some(1),
            tokens: None,
        }
    }

    /// 上游的 "inactive fix"：失活块里的历史也必须能搜到。
    #[test]
    fn block_docs_should_include_inactive_blocks() {
        let state = state_with(vec![
            block("b1", "auth", "auth token design", false, 1),
            block("b2", "logs", "log rotation", true, 1),
        ]);
        let docs = block_docs(&state);
        assert_eq!(docs.len(), 2);
        let results = search_blocks(&docs, "auth", &SearchOptions::default());
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].reference, "b1");
        assert_eq!(results[0].kind, SearchDocKind::Block);
    }

    #[test]
    fn search_should_rank_the_best_block_first() {
        let state = state_with(vec![
            block("b1", "misc", "unrelated content about nothing", true, 1),
            block("b2", "auth token", "the auth token is minted here", true, 1),
        ]);
        let docs = block_docs(&state);
        let results = search_blocks(&docs, "auth token", &SearchOptions::default());
        assert_eq!(results[0].reference, "b2");
    }

    #[test]
    fn search_should_honour_limit_and_min_score() {
        let state = state_with(vec![
            block("b1", "auth", "auth auth auth", true, 1),
            block("b2", "auth", "auth", true, 1),
            block("b3", "auth", "auth", true, 1),
        ]);
        let docs = block_docs(&state);
        let options = SearchOptions {
            limit: Some(2),
            ..Default::default()
        };
        assert_eq!(search_blocks(&docs, "auth", &options).len(), 2);

        // hybrid 会把最强命中归一化到 1.0，因此阈值必须超过 1.0 才能全滤掉。
        let strict = SearchOptions {
            min_score: Some(1.5),
            ..Default::default()
        };
        assert!(search_blocks(&docs, "auth", &strict).is_empty());
    }

    #[test]
    fn search_should_return_empty_for_unknown_algorithm() {
        let docs = vec![doc("b1", "auth")];
        let options = SearchOptions {
            algorithm: Some("does-not-exist".to_string()),
            ..Default::default()
        };
        assert!(search_blocks(&docs, "auth", &options).is_empty());
    }

    #[test]
    fn role_weights_should_boost_user_messages_over_tool_outputs() {
        // 两篇文档文本相同，只有角色不同 ⇒ 权重决定排序。
        let docs = vec![
            SearchDoc {
                kind: SearchDocKind::Message,
                reference: "m00001".to_string(),
                text: "auth token".to_string(),
                title: "tool".to_string(),
                role: Some(MessageRole::Tool),
                block_id: None,
                tier: None,
                tokens: None,
            },
            SearchDoc {
                kind: SearchDocKind::Message,
                reference: "m00002".to_string(),
                text: "auth token".to_string(),
                title: "user".to_string(),
                role: Some(MessageRole::User),
                block_id: None,
                tier: None,
                tokens: None,
            },
        ];
        let results = search_blocks(&docs, "auth token", &SearchOptions::default());
        assert_eq!(results[0].reference, "m00002", "用户消息应排在工具结果之前");
        // 1.5 / 0.6 的比值。
        assert!((results[0].score / results[1].score - 2.5).abs() < 1e-9);
    }

    #[test]
    fn message_docs_should_map_to_the_owning_block() {
        let mut state = state_with(vec![CompressionBlock {
            block_id: "b1".to_string(),
            summary: "folded".to_string(),
            effective_message_ids: vec!["msg1".to_string()],
            active: true,
            tier: 2,
            ..Default::default()
        }]);
        state
            .message_refs
            .by_raw
            .insert("msg1".to_string(), "m00001".to_string());
        state
            .message_refs
            .by_ref
            .insert("m00001".to_string(), "msg1".to_string());

        let messages = vec![CoreMessage::text("msg1", Role::User, "how does auth work")];
        let docs = message_docs(&messages, &state);
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].reference, "m00001");
        assert_eq!(docs[0].role, Some(MessageRole::User));
        assert_eq!(docs[0].block_id.as_deref(), Some("b1"));
        assert_eq!(docs[0].tier, Some(2));
    }

    #[test]
    fn message_docs_should_skip_unreferenced_and_summary_messages() {
        let state = state_with(vec![]);
        let messages = vec![
            CoreMessage::text("no-ref", Role::User, "invisible"),
            CoreMessage::text("acp_summary_b1", Role::System, "summary"),
        ];
        assert!(message_docs(&messages, &state).is_empty());
    }

    #[test]
    fn preview_should_center_on_the_first_hit() {
        let text = format!("{}needle{}", "x".repeat(300), "y".repeat(300));
        let preview = make_preview(&text, "needle", 100);
        assert!(preview.contains("needle"), "{preview}");
        assert!(preview.starts_with('…'), "{preview}");
        assert!(preview.ends_with('…'), "{preview}");
    }

    #[test]
    fn preview_should_fall_back_to_the_head_without_a_hit() {
        let preview = make_preview("short text", "zzz", 100);
        assert_eq!(preview, "short text");
    }

    fn with_algorithm(algorithm: Option<&str>) -> SearchOptions {
        SearchOptions {
            algorithm: algorithm.map(str::to_string),
            ..Default::default()
        }
    }

    fn top_hit(docs: &[SearchDoc], query: &str, algorithm: Option<&str>) -> Option<String> {
        search_blocks(docs, query, &with_algorithm(algorithm))
            .first()
            .map(|result| result.reference.clone())
    }

    /// 换掉遗留子串路径的**直接证据**：形态变化与错拼是子串计数必然漏掉的，
    /// hybrid 的 BM25 词干 + fuzzy bigram 能捞回来；而精确词条两者都不许失手
    /// （不能为了召回牺牲精度）。
    #[test]
    fn hybrid_should_beat_the_legacy_substring_path_where_it_matters() {
        let docs = vec![
            doc("b1", "context compression keeps the window small"),
            doc("b2", "token bucket rate limiting for the api"),
            doc("b3", "log rotation and retention policy"),
            doc("b4", "身份验证流程与会话令牌"),
        ];

        // 形态：查询写 compressed，摘要里写的是 compression。
        assert_eq!(
            top_hit(&docs, "compressed", Some("substring")),
            None,
            "子串计数漏掉形态变化（这正是旧实现的行为）"
        );
        assert_eq!(top_hit(&docs, "compressed", None).as_deref(), Some("b1"));

        // 错拼：tokan ≈ token。
        assert_eq!(
            top_hit(&docs, "tokan", Some("substring")),
            None,
            "子串计数漏掉错拼"
        );
        assert_eq!(top_hit(&docs, "tokan", None).as_deref(), Some("b2"));

        // 精确词条：两者都必须命中，且都排第一。
        assert_eq!(
            top_hit(&docs, "rotation", Some("substring")).as_deref(),
            Some("b3")
        );
        assert_eq!(top_hit(&docs, "rotation", None).as_deref(), Some("b3"));

        // CJK：上游点名的「身份验证 命中 身份验证流程」。
        assert_eq!(top_hit(&docs, "身份验证", None).as_deref(), Some("b4"));
    }

    /// 上游在 32 块 / 48 查询上量化的差距（substring MRR 0.797 → hybrid 0.898）
    /// 在小语料上的复现：同一批查询，hybrid 的 MRR 必须严格高于遗留路径。
    #[test]
    fn hybrid_mrr_should_exceed_the_legacy_substring_path() {
        let docs = vec![
            doc("b1", "context compression keeps the window small"),
            doc("b2", "token bucket rate limiting for the api"),
            doc("b3", "log rotation and retention policy"),
            doc("b4", "身份验证流程与会话令牌"),
            doc("b5", "the auth middleware rejects expired credentials"),
            doc("b6", "database migration rollback procedure"),
        ];
        let cases = [
            ("compressed", "b1"),
            ("compression", "b1"),
            ("tokan", "b2"),
            ("rotation", "b3"),
            ("身份验证", "b4"),
            ("credentials", "b5"),
            ("middleware", "b5"),
            ("migrations", "b6"),
        ];
        let mrr = |algorithm: Option<&str>| -> f64 {
            let mut total = 0.0f64;
            for (query, expected) in cases {
                let hits = search_blocks(&docs, query, &with_algorithm(algorithm));
                if let Some(rank) = hits.iter().position(|hit| hit.reference == expected) {
                    total += 1.0 / (rank as f64 + 1.0);
                }
            }
            total / cases.len() as f64
        };
        let legacy = mrr(Some("substring"));
        let hybrid = mrr(None);
        println!(
            "search MRR on the small corpus: legacy substring {legacy:.3} vs hybrid {hybrid:.3}"
        );
        assert!(
            hybrid > legacy,
            "hybrid MRR {hybrid:.3} 必须高于遗留子串 {legacy:.3}"
        );
        assert!(hybrid >= 0.8, "hybrid MRR 过低：{hybrid:.3}");
    }
}
