//! 检索分词 —— 对应 acp-kernel `src/search/tokenizer.ts`。
//!
//! 处理拉丁 + CJK 混排，这是相对朴素子串匹配**最大的一项质量杠杆**：
//! - 拉丁按非词边界切分（`[a-z][a-z0-9_]*[a-z0-9]` 或单字符 `[a-z0-9]`）；
//! - CJK（无空格）用**重叠 bigram** 近似「词」，于是 `身份验证` 仍能命中
//!   `身份验证流程`。
//!
//! # 与上游的一处有意偏差：CJK 没有词典分词
//!
//! 上游用 `Intl.Segmenter("zh", { granularity: "word" })`（ICU CLDR 词典）切
//! CJK 词：词典命中时多字词保持原子（`身份验证流程` → `身份/验证/流程`），
//! 只有整段全 OOV 才回退到重叠 bigram + 单字。Rust 没有内建等价物，而
//! acp-kernel 明确以「零运行时依赖」为设计前提（见 `search/SEARCH.md`），
//! 因此这里**始终走 bigram 近似**：多字段出重叠 bigram，单字文本出该单字。
//!
//! 后果（诚实记录）：
//! - 召回不减：上游点名的「`身份验证` 命中 `身份验证流程`」成立。
//! - 精度略降：上游靠词典避免的「`试验证明` 误命中 `验证`」**会发生**（bigram
//!   `验证` 确实是 `试验证明` 的第 2–3 字）。没有词典就无法避免；BM25 的 IDF
//!   与长度归一化会压低这类普遍词条的影响。
//! - 单字查询只命中单字文本：上游词典命中时同样不会为多字段产出单字词条，
//!   行为一致。
//!
//! CJK 的判定与 fuzzy 通道共用同一份 [`is_cjk`]，避免两处各自漂移。

use super::stemmer::stem;

/// CJK 表意文字 / 假名 / 谚文——「必须特殊处理的非拉丁文字」的唯一判定。
///
/// 刻意不含拉丁：2 字符英文词（`to`/`of`）没有意义，而几乎所有 CJK 词都是
/// 2 字原子单位（`登录`/`缓存`），两种文字需要相反的规则。
pub fn is_cjk(ch: char) -> bool {
    matches!(
        ch as u32,
        0x3400..=0x9FFF | 0xF900..=0xFAFF | 0x3040..=0x30FF | 0xAC00..=0xD7AF
    )
}

/// 文本里是否含 CJK。
pub fn contains_cjk(text: &str) -> bool {
    text.chars().any(is_cjk)
}

/// 按上游的 `LATIN_WORD = /[a-z][a-z0-9_]*[a-z0-9]|[a-z0-9]/g` 切拉丁词。
///
/// 输入必须是已小写的文本。两个分支的优先级与贪婪回溯语义与 JS 正则一致：
/// 先试「首字符是字母、末字符是字母或数字」的长形式，失败再退到单字符。
fn latin_words(lower: &str) -> Vec<&str> {
    let bytes = lower.as_bytes();
    let mut words = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let start = i;
        if bytes[i].is_ascii_lowercase() {
            // 长形式：吃掉 [a-z0-9_]，记住最后一个 [a-z0-9] 的位置作为词尾。
            let mut j = i + 1;
            let mut last_alnum: Option<usize> = None;
            while j < bytes.len()
                && (bytes[j].is_ascii_lowercase() || bytes[j].is_ascii_digit() || bytes[j] == b'_')
            {
                if bytes[j] != b'_' {
                    last_alnum = Some(j);
                }
                j += 1;
            }
            if let Some(end) = last_alnum {
                words.push(&lower[start..=end]);
                i = end + 1;
                continue;
            }
            // 长形式不成立（如 "a_"）：退到单字符。
            words.push(&lower[start..start + 1]);
            i = start + 1;
            continue;
        }
        if bytes[i].is_ascii_digit() {
            words.push(&lower[start..start + 1]);
            i = start + 1;
            continue;
        }
        i += 1;
    }
    words
}

/// 一段连续 CJK 文本 → token。
///
/// 上游先用 ICU CLDR 词典切词，取长度 ≥ 2 的词；整段全 OOV 时才回退到重叠
/// bigram + 单字。这里没有词典（见模块头注释），因此始终走 bigram 近似：
/// - 单字文本 → 该单字（否则单字查询永远无法命中任何东西）；
/// - 多字文本 → 重叠 bigram（`身份验证流程` → `身份/份验/验证/证流/流程`）。
///
/// 刻意**不为多字文本产出单字词条**：上游词典命中时同样不会，否则每个 CJK 字
/// 都变成一个词条，噪声会淹没 BM25。
fn cjk_run_tokens(run: &str) -> Vec<String> {
    let chars: Vec<char> = run.chars().collect();
    if chars.len() < 2 {
        return chars.into_iter().map(String::from).collect();
    }
    chars
        .windows(2)
        .map(|window| window.iter().collect())
        .collect()
}

/// 把文本切成检索 token。
///
/// 拉丁词长度 ≥ 2 才收（2–3 字符英文词是停用词噪声）；`stem` 为真时做词干
/// 还原（BM25 通道用）。CJK 段一律走 [`cjk_run_tokens`]。
pub fn tokenize(text: &str, stem_enabled: bool) -> Vec<String> {
    let lower = text.to_lowercase();
    let mut tokens: Vec<String> = Vec::new();

    for word in latin_words(&lower) {
        if word.len() >= 2 {
            tokens.push(if stem_enabled {
                stem(word)
            } else {
                word.to_string()
            });
        }
    }

    // 纯拉丁文本直接返回：上游刻意跳过分词器（避免为纯英文付一次全量 CJK 扫描）。
    if !contains_cjk(&lower) {
        return tokens;
    }

    // 把连续 CJK 字符聚成段（非 CJK 字符是段边界）。
    let mut run = String::new();
    for ch in lower.chars() {
        if is_cjk(ch) {
            run.push(ch);
        } else if !run.is_empty() {
            tokens.extend(cjk_run_tokens(&run));
            run.clear();
        }
    }
    if !run.is_empty() {
        tokens.extend(cjk_run_tokens(&run));
    }

    tokens
}

