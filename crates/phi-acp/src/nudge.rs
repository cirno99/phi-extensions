//! 提醒（nudge）决策与渲染 —— 对应 acp-kernel `src/compress.ts`(decideNudge)
//! 与 `src/nudge-text.ts`。

use std::collections::BTreeMap;

use crate::prune::SUMMARY_HEADER;
use crate::state::active_blocks;
use crate::tokenize::count_message_tokens;
use crate::types::{
    BlockSpan, CompressibleRange, CompressionBlock, CompressionState, Config, ContentType,
    CoreMessage, NudgeBreakdown, NudgeDecision, ProtectedRange, Recommendation, Role,
};

/// 提醒的上下文细分。
#[derive(Debug, Clone, Default)]
pub struct ContextBreakdown {
    /// 系统。
    pub system: u64,
    /// 工具。
    pub tool: u64,
    /// 摘要。
    pub summaries: u64,
    /// 代码。
    pub code: u64,
    /// 文本。
    pub text: u64,
    /// 总计。
    pub total: u64,
    /// 自上次提醒以来的增长。
    pub growth: u64,
}

/// 自适应增长阈值。
pub fn resolve_adaptive_growth(model_context_limit: u64, config: &Config) -> u64 {
    let nudge = &config.nudge;
    if model_context_limit == 0 {
        return nudge.growth_floor;
    }
    let scaled = (model_context_limit as f64 * nudge.growth_ratio).round() as u64;
    scaled.clamp(nudge.growth_floor, nudge.growth_cap)
}

/// 压力带最小收益。
pub fn resolve_min_pressure_benefit(model_context_limit: u64, config: &Config) -> u64 {
    config.nudge.min_pressure_benefit_tokens.unwrap_or_else(|| {
        let scaled = (model_context_limit as f64 * 0.01).round() as u64;
        scaled.max(5000)
    })
}

/// 各层级的待压缩量与目标块。
fn pending_by_tier(
    state: &CompressionState,
    recommendation: Option<&Recommendation>,
    min_compress_range: usize,
) -> BTreeMap<u8, (u64, Vec<CompressionBlock>)> {
    let mut out = BTreeMap::new();
    let merged = recommendation
        .map(|r| r.recommended_ranges.as_slice())
        .unwrap_or(&[]);
    let effective: Vec<&CompressibleRange> = if min_compress_range > 0 {
        merged
            .iter()
            .filter(|r| r.chars.unwrap_or((r.tokens * 4) as usize) >= min_compress_range)
            .collect()
    } else {
        merged.iter().collect()
    };
    out.insert(1, (effective.iter().map(|r| r.tokens).sum(), Vec::new()));

    let active = active_blocks(state);
    let t1: Vec<CompressionBlock> = active
        .iter()
        .filter(|b| b.tier == 1)
        .map(|b| (*b).clone())
        .collect();
    let t2: Vec<CompressionBlock> = active
        .iter()
        .filter(|b| b.tier == 2)
        .map(|b| (*b).clone())
        .collect();
    let t1_pending: u64 = t1
        .iter()
        .map(|b| crate::tokenize::count_tokens(&b.summary))
        .sum();
    let t2_pending: u64 = t2
        .iter()
        .map(|b| crate::tokenize::count_tokens(&b.summary))
        .sum();
    out.insert(2, (t1_pending, t1));
    out.insert(3, (t2_pending, t2));
    out
}

/// 计算上下文细分。
pub fn compute_context_breakdown(
    messages: &[CoreMessage],
    total: u64,
    growth: u64,
) -> ContextBreakdown {
    let mut bd = ContextBreakdown {
        total,
        growth,
        ..Default::default()
    };
    for message in messages {
        let tokens = count_message_tokens(message);
        if message.text_str().starts_with(SUMMARY_HEADER) {
            bd.summaries += tokens;
        } else if matches!(
            message.content_type,
            ContentType::ToolCall | ContentType::ToolResult
        ) {
            bd.tool += tokens;
        } else if message.role == Role::System {
            bd.system += tokens;
        } else if message.text_str().contains("```") {
            bd.code += tokens;
        } else {
            bd.text += tokens;
        }
    }
    bd
}

