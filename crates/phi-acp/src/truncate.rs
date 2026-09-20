//! 紧急截断 —— 对应 acp-kernel `src/truncate-tools.ts` / `src/truncate.ts`。
//!
//! 管线里最后一道 token 兜底：当使用率越过 `truncate.threshold` 时，把超大的
//! 工具结果（其次是大段文本）折叠为「前缀 + 标记 + 后缀」。

use crate::prune::{is_rendered_summary_message, SUMMARY_HEADER};
use crate::tokenize::count_tokens;
use crate::types::{Config, ContentType, CoreMessage, Role};

/// 截断标记。
const TRUNCATION_MARKER: &str = "[truncated for context space]";

/// 截断选项。
#[derive(Debug, Clone)]
pub struct TruncateOptions {
    /// 最小 token 数。
    pub min_output_tokens: u64,
    /// 保留前缀字符数。
    pub keep_prefix_chars: usize,
    /// 保留后缀字符数。
    pub keep_suffix_chars: usize,
    /// 保护最近 N 条消息。
    pub protect_recent_messages: usize,
    /// 是否把文本消息也作为最后手段候选。
    pub include_text_messages: bool,
}

impl Default for TruncateOptions {
    fn default() -> Self {
        Self {
            min_output_tokens: 1000,
            keep_prefix_chars: 2000,
            keep_suffix_chars: 2000,
            protect_recent_messages: 3,
            include_text_messages: false,
        }
    }
}

/// 截断结果。
#[derive(Debug, Clone, Default)]
pub struct TruncateResult {
    /// 处理后的消息。
    pub messages: Vec<CoreMessage>,
    /// 截断条数。
    pub truncated_count: usize,
    /// 节省的 token 数。
    pub saved_tokens: u64,
    /// 找到的候选数。
    pub candidates_found: usize,
}

/// 按字符边界安全截取前缀。
pub fn clamp_prefix(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

/// 按字符边界安全截取窗口 `[start, end)`。
pub fn clamp_window(text: &str, start: usize, end: usize) -> String {
    text.chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect()
}

/// 紧急截断超大工具输出。
pub fn truncate_large_tool_outputs(
    messages: &[CoreMessage],
    token_count: u64,
    config: &Config,
    options: &TruncateOptions,
) -> TruncateResult {
    let limit = config.model_context_limit;
    if limit == 0 || (token_count as f64) < config.truncate.threshold * limit as f64 {
        return TruncateResult {
            messages: messages.to_vec(),
            ..Default::default()
        };
    }

    let protected_index = messages
        .len()
        .saturating_sub(options.protect_recent_messages);

    let find_candidates = |predicate: &dyn Fn(&CoreMessage) -> bool| -> Vec<usize> {
        let mut found: Vec<usize> = Vec::new();
        for (i, message) in messages.iter().enumerate().take(protected_index) {
            if !predicate(message) {
                continue;
            }
            let text = message.text_str();
            if text.is_empty() || text.contains(TRUNCATION_MARKER) {
                continue;
            }
            if count_tokens(text) < options.min_output_tokens {
                continue;
            }
            found.push(i);
        }
        found.sort_by_key(|&i| std::cmp::Reverse(count_tokens(messages[i].text_str())));
        found
    };

    let tool_results = find_candidates(&|m| m.content_type == ContentType::ToolResult);
    let text_messages = if options.include_text_messages {
        find_candidates(&|m| {
            m.content_type == ContentType::Text
                && (m.role == Role::User || m.role == Role::Assistant)
                && !is_rendered_summary_message(m)
                && !m.text_str().starts_with(SUMMARY_HEADER)
        })
    } else {
        Vec::new()
    };
    let candidates_found = tool_results.len() + text_messages.len();

    let mut truncated_count = 0usize;
    let mut saved_tokens = 0u64;
    let mut remaining = token_count;
    let target_tokens = (config.truncate.threshold * limit as f64 * 0.9) as u64;
    let mut replacements: std::collections::BTreeMap<usize, String> =
        std::collections::BTreeMap::new();

    let mut apply_to = |candidates: &[usize],
                        truncated_count: &mut usize,
                        saved_tokens: &mut u64,
                        remaining: &mut u64| {
        for &index in candidates {
            if *remaining <= target_tokens {
                break;
            }
            let original = messages[index].text_str();
            let tokens = count_tokens(original);
            if original.chars().count() <= options.keep_prefix_chars + options.keep_suffix_chars {
                continue;
            }
            let prefix = clamp_prefix(original, options.keep_prefix_chars);
            let total = original.chars().count();
            let suffix = clamp_window(
                original,
                total.saturating_sub(options.keep_suffix_chars),
                total,
            );
            let replacement = format!(
                "{prefix}\n\n...{TRUNCATION_MARKER} — original ~{tokens} tokens]...\n\n{suffix}"
            );
            let saved = tokens.saturating_sub(count_tokens(&replacement));
            *remaining = remaining.saturating_sub(saved);
            *saved_tokens += saved;
            *truncated_count += 1;
            replacements.insert(index, replacement);
        }
    };

    apply_to(
        &tool_results,
        &mut truncated_count,
        &mut saved_tokens,
        &mut remaining,
    );
    apply_to(
        &text_messages,
        &mut truncated_count,
        &mut saved_tokens,
        &mut remaining,
    );

    if replacements.is_empty() {
        return TruncateResult {
            messages: messages.to_vec(),
            truncated_count: 0,
            saved_tokens: 0,
            candidates_found,
        };
    }

    let out = messages
        .iter()
        .enumerate()
        .map(|(i, m)| match replacements.get(&i) {
            Some(text) => CoreMessage {
                text: Some(text.clone()),
                ..m.clone()
            },
            None => m.clone(),
        })
        .collect();

    TruncateResult {
        messages: out,
        truncated_count,
        saved_tokens,
        candidates_found,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_should_noop_below_threshold() {
        let messages = vec![CoreMessage::text("a", Role::User, "hi")];
        let config = Config::default_for(100_000);
        let result =
            truncate_large_tool_outputs(&messages, 1000, &config, &TruncateOptions::default());
        assert_eq!(result.truncated_count, 0);
    }

    #[test]
    fn truncate_should_collapse_large_tool_output() {
        let big = "x".repeat(50_000);
        let messages = vec![
            CoreMessage {
                id: "t".into(),
                role: Role::Tool,
                content_type: ContentType::ToolResult,
                text: Some(big),
                ..Default::default()
            },
            CoreMessage::text("a", Role::User, "keep"),
            CoreMessage::text("b", Role::Assistant, "keep"),
            CoreMessage::text("c", Role::User, "keep"),
        ];
        let config = Config::default_for(100_000);
        let result = truncate_large_tool_outputs(
            &messages,
            99_000,
            &config,
            &TruncateOptions {
                protect_recent_messages: 3,
                ..Default::default()
            },
        );
        assert_eq!(result.truncated_count, 1);
        assert!(result.saved_tokens > 0);
        assert!(result.messages[0].text_str().contains(TRUNCATION_MARKER));
    }

    #[test]
    fn clamp_should_respect_char_boundaries() {
        assert_eq!(clamp_prefix("中文字符", 2), "中文");
        assert_eq!(clamp_window("abcdef", 2, 4), "cd");
    }
}
