//! BM25（词干 + CJK 分词）—— 对应 `algorithms/bm25.ts`。
//!
//! k1=1.2、b=0.75（IR 标准值）。IDF 压低语料里普遍出现的词；长度归一化避免长
//! 摘要靠词频堆量取胜。词干还原把英文形态收敛到同一词根
//! （`compress`/`compressed`/`compression` → `~compress`）。
//!
//! 上游在 32 块 EN/CJK 基准上：MRR 0.833 / R@1 0.833 / R@3 0.833，对照
//! substring 的 0.797 / 0.792 / 0.792——各项都更好，也是 hybrid 默认里的精度
//! 分量。

use std::collections::{HashMap, HashSet};

use super::super::doc_cache::doc_features;
use super::super::tokenizer::tokenize;
use super::super::types::{ScoredDoc, SearchAlgorithm, SearchDoc};

/// BM25 词频饱和参数。
const K1: f64 = 1.2;
/// BM25 长度归一化参数。
const B: f64 = 0.75;

/// BM25 算法。
pub struct Bm25;

impl SearchAlgorithm for Bm25 {
    fn name(&self) -> &'static str {
        "bm25"
    }

    fn description(&self) -> &'static str {
        "BM25 with stemming + CJK bigram tokenization. IR-standard relevance ranking."
    }

    fn score(&self, docs: &[SearchDoc], query: &str) -> Vec<ScoredDoc> {
        let zeros = |docs: &[SearchDoc]| -> Vec<ScoredDoc> {
            docs.iter()
                .map(|doc| ScoredDoc {
                    reference: doc.reference.clone(),
                    score: 0.0,
                })
                .collect()
        };

        let n = docs.len();
        let features: Vec<_> = docs.iter().map(|doc| doc_features(&doc.text)).collect();
        let avgdl = if n == 0 {
            0.0
        } else {
            features.iter().map(|f| f.len as f64).sum::<f64>() / n as f64
        };
        // 上游 `avgdl || 1`：0 时退化为 1。
        let avgdl = if avgdl == 0.0 { 1.0 } else { avgdl };

        let query_terms = tokenize(query, true);
        if query_terms.is_empty() {
            return zeros(docs);
        }

        // IDF 只对去重后的查询词算一次。
        let mut idf: HashMap<&str, f64> = HashMap::new();
        let unique: HashSet<&str> = query_terms.iter().map(String::as_str).collect();
        for term in unique {
            let df = features.iter().filter(|f| f.tf.contains_key(term)).count();
            let value = (1.0 + (n as f64 - df as f64 + 0.5) / (df as f64 + 0.5)).ln();
            idf.insert(term, value);
        }

        docs.iter()
            .zip(features.iter())
            .map(|(doc, f)| {
                let mut score = 0.0f64;
                // 注意：查询词**不去重**（上游同样如此），重复词按出现次数加权。
                for term in &query_terms {
                    let tf = f.tf.get(term.as_str()).copied().unwrap_or(0) as f64;
                    if tf == 0.0 {
                        continue;
                    }
                    let idf_term = idf.get(term.as_str()).copied().unwrap_or(0.0);
                    score += idf_term * (tf * (K1 + 1.0))
                        / (tf + K1 * (1.0 - B + B * f.len as f64 / avgdl));
                }
                ScoredDoc {
                    reference: doc.reference.clone(),
                    score,
                }
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
    fn bm25_should_rank_the_rare_term_higher() {
        // "cache" 只在 b2 出现 ⇒ IDF 高；"the" 到处都是 ⇒ IDF 低。
        let docs = vec![
            doc("b1", "the the the the auth"),
            doc("b2", "the cache invalidation"),
        ];
        let scored = Bm25.score(&docs, "cache");
        let b2 = scored.iter().find(|s| s.reference == "b2").unwrap();
        let b1 = scored.iter().find(|s| s.reference == "b1").unwrap();
        assert!(b2.score > 0.0);
        assert_eq!(b1.score, 0.0);
    }

    #[test]
    fn bm25_should_match_through_stemming() {
        let docs = vec![doc("b1", "compression of context"), doc("b2", "unrelated")];
        // 查询 "compressed" 必须命中含 "compression" 的文档。
        let scored = Bm25.score(&docs, "compressed");
        assert!(scored[0].score > 0.0, "{scored:?}");
    }

    #[test]
    fn bm25_should_match_cjk_bigrams() {
        let docs = vec![doc("b1", "身份验证流程"), doc("b2", "日志与配置")];
        let scored = Bm25.score(&docs, "身份验证");
        assert!(scored[0].score > 0.0, "{scored:?}");
        assert_eq!(scored[1].score, 0.0);
    }

    #[test]
    fn bm25_should_return_zero_for_blank_query() {
        let docs = vec![doc("b1", "anything")];
        let scored = Bm25.score(&docs, "  ");
        assert_eq!(scored[0].score, 0.0);
    }
}