/// 活跃块的 ref 跨度。
pub fn active_block_spans(state: &CompressionState) -> Vec<BlockSpan> {
    crate::block_map::active_block_spans(state)
}

/// 提醒决策输入。
pub struct NudgeInput<'a> {
    /// 当前 token 数。
    pub token_count: u64,
    /// 配置。
    pub config: &'a Config,
    /// 状态。
    pub state: &'a CompressionState,
    /// 消息。
    pub messages: &'a [CoreMessage],
    /// 推荐结果。
    pub recommendation: Option<&'a Recommendation>,
}

/// 决策是否注入提醒以及注入哪一层。
pub fn decide_nudge(input: NudgeInput<'_>) -> NudgeDecision {
    let config = input.config;
    let state = input.state;
    let token_count = input.token_count;
    let limit = config.model_context_limit;
    let usage = if limit > 0 {
        token_count as f64 / limit as f64
    } else {
        0.0
    };

    let nudge_growth_tokens = resolve_adaptive_growth(limit, config);
    let min_pressure_benefit = resolve_min_pressure_benefit(limit, config);

    let over_limit = usage >= config.nudge.max_context_limit_pct;
    let emergency_override = usage >= config.nudge.emergency_threshold_pct;
    let pressure = over_limit || emergency_override;

    let baseline = state.nudge.last_per_message_nudge_tokens;
    let had_pending_nudge = state.nudge.last_nudge_shown_tokens > 0;
    let effective_threshold = if had_pending_nudge {
        nudge_growth_tokens / 2
    } else {
        nudge_growth_tokens
    };

    let growth_reference = if state.nudge.last_nudge_shown_tokens > 0 {
        state.nudge.last_nudge_shown_tokens
    } else if baseline > 0 {
        baseline
    } else {
        token_count
    };
    let growth_floor = config
        .nudge
        .min_growth_floor
        .max((config.nudge.min_growth_ratio * nudge_growth_tokens as f64) as u64);
    let growth_since_reference = token_count.saturating_sub(growth_reference);

    let tiers = pending_by_tier(
        state,
        input.recommendation,
        config.compress.min_compress_range,
    );

    let tier2_threshold =
        (nudge_growth_tokens as f64 * config.nudge.tier2_growth_multiplier).round() as u64;

    let t1_eff = tiers.get(&1).map(|(p, _)| *p).unwrap_or(0);
    let t2_pen = tiers.get(&2).map(|(p, _)| *p).unwrap_or(0);
    let t3_pen = tiers.get(&3).map(|(p, _)| *p).unwrap_or(0);
    let max_pending = t1_eff.max(t2_pen).max(t3_pen);

    let first_sight_mass_ready = state.nudge.last_nudge_shown_tokens == 0
        && baseline == 0
        && usage >= config.nudge.min_context_limit_pct
        && max_pending >= nudge_growth_tokens;
    let growth_ready = first_sight_mass_ready || growth_since_reference >= growth_floor;

    let t2_count = tiers.get(&2).map(|(_, b)| b.len()).unwrap_or(0);
    let t3_count = tiers.get(&3).map(|(_, b)| b.len()).unwrap_or(0);
    let tier_count_usage_floor = config.nudge.min_context_limit_pct;
    let t2_count_ready = t2_count >= config.tiers.tier2_trigger && usage >= tier_count_usage_floor;
    let t3_count_ready = t3_count >= config.tiers.tier3_trigger && usage >= tier_count_usage_floor;

    let mut injected_tier: Option<u8> = None;
    let mut injected_reason = String::new();
    let mut best_pending = 0u64;

    if pressure {
        let mut candidates = vec![1u8];
        if config.tiers.enabled {
            candidates.push(2);
            candidates.push(3);
        }
        let mut best: Option<u8> = None;
        for t in candidates {
            let p = tiers.get(&t).map(|(p, _)| *p).unwrap_or(0);
            if p > best_pending {
                best_pending = p;
                best = Some(t);
            }
        }
        if let Some(best) = best {
            if best_pending >= min_pressure_benefit {
                injected_tier = Some(best);
                let label = if emergency_override {
                    "EMERGENCY"
                } else {
                    "OVER-LIMIT"
                };
                injected_reason = if best == 1 {
                    format!(
                        "{label} T1: max effective pending {best_pending}, usage {}%",
                        (usage * 100.0).round()
                    )
                } else {
                    format!(
                        "{label} T{best} distill: max pending {best_pending} (T1 effective {t1_eff}, T2 {t2_pen}, T3 {t3_pen}), usage {}%",
                        (usage * 100.0).round()
                    )
                };
            }
        }
    } else if growth_ready {
        if t1_eff >= nudge_growth_tokens {
            injected_tier = Some(1);
            injected_reason = format!(
                "T1 effective {t1_eff} >= {nudge_growth_tokens}, growth {growth_since_reference}, usage {}%",
                (usage * 100.0).round()
            );
        } else if config.tiers.enabled
            && (t2_count_ready || (t2_pen >= tier2_threshold && t2_pen > t1_eff))
        {
            let last_shown = state.nudge.last_shown_by_tier.get(&2).copied().unwrap_or(0);
            if last_shown == 0 || token_count.saturating_sub(last_shown) >= growth_floor {
                injected_tier = Some(2);
                injected_reason = format!(
                    "T2 distill ready: {t2_count} tier-1 blocks ({t2_pen} tokens), usage {}%",
                    (usage * 100.0).round()
                );
            }
        } else if config.tiers.enabled
            && (t3_count_ready || (t3_pen >= tier2_threshold && t3_pen > t2_pen && t3_pen > t1_eff))
        {
            let last_shown = state.nudge.last_shown_by_tier.get(&3).copied().unwrap_or(0);
            if last_shown == 0 || token_count.saturating_sub(last_shown) >= growth_floor {
                injected_tier = Some(3);
                injected_reason = format!(
                    "T3 condense ready: {t3_count} tier-2 blocks ({t3_pen} tokens), usage {}%",
                    (usage * 100.0).round()
                );
            }
        }
    }

    let should_inject = injected_tier.is_some();
    if should_inject && first_sight_mass_ready {
        injected_reason.push_str(" [first-sight mass]");
    }

    let reason = if injected_tier.is_some() {
        injected_reason
    } else if pressure {
        let label = if emergency_override {
            "EMERGENCY"
        } else {
            "OVER-LIMIT"
        };
        if best_pending == 0 {
            format!(
                "{label}: usage {}% but no tier has effective compressible content (T1 effective {t1_eff}, T2 {t2_pen}, T3 {t3_pen})",
                (usage * 100.0).round()
            )
        } else {
            format!(
                "{label}: usage {}% but max pending {best_pending} < min benefit {min_pressure_benefit} tokens",
                (usage * 100.0).round()
            )
        }
    } else {
        let pending_short = max_pending < nudge_growth_tokens;
        let growth_short = growth_since_reference < growth_floor;
        let mut parts = Vec::new();
        if pending_short {
            parts.push(format!(
                "max compressible {max_pending} < threshold {nudge_growth_tokens}"
            ));
        }
        if growth_short {
            parts.push(format!(
                "growth {growth_since_reference} < floor {growth_floor}"
            ));
        }
        if parts.is_empty() {
            parts.push(format!(
                "max compressible {max_pending}, growth {growth_since_reference}"
            ));
        }
        parts.join("; ")
    };

    let breakdown = NudgeBreakdown {
        usage,
        growth: growth_since_reference,
        growth_reference,
        effective_threshold,
        nudge_growth_tokens,
        growth_floor,
        has_pending_nudge: u64::from(had_pending_nudge),
        over_limit: u64::from(over_limit),
        emergency_override: u64::from(emergency_override),
        pending_t1: t1_eff,
        pending_t2: t2_pen,
        pending_t3: t3_pen,
        max_pending,
        min_pressure_benefit,
    };

    let (compressible_ranges, protected_ranges) = match input.recommendation {
        Some(rec) => (
            rec.recommended_ranges.clone(),
            rec.context_ranges.protected.clone(),
        ),
        None => (Vec::new(), Vec::new()),
    };
    let tier_target_blocks = injected_tier
        .and_then(|t| tiers.get(&t).map(|(_, b)| b.clone()))
        .unwrap_or_default();

    NudgeDecision {
        should_inject,
        reason,
        compressible_ranges,
        protected_ranges,
        active_block_spans: active_block_spans(state),
        tier_target_blocks,
        context_usage: usage,
        tier: injected_tier,
        breakdown,
    }
}