/// 任意文本的**字符** bigram——fuzzy 通道用。
///
/// 对应上游 `charBigrams`：逐字符滑窗取相邻两字符，跳过「首字符或末字符是
/// 空白」的 pair（上游的 `pair.trim().length === pair.length` 判定）。因此
/// `auth token` 只会产出 `au/ut/th/to/ok/ke/en`，跨空格的两对不算。
pub fn char_bigrams(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut grams = Vec::new();
    for window in chars.windows(2) {
        let first = window[0];
        let second = window[1];
        if !first.is_whitespace() && !second.is_whitespace() {
            grams.push(window.iter().collect());
        }
    }
    grams
}

/// 词频表。
pub fn tf_map(text: &str, stem_enabled: bool) -> std::collections::HashMap<String, u32> {
    let mut map = std::collections::HashMap::new();
    for token in tokenize(text, stem_enabled) {
        *map.entry(token).or_insert(0) += 1;
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latin_words_should_follow_the_upstream_regex() {
        assert_eq!(latin_words("auth token"), vec!["auth", "token"]);
        // 单字符也收（`[a-z0-9]` 分支）。
        assert_eq!(latin_words("a b"), vec!["a", "b"]);
        assert_eq!(latin_words("a1"), vec!["a1"]);
        // 下划线可出现在词中，但不能作词尾。
        assert_eq!(latin_words("foo_bar"), vec!["foo_bar"]);
        assert_eq!(latin_words("foo_ bar"), vec!["foo", "bar"]);
        assert_eq!(latin_words("a_"), vec!["a"]);
        // 非字母数字全部跳过。
        assert_eq!(latin_words("--x--"), vec!["x"]);
    }

    #[test]
    fn tokenize_should_drop_single_char_latin_but_keep_cjk_singles() {
        let tokens = tokenize("a token", false);
        assert!(!tokens.contains(&"a".to_string()), "{tokens:?}");
        assert!(tokens.contains(&"token".to_string()), "{tokens:?}");
    }

    #[test]
    fn tokenize_should_keep_cjk_bigrams() {
        let tokens = tokenize("身份验证流程", false);
        // 重叠 bigram：身份 / 份验 / 验证 / 证流 / 流程
        assert!(tokens.contains(&"身份".to_string()), "{tokens:?}");
        assert!(tokens.contains(&"验证".to_string()), "{tokens:?}");
        assert!(tokens.contains(&"流程".to_string()), "{tokens:?}");
        // 多字文本不产出单字词条（上游词典命中时同样如此）。
        assert!(!tokens.contains(&"验".to_string()), "{tokens:?}");
    }

    #[test]
    fn tokenize_should_keep_single_char_cjk_text() {
        // 单字文本必须留下该单字，否则单字查询永远无解。
        assert_eq!(tokenize("验", false), vec!["验".to_string()]);
    }

    /// 上游点名的召回行为：`身份验证` 必须能命中 `身份验证流程`。
    #[test]
    fn tokenize_should_hit_cjk_substring_via_bigrams() {
        let doc = tokenize("身份验证流程", false);
        let query = tokenize("身份验证", false);
        assert!(query.iter().any(|t| doc.contains(t)), "{doc:?} / {query:?}");
    }

    /// **已知精度缺口（诚实记录）**：上游靠 ICU 词典把 `试验证明` 切成
    /// `试验/证明`，因此查询 `验证` 不命中；没有词典时 bigram `验证` 恰好是
    /// `试验证明` 的第 2–3 字，于是会误命中。这条测试把该偏差钉住，避免日后
    /// 被当成「已修复」。
    #[test]
    fn tokenize_known_gap_cjk_false_hit_without_a_dictionary() {
        let doc = tokenize("试验证明", false);
        let query = tokenize("验证", false);
        assert!(
            query.iter().any(|t| doc.contains(t)),
            "无词典时 bigram 会误命中；若这里变成 false，说明换成了真分词器：{doc:?} / {query:?}"
        );
    }

    #[test]
    fn tokenize_should_segment_mixed_scripts() {
        let tokens = tokenize("compress 压缩 缓存", false);
        assert!(tokens.contains(&"compress".to_string()), "{tokens:?}");
        assert!(tokens.contains(&"压缩".to_string()), "{tokens:?}");
        assert!(tokens.contains(&"缓存".to_string()), "{tokens:?}");
    }

    #[test]
    fn tokenize_should_apply_stemming_only_when_asked() {
        assert!(tokenize("compressed", false).contains(&"compressed".to_string()));
        assert!(tokenize("compressed", true).contains(&"compress".to_string()));
    }

    #[test]
    fn char_bigrams_should_skip_whitespace_edges() {
        assert_eq!(
            char_bigrams("auth token"),
            vec!["au", "ut", "th", "to", "ok", "ke", "en"]
        );
        assert!(char_bigrams(" ").is_empty());
        assert!(char_bigrams("a").is_empty());
    }

    #[test]
    fn tf_map_should_count_term_frequencies() {
        let tf = tf_map("token tokens", true);
        assert_eq!(tf.get("token"), Some(&2));
    }
}
