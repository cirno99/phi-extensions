//! 工具输出即时吸收（absorb）—— 对应 billion-context 的 absorb 通道。
//!
//! # 为什么需要它
//!
//! phi 扩展**没有**请求体钩子、也拿不到宿主消息历史（见 [`crate::lib`] 文档），
//! 唯一能把内容从上游请求里真正去掉的通道就是 `tool_result` 拦截：返回
//! `phi::ToolResultResult { content: Some(..) }` 会替换模型看到的那条消息。
//!
//! ACP 的 `compress` 块只写进本扩展自己的 `state.json`，压不掉宿主历史里的
//! `tool_result`。因此**「把巨型工具输出压小」是本扩展在 phi 上唯一能兑现的
//! token 收益**，也是把 `runtime.messages` 视图与上游真实内容对齐的手段
//! （否则 `acp_status` 报告的上下文会系统性高估）。
//!
//! # 策略（保守优先）
//!
//! 只吸收「足够大」的结果：保留头部 + 尾部 + 一行吸收标记，中间替换。
//! 判据全部可配（`minToolTokens` / `keepPrefixChars` / `keepSuffixChars`），
//! 且以下情形一律不动：
//!
//! - 出错的结果（`is_error`）—— 错误文本需要完整。
//! - ACP 自身工具的 I/O（`compress` 账本、`acp_decompress` 恢复出的原文）。
//! - 上下文使用率低于 `contextThresholdPct` 时（默认 0 = 不设门槛）。
//! - 压完反而更长（小输出走了 keep 窗口）。
//!
//! 与 compress 块不同，吸收**不保留原文**：标记里说明原文已丢弃，模型若需要
//! 细节应重新执行工具。这正是 `TIER` 提示词里「按需重跑」语义的落地。

use crate::tokenize::count_tokens;
use crate::types::AbsorbConfig;

/// 被吸收消息里的标记（也是幂等判据：已含标记的结果不再处理）。
pub const ABSORB_MARKER: &str = "[acp absorb]";

/// 永不吸收的工具名（本扩展自身的载荷）。
pub const NEVER_ABSORB_TOOLS: &[&str] = &[
    "compress",
    "acp_decompress",
    "acp_search",
    "acp_status",
    "acp_rule",
    "acp_cache",
];

/// 一次吸收的计划。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbsorbPlan {
    /// 替换后的文本。
    pub text: String,
    /// 原始 token 数。
    pub original_tokens: u64,
    /// 替换后 token 数。
    pub stub_tokens: u64,
}

impl AbsorbPlan {
    /// 回收的 token 数（下界，因两者都是估算）。
    pub fn reclaimed_tokens(&self) -> u64 {
        self.original_tokens.saturating_sub(self.stub_tokens)
    }
}

/// 工具名是否禁止吸收。
pub fn is_never_absorbed(tool_name: &str) -> bool {
    NEVER_ABSORB_TOOLS.contains(&tool_name)
}

/// 工具名是否命中 `excludeTools` 里的任一「子串或 glob 前后缀」模式。
///
/// 与 `protected` 模块的模式语义保持一致：模式两端允许 `*`，其余按子串匹配
/// （工具名短，子串匹配足够且无需引入正则）。
pub fn excluded_by_config(tool_name: &str, config: &AbsorbConfig) -> bool {
    config.exclude_tools.iter().any(|pattern| {
        let trimmed = pattern.trim();
        if trimmed.is_empty() {
            return false;
        }
        let core = trimmed.trim_matches('*');
        if core.is_empty() {
            return true;
        }
        if trimmed.starts_with('*') && trimmed.ends_with('*') {
            tool_name.contains(core)
        } else if let Some(prefix) = trimmed.strip_suffix('*') {
            tool_name.starts_with(prefix)
        } else if let Some(suffix) = trimmed.strip_prefix('*') {
            tool_name.ends_with(suffix)
        } else {
            tool_name == core
        }
    })
}