fn format_k(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}K", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

fn format_breakdown(bd: &ContextBreakdown) -> String {
    let mut parts = Vec::new();
    if bd.system > 0 {
        parts.push(format!("{} system", format_k(bd.system)));
    }
    if bd.tool > 0 {
        parts.push(format!("{} tool", format_k(bd.tool)));
    }
    if bd.summaries > 0 {
        parts.push(format!("{} summaries", format_k(bd.summaries)));
    }
    if bd.code > 0 {
        parts.push(format!("{} code", format_k(bd.code)));
    }
    if bd.text > 0 {
        parts.push(format!("{} text", format_k(bd.text)));
    }
    let growth = if bd.growth > 0 {
        format!("\n+{} since last nudge", format_k(bd.growth))
    } else {
        String::new()
    };
    format!("Context breakdown: {}{growth}", parts.join(" | "))
}

/// 格式化可压缩 / 受保护范围为单块最旧优先列表。
pub fn format_ranges(compressible: &[CompressibleRange], protected: &[ProtectedRange]) -> String {
    if compressible.is_empty() && protected.is_empty() {
        return "[No specific ranges detected — compress any consumed content.]".to_string();
    }
    let mut lines = Vec::new();
    let total = compressible.len() + protected.len();
    for r in compressible {
        let user_note = match r.user_msgs.unwrap_or(0) {
            0 => String::new(),
            n => format!(" · {n} user msg{}", if n > 1 { "s" } else { "" }),
        };
        lines.push(format!(
            "  {}–{}  {} msgs  {} [tool {}% | text {}%]{}",
            r.start_ref,
            r.end_ref,
            r.count,
            format_k(r.tokens),
            r.tool_pct,
            r.text_pct,
            user_note
        ));
    }
    for r in protected {
        lines.push(format!(
            "  {}–{}  {} msgs  {} [PROTECTED: {} — not compressible]",
            r.start_ref,
            r.end_ref,
            r.count,
            format_k(r.tokens),
            r.tools.join(", ")
        ));
    }
    format!(
        "Compressible ranges ({total}, oldest first):\n{}",
        lines.join("\n")
    )
}

