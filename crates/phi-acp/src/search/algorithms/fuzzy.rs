//! 模糊字符 bigram 匹配（Jaccard 风格）—— 对应 `algorithms/fuzzy.ts`。
//!
//! 把查询拆成字符 bigram，与每篇文档求重合度。对错拼（`tokan`≈`token`）、
//! 部分词都鲁棒，且跨文字统一（CJK 受益最大）。
//!
//! 查询词闸门——CJK 有自己的一条长度规则，拉丁冻结：
//! - 长度 ≥ 4（任何文字）：错拼容错需要几个字符才有意义；2–3 字符的拉丁词
//!   （`to`/`of`/`us`）是停用词噪声，它们的 bigram 几乎与每篇文档都重合。
//! - 长度 ≥ 2 且是 CJK：中日韩词大多是 2 字符原子单位（`登录`/`缓存`/`図表`），
//!   照搬拉丁的 ≥4 规则会把整个 CJK 查询空间挡在这个召回通道之外。单字 CJK
//!   仍然排除——一个字符构不成 bigram，没有可比较的东西。

use std::collections::HashSet;

use super::super::doc_cache::doc_features;
use super::super::tokenizer::{char_bigrams, contains_cjk};
use super::super::types::{ScoredDoc, SearchAlgorithm, SearchDoc};

/// 模糊 bigram 算法。
pub struct Fuzzy;

impl SearchAlgorithm for Fuzzy {
    fn name(&self) -> &'static str {
        "fuzzy"
    }

    fn description(&self) -> &'static str {
        "Character bigram overlap. Typo-tolerant, script-agnostic, high recall."
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

        // 闸门：拉丁短词是噪声，2 字符 CJK 词是真词条，要放进来。
        let query_tokens: Vec<String> = query
            .to_lowercase()
            .split(|c: char| c.is_whitespace() || c == ',')
            .filter(|token| {
                let len = token.chars().count();
                len >= 4 || (len >= 2 && contains_cjk(token))
            })
            .map(str::to_string)
            .collect();
        if query_tokens.is_empty() {
            return zeros(docs);
        }

        let mut query_grams: HashSet<String> = HashSet::new();
        for token in &query_tokens {
            query_grams.extend(char_bigrams(token));
        }
        if query_grams.is_empty() {
            return zeros(docs);
        }

        let total = query_grams.len() as f64;
        docs.iter()
            .map(|doc| {
                let features = doc_features(&doc.text);
                let hits = query_grams
                    .iter()
                    .filter(|gram| features.grams.contains(*gram))
                    .count();
                ScoredDoc {
                    reference: doc.reference.clone(),
                    score: hits as f64 / total,
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
    fn fuzzy_should_tolerate_typos() {
        let docs = vec![doc("b1", "token bucket rate limiting")];
        let scored = Fuzzy.score(&docs, "tokan");
        assert!(scored[0].score > 0.0, "{scored:?}");
    }

    #[test]
    fn fuzzy_should_admit_two_char_cjk_queries() {
        let docs = vec![doc("b1", "缓存失效策略"), doc("b2", "unrelated latin")];
        let scored = Fuzzy.score(&docs, "缓存");
        assert!(scored[0].score > 0.0, "{scored:?}");
        assert_eq!(scored[1].score, 0.0);
    }

    #[test]
    fn fuzzy_should_reject_short_latin_and_single_cjk() {
        let docs = vec![doc("b1", "to of us")];
        // 2–3 字符拉丁词全部被闸门挡住 ⇒ 无查询 bigram ⇒ 全 0。
        assert_eq!(Fuzzy.score(&docs, "to")[0].score, 0.0);
        // 单个 CJK 字构不成 bigram。
        assert_eq!(Fuzzy.score(&docs, "验")[0].score, 0.0);
    }

    #[test]
    fn fuzzy_should_split_on_whitespace_and_commas() {
        let docs = vec![doc("b1", "authentication handling")];
        let scored = Fuzzy.score(&docs, "authentication,handling");
        assert!(scored[0].score > 0.0, "{scored:?}");
    }
}