/// 按字符边界安全截取前缀。
fn clamp_prefix(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

/// 按字符边界安全截取窗口 `[start, end)`。
fn clamp_window(text: &str, start: usize, end: usize) -> String {
    text.chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect()
}

/// 规划一次吸收；`None` 表示这条结果保持原样。
///
/// `context_usage` 传当前使用率（0 表示未知）；只有 `contextThresholdPct > 0`
/// 时才用它做门槛判断。
pub fn plan_absorb(
    tool_name: &str,
    content: &str,
    is_error: bool,
    context_usage: f64,
    config: &AbsorbConfig,
) -> Option<AbsorbPlan> {
    if !config.enabled || is_error {
        return None;
    }
    if is_never_absorbed(tool_name) || excluded_by_config(tool_name, config) {
        return None;
    }
    if config.context_threshold_pct > 0.0 && context_usage < config.context_threshold_pct {
        return None;
    }
    if content.is_empty() || content.contains(ABSORB_MARKER) {
        return None;
    }

    let original_tokens = count_tokens(content);
    if original_tokens < config.min_tool_tokens {
        return None;
    }

    let total_chars = content.chars().count();
    let keep_prefix = config.keep_prefix_chars;
    let keep_suffix = config.keep_suffix_chars;
    // 中段必须有值得砍掉的东西，否则原样返回。
    if total_chars <= keep_prefix + keep_suffix {
        return None;
    }

    let prefix = clamp_prefix(content, keep_prefix);
    let suffix = clamp_window(
        content,
        total_chars.saturating_sub(keep_suffix),
        total_chars,
    );
    let elided_chars = total_chars - keep_prefix - keep_suffix;
    let marker = format!(
        "...{ABSORB_MARKER} {elided_chars} chars (~{} tokens) elided from {tool_name} output; \
         raw output was discarded — re-run the tool if you need the missing middle.\n\n",
        original_tokens.saturating_sub(count_tokens(&prefix) + count_tokens(&suffix)),
    );
    let text = format!("{prefix}\n\n{marker}{suffix}");

    let stub_tokens = count_tokens(&text);
    if stub_tokens >= original_tokens {
        return None;
    }
    Some(AbsorbPlan {
        text,
        original_tokens,
        stub_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> AbsorbConfig {
        AbsorbConfig {
            enabled: true,
            min_tool_tokens: 1000,
            keep_prefix_chars: 2000,
            keep_suffix_chars: 800,
            ..Default::default()
        }
    }

    #[test]
    fn small_output_should_be_untouched() {
        assert!(plan_absorb("bash", "hello", false, 0.0, &config()).is_none());
    }

    #[test]
    fn large_output_should_be_absorbed_with_head_and_tail() {
        let big = format!("HEAD{}TAIL", "x".repeat(60_000));
        let plan = plan_absorb("bash", &big, false, 0.0, &config()).expect("应吸收");
        assert!(plan.text.starts_with("HEAD"));
        assert!(plan.text.ends_with("TAIL"));
        assert!(plan.text.contains(ABSORB_MARKER));
        assert!(plan.reclaimed_tokens() > 0);
    }

    #[test]
    fn errors_and_acp_tools_should_be_untouched() {
        let big = "x".repeat(60_000);
        assert!(plan_absorb("bash", &big, true, 0.0, &config()).is_none());
        for tool in NEVER_ABSORB_TOOLS {
            assert!(
                plan_absorb(tool, &big, false, 0.0, &config()).is_none(),
                "{tool}"
            );
        }
    }

    #[test]
    fn already_absorbed_should_be_idempotent() {
        let big = format!("HEAD{}TAIL", "x".repeat(60_000));
        let plan = plan_absorb("bash", &big, false, 0.0, &config()).expect("应吸收");
        assert!(plan_absorb("bash", &plan.text, false, 0.0, &config()).is_none());
    }

    #[test]
    fn usage_gate_should_suppress_below_threshold() {
        let big = "x".repeat(60_000);
        let gated = AbsorbConfig {
            context_threshold_pct: 0.5,
            ..config()
        };
        assert!(plan_absorb("bash", &big, false, 0.2, &gated).is_none());
        assert!(plan_absorb("bash", &big, false, 0.7, &gated).is_some());
    }

    #[test]
    fn exclude_patterns_should_match_substring_and_glob() {
        let cfg = AbsorbConfig {
            exclude_tools: vec!["git*".into(), "*diff*".into(), "read".into()],
            ..config()
        };
        assert!(excluded_by_config("git", &cfg));
        assert!(excluded_by_config("git_log", &cfg));
        assert!(excluded_by_config("my_diff_thing", &cfg));
        assert!(excluded_by_config("read", &cfg));
        assert!(!excluded_by_config("bash", &cfg));
        assert!(!excluded_by_config("grep", &cfg));
    }

    #[test]
    fn keep_window_larger_than_body_should_be_untouched() {
        let cfg = AbsorbConfig {
            min_tool_tokens: 1,
            keep_prefix_chars: 10_000,
            keep_suffix_chars: 10_000,
            ..config()
        };
        let mid = "x".repeat(9000);
        assert!(plan_absorb("bash", &mid, false, 0.0, &cfg).is_none());
    }
}