/// 渲染提醒文本。
pub fn render_nudge_text(
    decision: &NudgeDecision,
    prompts: &crate::prompts::Prompts,
    context_breakdown: Option<&ContextBreakdown>,
) -> String {
    let breakdown_str = context_breakdown.map(format_breakdown).unwrap_or_default();
    let ranges_str = format_ranges(&decision.compressible_ranges, &decision.protected_ranges);
    let is_emergency =
        decision.breakdown.emergency_override > 0 || decision.breakdown.over_limit > 0;

    let mut parts: Vec<String> = Vec::new();
    if let Some(tier) = decision.tier {
        if tier >= 2 {
            let is_t2 = tier == 2;
            let head = if is_emergency {
                format!(
                    "⚠️ Context limit reached — compress now. Prioritize consumed tool outputs.\n\n{}",
                    prompts.compress_philosophy
                )
            } else {
                format!(
                    "This is an efficiency nudge to compress early and keep context lean — not an overflow warning.\n\n{}",
                    prompts.compress_philosophy
                )
            };
            parts.push(head);
            parts.push(String::new());
            parts.push(breakdown_str);
            parts.push(String::new());
            parts.push(if is_emergency {
                format!(
                    "[EMERGENCY — TIER {tier} {}] Context limit reached — distill NOW.",
                    if is_t2 {
                        "DISTILLATION"
                    } else {
                        "CONDENSATION"
                    }
                )
            } else {
                format!(
                    "[TIER {tier} {} TRIGGER]",
                    if is_t2 {
                        "DISTILLATION"
                    } else {
                        "CONDENSATION"
                    }
                )
            });
            parts.push(if is_t2 {
                prompts.tier2_distill_rules.clone()
            } else {
                prompts.tier3_condense_rules.clone()
            });
            parts.push(prompts.how_to_compress_rules.clone());
            return compact(parts).join("\n");
        }
    }

    if is_emergency {
        parts.push(format!(
            "⚠️ Context limit reached — compress now. Prioritize consumed tool outputs.\n\n{}",
            prompts.compress_philosophy
        ));
        parts.push(String::new());
        parts.push(breakdown_str);
        parts.push(String::new());
        parts.push(prompts.how_to_compress_rules.clone());
        parts.push(String::new());
        parts.push(ranges_str);
        return compact(parts).join("\n");
    }

    parts.push(format!(
        "This is an efficiency nudge to compress early and keep context lean — not an overflow warning. A separate, stronger alert will appear if the context is actually full.\n\n{}",
        prompts.compress_philosophy
    ));
    parts.push(String::new());
    parts.push(breakdown_str);
    parts.push(String::new());
    parts.push(prompts.how_to_compress_rules.clone());
    parts.push(String::new());
    parts.push(ranges_str);
    parts.push(String::new());
    parts.push(
        "💡 Compress all ranges in one call (pass multiple content entries: `content: [{...}, {...}]`)."
            .to_string(),
    );
    compact(parts).join("\n")
}

