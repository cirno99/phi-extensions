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
//! 原文可逆：默认把原文落到 `state/absorbed/<handle>.txt`，stub 里带句柄，模型用
//! `acp_decompress <handle>` 即可逐字取回，不必重跑工具（见 [`crate::absorb_store`]）。
//! 仅在原文仓库不可用时才退化为「已丢弃、需重跑」的旧措辞。

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
];

/// 一次吸收的计划。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbsorbPlan {
    /// 替换后的文本。
    pub text: String,
    /// 原文句柄（`aN`）：模型可用 `acp_decompress aN` 取回逐字原文。
    pub handle: String,
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

/// 解析可逆吸收句柄（`a3` / `A3`）。
///
/// 必须带 `a` 前缀：裸数字留给块 id（`b3` / `3`），否则 `acp_decompress 3`
/// 会歧义。
pub fn parse_absorb_handle(arg: &str) -> Option<String> {
    let trimmed = arg.trim().to_ascii_lowercase();
    let digits = trimmed.strip_prefix('a')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u64 = digits.parse().ok()?;
    if n == 0 {
        return None;
    }
    Some(format!("a{n}"))
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

/// 保留窗口的下限：再激进也不能把 stub 压到连错误行与结论都看不见。
const MIN_KEEP_PREFIX_CHARS: usize = 400;
const MIN_KEEP_SUFFIX_CHARS: usize = 150;
/// 使用率到达该值时保留窗口缩到下限（同时也是「超限压力带」的开端）。
const AGGRESSIVE_AT_USAGE: f64 = 0.95;

/// 压力插值系数：使用率从门槛升到 [`AGGRESSIVE_AT_USAGE`] 时由 0 线性升到 1。
///
/// 门槛缺失（0）时按满压起点处理。`resolve_keep_window` 与 `resolve_min_tokens`
/// 共用同一套压力计算，避免两处各写一遍而漂移。
fn pressure_ratio(usage: f64, config: &AbsorbConfig) -> f64 {
    let floor_pct = config.context_threshold_pct.max(0.0);
    // 从门槛到 AGGRESSIVE_AT_USAGE 之间线性加码；门槛缺失时按满压处理。
    let span = (AGGRESSIVE_AT_USAGE - floor_pct).max(f64::EPSILON);
    ((usage - floor_pct) / span).clamp(0.0, 1.0)
}

/// 根据当前使用率解析本次吸收的保留窗口。
///
/// # 为什么是自适应的
///
/// 固定窗口会给出固定的水位：不管上下文是 60K 还是 170K，每次都只删同样多，
/// 于是使用率高时吸不赢新增量，只能看着上下文慢慢堆积。
///
/// 这里把保留窗口做成使用率的函数：刚过门槛时几乎保持配置值（信息留得最全），
/// 接近上限时缩到下限（回收量最大）。于是吸收量随压力单调递增，形成一个
/// **自调节的负反馈**：使用率越高→删得越多→回落；回落低于门槛后又停止动手，
/// 让上下文自然长回来。最终上下文在「门槛水位 ~ 上限」之间波动，而不是单边堆积。
///
/// 返回 `(保留前缀字符数, 保留后缀字符数)`。
///
/// 两端都是**硬插值**：t=0 时等于配置值，t=1 时落到 [`MIN_KEEP_PREFIX_CHARS`] /
/// [`MIN_KEEP_SUFFIX_CHARS`]。配置值已经小于下限时以配置值为准（用户显式指定优先）。
fn resolve_keep_window(usage: f64, config: &AbsorbConfig) -> (usize, usize) {
    let t = pressure_ratio(usage, config);
    let lerp = |configured: usize, floor: usize| -> usize {
        if configured <= floor {
            return configured;
        }
        let value = configured as f64 - (configured - floor) as f64 * t;
        value.round() as usize
    };
    (
        lerp(config.keep_prefix_chars, MIN_KEEP_PREFIX_CHARS),
        lerp(config.keep_suffix_chars, MIN_KEEP_SUFFIX_CHARS),
    )
}

/// 根据使用率解析本次吸收的最小 token 门槛。
///
/// 压力越大，越值得为「中等大小」的输出付一次重跑成本：门槛从配置值
/// 降到下限 200 token。这样高水位时每个工具结果都能贡献一点回收量，
/// 而不是只有巨型输出才被处理。
fn resolve_min_tokens(usage: f64, config: &AbsorbConfig) -> u64 {
    /// 高压下的最小门槛：再小的输出也不值得为它付重跑成本。
    const MIN_TOKENS_FLOOR: u64 = 200;
    let configured = config.min_tool_tokens.max(MIN_TOKENS_FLOOR);
    let t = pressure_ratio(usage, config);
    // 从配置值线性降到下限（最多降 60%）。
    let scaled = configured as f64 * (1.0 - 0.6 * t);
    (scaled as u64).clamp(MIN_TOKENS_FLOOR, configured)
}

/// 规划一次吸收；`None` 表示这条结果保持原样。
/// `handle` 是调用方预分配的句柄（`aN`）：`reversible` 为真时 stub 里会带上它，
/// 模型据此用 `acp_decompress <handle>` 取回原文（调用方负责把原文写进
/// [`crate::absorb_store`]）；为假时退化为「已丢弃、需重跑」的旧措辞，
/// 不向模型承诺一个取不回来的句柄。
///
/// `context_usage` 传当前使用率（0 表示未知）；只有 `contextThresholdPct > 0`
/// 时才用它做门槛判断。
pub fn plan_absorb(
    tool_name: &str,
    content: &str,
    is_error: bool,
    context_usage: f64,
    config: &AbsorbConfig,
    handle: &str,
    reversible: bool,
) -> Option<AbsorbPlan> {
    if !config.enabled || is_error {
        return None;
    }
    if is_never_absorbed(tool_name) || excluded_by_config(tool_name, config) {
        return None;
    }
    if content.is_empty() || content.contains(ABSORB_MARKER) {
        return None;
    }

    let original_tokens = count_tokens(content);
    let min_tokens = resolve_min_tokens(context_usage, config);
    if original_tokens < min_tokens {
        return None;
    }
    if config.context_threshold_pct > 0.0 && context_usage < config.context_threshold_pct {
        // 门槛之下本应不动手（保留「先长后收」的波动），但**巨型**输出是例外：
        // 一条 ≥ `always_above_tokens` 的结果无论当前水位多少，完整留在历史里
        // 都是纯噪声，压成 stub 的信息损失远小于它占用的上下文。
        if config.always_above_tokens == 0 || original_tokens < config.always_above_tokens {
            return None;
        }
    }

    let (keep_prefix, keep_suffix) = resolve_keep_window(context_usage, config);
    let total_chars = content.chars().count();
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
        "...{ABSORB_MARKER} {elided_chars} chars (~{} tokens) elided from {tool_name} output; {}",
        original_tokens.saturating_sub(count_tokens(&prefix) + count_tokens(&suffix)),
        if reversible {
            format!(
                "full output stored as `{handle}` — call `acp_decompress {handle}` to restore it verbatim.\n\n"
            )
        } else {
            "raw output was discarded — re-run the tool if you need the missing middle.\n\n"
                .to_string()
        },
    );
    let text = format!("{prefix}\n\n{marker}{suffix}");

    let stub_tokens = count_tokens(&text);
    if stub_tokens >= original_tokens {
        return None;
    }
    Some(AbsorbPlan {
        text,
        handle: handle.to_string(),
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
        assert!(plan_absorb("bash", "hello", false, 0.0, &config(), "a1", true).is_none());
    }

    #[test]
    fn large_output_should_be_absorbed_with_head_and_tail() {
        let big = format!("HEAD{}TAIL", "x".repeat(60_000));
        let plan = plan_absorb("bash", &big, false, 0.0, &config(), "a1", true).expect("应吸收");
        assert!(plan.text.starts_with("HEAD"));
        assert!(plan.text.ends_with("TAIL"));
        assert!(plan.text.contains(ABSORB_MARKER));
        // 可逆：stub 必须带句柄，模型据此用 acp_decompress 取回原文。
        assert_eq!(plan.handle, "a1");
        assert!(plan.text.contains("`a1`"));
        assert!(plan.text.contains("acp_decompress a1"));
        assert!(!plan.text.contains("raw output was discarded"));
        assert!(plan.reclaimed_tokens() > 0);
    }

    #[test]
    fn absorb_handle_should_require_a_prefix() {
        assert_eq!(parse_absorb_handle("a3").as_deref(), Some("a3"));
        assert_eq!(parse_absorb_handle(" A12 ").as_deref(), Some("a12"));
        // 裸数字属于块 id，不应当被当成句柄。
        assert!(parse_absorb_handle("3").is_none());
        assert!(parse_absorb_handle("b3").is_none());
        assert!(parse_absorb_handle("a0").is_none());
        assert!(parse_absorb_handle("ax").is_none());
    }

    #[test]
    fn errors_and_acp_tools_should_be_untouched() {
        let big = "x".repeat(60_000);
        assert!(plan_absorb("bash", &big, true, 0.0, &config(), "a1", true).is_none());
        for tool in NEVER_ABSORB_TOOLS {
            assert!(
                plan_absorb(tool, &big, false, 0.0, &config(), "a1", true).is_none(),
                "{tool}"
            );
        }
    }

    #[test]
    fn already_absorbed_should_be_idempotent() {
        let big = format!("HEAD{}TAIL", "x".repeat(60_000));
        let plan = plan_absorb("bash", &big, false, 0.0, &config(), "a1", true).expect("应吸收");
        assert!(plan_absorb("bash", &plan.text, false, 0.0, &config(), "a1", true).is_none());
    }

    #[test]
    fn usage_gate_should_suppress_below_threshold() {
        let big = "x".repeat(60_000);
        // 关掉「巨型输出例外」，单独验证使用率门槛本身。
        let gated = AbsorbConfig {
            context_threshold_pct: 0.5,
            always_above_tokens: 0,
            ..config()
        };
        assert!(plan_absorb("bash", &big, false, 0.2, &gated, "a1", true).is_none());
        assert!(plan_absorb("bash", &big, false, 0.7, &gated, "a1", true).is_some());
    }

    /// 门槛之下，「巨型」输出仍应被吸收：否则早期会话 / 高门槛会话里，
    /// 一条几万 token 的构建日志会完整留在历史中，直到水位涨到门槛才被处理。
    #[test]
    fn huge_output_should_be_absorbed_even_below_threshold() {
        let gated = AbsorbConfig {
            context_threshold_pct: 0.5,
            always_above_tokens: 2000,
            ..config()
        };
        // ~15000 token，远超 always_above_tokens：低水位也吸收。
        let huge = "x".repeat(60_000);
        assert!(plan_absorb("bash", &huge, false, 0.1, &gated, "a1", true).is_some());
        // ~1000 token，低于 always_above_tokens：低水位不吸收。
        let medium = "x".repeat(4_000);
        assert!(plan_absorb("bash", &medium, false, 0.1, &gated, "a1", true).is_none());
        // 越过门槛后，同一「中等」输出由常规门槛接管（min_tool_tokens 已降）。
        assert!(plan_absorb("bash", &medium, false, 0.9, &gated, "a1", true).is_some());
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
        assert!(plan_absorb("bash", &mid, false, 0.0, &cfg, "a1", true).is_none());
    }

    /// 保留窗口随使用率单调收缩：这是「吸到高水位就多删」的调节回路。
    #[test]
    fn keep_window_should_shrink_as_usage_rises() {
        let cfg = AbsorbConfig {
            context_threshold_pct: 0.30,
            keep_prefix_chars: 2000,
            keep_suffix_chars: 800,
            ..config()
        };
        let (p_low, s_low) = resolve_keep_window(0.30, &cfg);
        let (p_mid, s_mid) = resolve_keep_window(0.60, &cfg);
        let (p_high, s_high) = resolve_keep_window(0.95, &cfg);
        assert!(p_low >= p_mid && p_mid >= p_high, "前缀窗口应递减");
        assert!(s_low >= s_mid && s_mid >= s_high, "后缀窗口应递减");
        // 刚过门槛时保持配置值。
        assert_eq!((p_low, s_low), (2000, 800));
        // 到达超限压力带时落到下限（信息最少但仍可见）。
        assert_eq!(
            (p_high, s_high),
            (MIN_KEEP_PREFIX_CHARS, MIN_KEEP_SUFFIX_CHARS)
        );
    }

    /// 高水位下「中等大小」的输出也应被吸收：否则新增量吸不赢，只能堆积。
    #[test]
    fn higher_usage_should_absorb_more_outputs() {
        let cfg = AbsorbConfig {
            context_threshold_pct: 0.30,
            min_tool_tokens: 1000,
            keep_prefix_chars: 2000,
            keep_suffix_chars: 800,
            ..config()
        };
        // ~800 token（3200 字符）的中等输出：门槛 1000 时不够格。
        let mid = format!("HEAD{}TAIL", "x".repeat(3200));
        assert!(plan_absorb("bash", &mid, false, 0.30, &cfg, "a1", true).is_none());
        // 接近上限时门槛降到 400（降 60%），同一条输出被吸收。
        assert!(plan_absorb("bash", &mid, false, 0.95, &cfg, "a1", true).is_some());
    }

    /// 回收量必须随压力单调递增（更高使用率 → 保留窗口更小 → 删得更多）。
    #[test]
    fn reclaimed_tokens_should_grow_with_usage() {
        let cfg = AbsorbConfig {
            context_threshold_pct: 0.30,
            keep_prefix_chars: 2000,
            keep_suffix_chars: 800,
            ..config()
        };
        let big = format!("HEAD{}TAIL", "y".repeat(80_000));
        let low = plan_absorb("bash", &big, false, 0.35, &cfg, "a1", true).expect("应吸收");
        let high = plan_absorb("bash", &big, false, 0.95, &cfg, "a1", true).expect("应吸收");
        assert!(
            high.reclaimed_tokens() > low.reclaimed_tokens(),
            "高压下应回收更多：low={} high={}",
            low.reclaimed_tokens(),
            high.reclaimed_tokens()
        );
    }
}
