//! 子串计数——最初的基线算法（对应 `algorithms/substring.ts`）。
//!
//! 精确、小写化的子串出现次数。可预测但不懂形态、错拼与 CJK 词边界。
//! 保留用于向后兼容与作为确定性参照。

use super::super::doc_cache::doc_features;
use super::super::types::{ScoredDoc, SearchAlgorithm, SearchDoc};

/// 子串计数算法。
pub struct Substring;

impl SearchAlgorithm for Substring {
    fn name(&self) -> &'static str {
        "substring"
    }

    fn description(&self) -> &'static str {
        "Exact substring counting (original baseline). Predictable, no normalization."
    }

    fn score(&self, docs: &[SearchDoc], query: &str) -> Vec<ScoredDoc> {
        let terms: Vec<String> = query
            .to_lowercase()
            .split_whitespace()
            .map(str::to_string)
            .collect();
        if terms.is_empty() {
            return docs
                .iter()
                .map(|doc| ScoredDoc {
                    reference: doc.reference.clone(),
                    score: 0.0,
                })
                .collect();
        }
        docs.iter()
            .map(|doc| {
                let features = doc_features(&doc.text);
                let score: f64 = terms
                    .iter()
                    .map(|term| count_occurrences(&features.lower, term) as f64)
                    .sum();
                ScoredDoc {
                    reference: doc.reference.clone(),
                    score,
                }
            })
            .collect()
    }
}

/// 非重叠子串出现次数（对应上游 `countOccurrences`）。
pub fn count_occurrences(haystack: &str, needle: &str) -> usize {
    if haystack.is_empty() || needle.is_empty() {
        return 0;
    }
    // memchr::memmem 用 SIMD 子串搜索，比逐字符扫描更适合长摘要。
    let haystack_bytes = haystack.as_bytes();
    let needle_bytes = needle.as_bytes();
    let mut count = 0usize;
    let mut start = 0usize;
    while start + needle_bytes.len() <= haystack_bytes.len() {
        match memchr::memmem::find(&haystack_bytes[start..], needle_bytes) {
            Some(position) => {
                count += 1;
                start += position + needle_bytes.len();
            }
            None => break,
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::types::SearchDocKind;

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

    #[test]
    fn count_occurrences_should_be_non_overlapping() {
        assert_eq!(count_occurrences("aaa", "aa"), 1);
        assert_eq!(count_occurrences("aaaa", "aa"), 2);
        assert_eq!(count_occurrences("abc", ""), 0);
    }

    #[test]
    fn substring_should_rank_by_occurrence_count() {
        let docs = vec![doc("b1", "auth auth auth"), doc("b2", "auth once")];
        let scored = Substring.score(&docs, "auth");
        assert_eq!(scored[0].reference, "b1");
        assert_eq!(scored[0].score, 3.0);
        assert_eq!(scored[1].score, 1.0);
    }

    #[test]
    fn substring_should_return_zero_for_blank_query() {
        let docs = vec![doc("b1", "anything")];
        let scored = Substring.score(&docs, "   ");
        assert_eq!(scored.len(), 1);
        assert_eq!(scored[0].score, 0.0);
    }
}