/// 渲染人工触发（`/acp compress`）的压缩指令文本。
///
/// 与 [`render_nudge_text`] 的自动提醒不同：人工指令绕过增长率 / 使用率门限，
/// 只要存在可压缩范围就生成一份「立即压缩」指令，交由模型调用 `compress` 工具。
pub fn render_manual_compress_text(
    decision: &NudgeDecision,
    prompts: &crate::prompts::Prompts,
) -> String {
    let ranges_str = format_ranges(&decision.compressible_ranges, &decision.protected_ranges);
    let parts: Vec<String> = vec![
        "[MANUAL COMPRESS] The user requested compression explicitly. Follow the rules below \
         and call the `compress` tool now in a single call."
            .to_string(),
        String::new(),
        prompts.compress_philosophy.clone(),
        String::new(),
        prompts.how_to_compress_rules.clone(),
        String::new(),
        ranges_str,
        String::new(),
        "💡 Compress all ranges in one call (pass multiple content entries: `content: [{...}, {...}]`)."
            .to_string(),
    ];
    compact(parts).join("\n")
}

fn compact(parts: Vec<String>) -> Vec<String> {
    let mut out = parts;
    while out.first().is_some_and(|s| s.is_empty()) {
        out.remove(0);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adaptive_growth_should_clamp() {
        let mut config = Config::default_for(200_000);
        // 200000 * 0.05 = 10000 -> clamp 到 floor 50000。
        assert_eq!(resolve_adaptive_growth(200_000, &config), 50_000);
        config.nudge.growth_ratio = 1.0;
        assert_eq!(resolve_adaptive_growth(200_000, &config), 50_000);
    }

    #[test]
    fn min_pressure_benefit_should_scale_with_limit() {
        let config = Config::default_for(200_000);
        assert_eq!(resolve_min_pressure_benefit(200_000, &config), 5000);
        let config2 = Config::default_for(2_000_000);
        assert_eq!(resolve_min_pressure_benefit(2_000_000, &config2), 20_000);
    }

    #[test]
    fn decide_nudge_should_suppress_below_thresholds() {
        let messages = vec![CoreMessage::text("a", Role::User, "hi")];
        let mut state = crate::state::create_initial_state();
        state.nudge.last_per_message_nudge_tokens = 100;
        let config = Config::default_for(200_000);
        let decision = decide_nudge(NudgeInput {
            token_count: 1000,
            config: &config,
            state: &state,
            messages: &messages,
            recommendation: None,
        });
        assert!(!decision.should_inject);
    }

    #[test]
    fn decide_nudge_should_inject_on_emergency_with_pending() {
        let messages = vec![CoreMessage::text("a", Role::User, "hi")];
        let state = crate::state::create_initial_state();
        let config = Config::default_for(100_000);
        let rec = Recommendation {
            recommended_ranges: vec![CompressibleRange {
                start_ref: "m00001".into(),
                end_ref: "m00002".into(),
                count: 2,
                tokens: 60_000,
                chars: Some(240_000),
                ..Default::default()
            }],
            ..Default::default()
        };
        let decision = decide_nudge(NudgeInput {
            token_count: 99_000,
            config: &config,
            state: &state,
            messages: &messages,
            recommendation: Some(&rec),
        });
        assert!(decision.should_inject);
        assert_eq!(decision.tier, Some(1));
    }

    #[test]
    fn manual_compress_text_should_include_header_and_ranges() {
        let decision = NudgeDecision {
            compressible_ranges: vec![CompressibleRange {
                start_ref: "m00001".into(),
                end_ref: "m00010".into(),
                count: 10,
                tokens: 1_234,
                ..Default::default()
            }],
            ..Default::default()
        };
        let text = render_manual_compress_text(&decision, &crate::prompts::Prompts::default());
        assert!(text.starts_with("[MANUAL COMPRESS]"));
        assert!(text.contains("m00001"));
        assert!(text.contains("HOW TO COMPRESS"));
    }

    #[test]
    fn tuned_profile_should_nudge_earlier_than_default() {
        let messages = vec![CoreMessage::text("a", Role::User, "hi")];
        let state = crate::state::create_initial_state();
        let rec = Recommendation {
            recommended_ranges: vec![CompressibleRange {
                start_ref: "m00001".into(),
                end_ref: "m00004".into(),
                count: 4,
                tokens: 20_000,
                ..Default::default()
            }],
            ..Default::default()
        };
        // 使用率 30%、可压缩 20k：内核默认（min 45% / 阈值 50k）不触发。
        let mut config = Config::default_for(200_000);
        let default_decision = decide_nudge(NudgeInput {
            token_count: 60_000,
            config: &config,
            state: &state,
            messages: &messages,
            recommendation: Some(&rec),
        });
        assert!(!default_decision.should_inject);
        // 调优后的默认阈值（min 30% / 阈值 20k）则触发 T1。
        config.nudge.growth_floor = 20_000;
        config.nudge.growth_cap = 20_000;
        config.nudge.min_growth_floor = 10_000;
        config.nudge.min_context_limit_pct = 0.30;
        config.nudge.max_context_limit_pct = 0.65;
        let tuned_decision = decide_nudge(NudgeInput {
            token_count: 60_000,
            config: &config,
            state: &state,
            messages: &messages,
            recommendation: Some(&rec),
        });
        assert!(tuned_decision.should_inject);
        assert_eq!(tuned_decision.tier, Some(1));
    }
}
