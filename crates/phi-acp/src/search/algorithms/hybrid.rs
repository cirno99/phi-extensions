//! 混合检索：归一化 BM25(词干) + fuzzy n-gram，权重 0.7 / 0.3 —— 对应
//! `algorithms/hybrid.ts`，也是**默认算法**。
//!
//! BM25 提供真词条上的精度（形态 + IDF + 长度归一化），fuzzy 提供错拼、部分词
//! 与跨文字上的召回。两个分量先各自按最大值归一化到 [0,1] 再加权，使其量级与
//! 语料规模无关。
//!
//! 上游基准（32 块 / 48 条 EN+CJK 查询）：
//! ```text
//! substring  MRR 0.797  R@1 0.792  R@3 0.792
//! bm25       MRR 0.833  R@1 0.833  R@3 0.833
//! fuzzy      MRR 0.795  R@1 0.708  R@3 0.875
//! hybrid     MRR 0.898  R@1 0.875  R@3 0.917   ← 各项最优
//! ```
//! 权重比对 0.6–0.8 的 BM25 都在 0.001 MRR 内，不敏感。

use std::collections::HashMap;

use super::super::types::{ScoredDoc, SearchAlgorithm, SearchDoc};
use super::bm25::Bm25;
use super::fuzzy::Fuzzy;

/// BM25 分量权重。
const W_BM25: f64 = 0.7;
/// fuzzy 分量权重。
const W_FUZZY: f64 = 0.3;

/// 混合算法。
pub struct Hybrid;

impl SearchAlgorithm for Hybrid {
    fn name(&self) -> &'static str {
        "hybrid"
    }

    fn description(&self) -> &'static str {
        "Weighted BM25(stem) + fuzzy n-gram. Default — best precision + recall."
    }

    fn score(&self, docs: &[SearchDoc], query: &str) -> Vec<ScoredDoc> {
        let bm25 = Bm25.score(docs, query);
        let fuzzy = Fuzzy.score(docs, query);

        // 上游 `Math.max(...scores, 1e-9)`：空数组或全 0 时取 1e-9，避免除零。
        let max_bm25 = bm25.iter().map(|r| r.score).fold(1e-9f64, f64::max);
        let max_fuzzy = fuzzy.iter().map(|r| r.score).fold(1e-9f64, f64::max);
        let bm25_map: HashMap<&str, f64> = bm25
            .iter()
            .map(|r| (r.reference.as_str(), r.score / max_bm25))
            .collect();
        let fuzzy_map: HashMap<&str, f64> = fuzzy
            .iter()
            .map(|r| (r.reference.as_str(), r.score / max_fuzzy))
            .collect();

        docs.iter()
            .map(|doc| ScoredDoc {
                reference: doc.reference.clone(),
                score: W_BM25 * bm25_map.get(doc.reference.as_str()).copied().unwrap_or(0.0)
                    + W_FUZZY
                        * fuzzy_map
                            .get(doc.reference.as_str())
                            .copied()
                            .unwrap_or(0.0),
            })
            .collect()
    }
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
    fn hybrid_should_score_within_unit_range() {
        let docs = vec![doc("b1", "auth token rotation"), doc("b2", "logging")];
        let scored = Hybrid.score(&docs, "auth");
        assert!(scored[0].score > 0.0);
        assert!(scored[0].score <= 1.0 + f64::EPSILON, "{scored:?}");
    }

    /// 精确词条应排在错拼命中之前（BM25 精度分量主导）。
    #[test]
    fn hybrid_should_prefer_the_exact_term_over_the_fuzzy_hit() {
        let docs = vec![
            doc("b1", "the token bucket algorithm"),
            doc("b2", "a taken shortcut"),
        ];
        let scored = Hybrid.score(&docs, "token");
        assert_eq!(scored[0].reference, "b1", "{scored:?}");
        assert!(scored[0].score > scored[1].score, "{scored:?}");
    }

    /// 完全无重合时不得因为归一化而给出非零分。
    #[test]
    fn hybrid_should_return_zero_when_nothing_matches() {
        let docs = vec![doc("b1", "aaaaaaaa"), doc("b2", "bbbbbbbb")];
        let scored = Hybrid.score(&docs, "zzzzzzzz");
        assert!(scored.iter().all(|s| s.score == 0.0), "{scored:?}");
    }

    #[test]
    fn hybrid_should_keep_fuzzy_recall_for_cjk() {
        let docs = vec![doc("b1", "缓存失效"), doc("b2", "日志配置")];
        let scored = Hybrid.score(&docs, "缓存");
        assert!(scored[0].score > scored[1].score, "{scored:?}");
    }
}
