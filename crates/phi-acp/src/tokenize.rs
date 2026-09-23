//! token 估算 —— 对应 acp-kernel `src/tokenize.ts`。
//!
//! 口径与 TS 版一致：CJK 字符按 ~1 token/字计，其余按 4 字符/token 估算。
//! 这样中日韩代码/日志不会被严重低估（`chars/4` 对 CJK 会低估约 4 倍）。
//!
//! 性能：这是每 turn 都会跑的热路径（推荐、状态、截断都调用）。纯 ASCII 输入
//! 走 `str::is_ascii` 的向量化快路径（一次扫描 + 一次除法），只有出现高位字节时
//! 才逐字符判定 CJK 区间。

use std::collections::HashMap;

use crate::types::CoreMessage;

/// 判断一个 Unicode 标量是否落在「CJK 密集」区间（中日韩文字 + 假名 + 谚文）。
///
/// 与 TS 版的 `[\u4e00-\u9fff\u3040-\u30ff\uac00-\ud7af]` 完全一致。
#[inline]
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x4E00..=0x9FFF | 0x3040..=0x30FF | 0xAC00..=0xD7AF)
}

/// 默认 token 估算。
///
/// 空串返回 0。CJK 字符每个计 1，其余字符按 4 字符/token 向上取整。
pub fn count_tokens(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    let bytes = text.as_bytes();
    // 纯 ASCII 快路径：`str::is_ascii` 在标准库里已是按机器字 / SIMD 扫描，
    // 没有高位字节就一定是非 CJK，直接按字节数估。
    if text.is_ascii() {
        return bytes.len().div_ceil(4) as u64;
    }
    // 单次遍历同时统计字符总数与 CJK 数（此前 `chars()` 扫了两遍）。
    let (chars, cjk) = text.chars().fold((0u64, 0u64), |(chars, cjk), c| {
        (chars + 1, cjk + u64::from(is_cjk(c)))
    });
    cjk + (chars - cjk).div_ceil(4)
}

/// 快速估算（纯 `chars/4`），用于无需 CJK 精度的场景。
pub fn estimate_tokens_fast(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    (text.chars().count() as u64).div_ceil(4)
}

/// 宿主投影的推理 token 计数：非正 / 缺失一律视为 0。
#[inline]
pub fn thinking_token_value(thinking: Option<u32>) -> u64 {
    thinking.map(u64::from).unwrap_or(0)
}

/// 单条消息的计量大小：可见文本 + 推理 token。
pub fn count_message_tokens(message: &CoreMessage) -> u64 {
    count_tokens(message.text_str()) + thinking_token_value(message.thinking_tokens)
}

/// 以给定分词器计量单条消息。
pub fn count_message_tokens_with(message: &CoreMessage, count: &dyn Fn(&str) -> u64) -> u64 {
    count(message.text_str()) + thinking_token_value(message.thinking_tokens)
}

/// 一批消息的 token 索引：同一 turn 内 `compute_protected_refs` /
/// `build_compressible_ranges` / 细分统计都要遍历同一批消息并逐条计数。
/// 把它们各自的全量扫描收敛成**一次**，避免每 turn 对同样的文本重复计数。
///
/// 键是消息 id（视图内唯一）；缺失时回退实时计算（防御，正常不会发生）。
pub struct MessageTokenIndex<'a> {
    by_id: HashMap<&'a str, u64>,
}

impl<'a> MessageTokenIndex<'a> {
    /// 遍历一次 `messages`，算好每条消息的 token 数。
    pub fn build(messages: &'a [CoreMessage]) -> Self {
        let mut by_id = HashMap::with_capacity(messages.len());
        for message in messages {
            by_id
                .entry(message.id.as_str())
                .or_insert_with(|| count_message_tokens(message));
        }
        Self { by_id }
    }

    /// 取某条消息的 token 数。
    #[inline]
    pub fn tokens(&self, message: &CoreMessage) -> u64 {
        self.by_id
            .get(message.id.as_str())
            .copied()
            .unwrap_or_else(|| count_message_tokens(message))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_tokens_should_return_zero_for_empty() {
        assert_eq!(count_tokens(""), 0);
    }

    #[test]
    fn count_tokens_should_estimate_ascii_by_quarter() {
        assert_eq!(count_tokens("abcd"), 1);
        assert_eq!(count_tokens("abcde"), 2);
        assert_eq!(count_tokens("a"), 1);
    }

    #[test]
    fn count_tokens_should_count_cjk_per_char() {
        // 4 个汉字 = 4 token（而非 1）。
        assert_eq!(count_tokens("中文字符"), 4);
    }

    #[test]
    fn count_tokens_should_mix_cjk_and_ascii() {
        // 2 汉字 + 8 ASCII = 2 + 2 = 4。
        assert_eq!(count_tokens("中文abcdefgh"), 4);
    }

    #[test]
    fn count_tokens_should_treat_kana_and_hangul_as_cjk() {
        assert_eq!(count_tokens("あいう"), 3);
        assert_eq!(count_tokens("한글"), 2);
    }

    #[test]
    fn count_message_tokens_should_add_thinking() {
        let mut m = CoreMessage::text("x", crate::types::Role::Assistant, "abcd");
        m.thinking_tokens = Some(7);
        assert_eq!(count_message_tokens(&m), 8);
    }

    #[test]
    fn thinking_token_value_should_guard_absent() {
        assert_eq!(thinking_token_value(None), 0);
        assert_eq!(thinking_token_value(Some(0)), 0);
        assert_eq!(thinking_token_value(Some(3)), 3);
    }

    #[test]
    fn estimate_tokens_fast_should_ignore_cjk_weighting() {
        assert_eq!(estimate_tokens_fast("中文字符"), 1);
    }
}
