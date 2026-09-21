//! `/acp` 斜杠命令 —— 对应 billion-context 的 `/acp` 面板。

use phi_ext::phi;

use crate::config::{self, EXTENSION_NAME};
use crate::runtime::Shared;

/// 用法说明。
const USAGE: &str = "📋 /acp 子命令：\n\
  status                       — 上下文使用率、块统计与可压缩范围（默认）\n\
  compress                     — 查看当前可压缩范围（仅信息展示；phi 无请求体钩子，compress 不减少上游 token）\n\
  absorb                       — absorb 诊断：真正回收上下文的通道（阈值 / 已回收 / 明细）\n\
  enable | disable             — 开关扩展\n\
  config <key> <value>         — 设置配置（context-limit / render-tags / min-compress / max-summary-ratio / host-tokens / growth-tokens / min-growth-tokens / min-context-pct / max-context-pct / tier2-trigger / tier3-trigger / absorb / absorb-min-tokens / absorb-keep-prefix / absorb-keep-suffix / absorb-threshold-pct / absorb-always-above / auto-nudge）\n\
  rules                        — 列出持久规则\n\
  reset                        — 清空当前会话观测视图与压缩状态（需确认）\n\
  help                         — 显示本说明";

/// 注册命令。
pub fn register(ext: &mut phi::Extension, shared: Shared) {
    ext.register_command(
        "acp",
        phi::Command::new("ACP 上下文压缩状态与配置", move |args, ctx| {
            // 命令处理器是 SDK 唯一能拿到宿主 cwd 的地方，回填给 `session_tokens`
            // 用于精确定位当前项目的会话 JSONL（避免读到别的项目的 usage）。
            crate::session_tokens::set_host_cwd(ctx.cwd());

            let tokens: Vec<&str> = args.split_whitespace().collect();
            let subcommand = tokens.first().copied().unwrap_or("status");

            // 先刷出回调缓存的告警。
            let notices = shared.borrow_mut().take_notices();
            for notice in notices {
                ctx.notify("info", notice.as_str());
            }

            match subcommand {
                "status" => {
                    let report = {
                        let mut guard = shared.borrow_mut();
                        let outcome = guard.process();
                        let tokens = guard.effective_token_count();
                        let source = if guard.config.use_host_tokens
                            && crate::session_tokens::last_read_used_host()
                        {
                            "host"
                        } else {
                            "local estimate"
                        };
                        let report = crate::compress::status(
                            &guard.state,
                            tokens,
                            &guard.config.to_kernel_config(),
                        );
                        let observed = guard.estimate_tokens();
                        let mut empty_ranges = true;
                        let ranges = outcome
                            .nudge
                            .as_ref()
                            .map(|n| {
                                empty_ranges = n.compressible_ranges.is_empty();
                                crate::nudge::format_ranges(
                                    &n.compressible_ranges,
                                    &n.protected_ranges,
                                )
                            })
                            .unwrap_or_default();
                        let absorbed = guard.state.stats.absorbed_tokens;
                        let gate = guard.config.absorb_context_threshold_pct;
                        let always_above = guard.config.absorb_always_above_tokens;
                        let auto_nudge = guard.config.auto_nudge_enabled;
                        // 状态按会话存（见 `crate::session`）：报出来才能解释
                        // 「为什么块账本是空的」——新会话本来就应该是空的。
                        let session = guard.session_key().unwrap_or("unknown").to_string();
                        (
                            report,
                            ranges,
                            absorbed,
                            source,
                            gate,
                            auto_nudge,
                            always_above,
                            observed,
                            empty_ranges,
                            session,
                        )
                    };
                    // 超过限额且无内容可压时如实告知：compress 在 phi 上改不了宿主
                    // 请求体，观察视图又远小于真实上下文。
                    let note = if report.0.context_usage >= 1.0 && report.8 {
                        "\n⚠️ 超过限额且本扩展视图内无可压缩内容。phi 上 compress 改不了宿主请求体——视图只覆盖用户输入 + 工具结果（看不到助手正文 / 推理 / 被压缩历史），可能远小于真实上下文。宿主只在**没有工具调用**的回合末尾才跑原生压缩（engine.go：`len(msg.ToolCalls)==0`），因此长时间的工具调用循环会让上下文单调上涨，无论 context_window 设多少。真正能减小的通道：absorb（此水位已最大力度），以及**结束本回合**（纯文本回复）以触发宿主压缩。"
                    } else {
                        ""
                    };
                    ctx.notify(
                        "info",
                        &format!(
                            "ACP: {:.1}% ({}/{} tokens, {}) · session {} · observed view ~{} tok · absorb 门槛 {:.0}% / 强制吸收 ≥{} tok (已回收 {}) · 自动提醒 {} · active blocks {} · total {} · reclaimed {} tokens\n{}{}",
                            report.0.context_usage * 100.0,
                            report.0.token_count,
                            report.0.model_context_limit,
                            report.3,
                            report.9,
                            report.7,
                            report.4 * 100.0,
                            report.6,
                            report.2,
                            if report.5 { "on" } else { "off" },
                            report.0.active_blocks,
                            report.0.total_blocks,
                            report.0.tokens_compressed,
                            report.1,
                            note
                        ),
                    );
                }

                "compress" => {
                    // phi 没有请求体重写钩子，compress 只写块账本、不会减少上游 token；
                    // 主动向模型提交「立即压缩」指令是净亏损路径（多花 token 写压不掉的摘要）。
                    // 因此这里只做信息展示，真正回收上下文交给 absorb。
                    let (count, ranges, absorbed) = {
                        let mut guard = shared.borrow_mut();
                        if !guard.config.enabled {
                            ctx.notify("warning", "phi-acp 已禁用，先执行 /acp enable");
                            return Ok(());
                        }
                        let outcome = guard.process();
                        let count = outcome
                            .nudge
                            .as_ref()
                            .map(|n| n.compressible_ranges.len())
                            .unwrap_or(0);
                        let ranges = outcome
                            .nudge
                            .as_ref()
                            .map(|n| {
                                crate::nudge::format_ranges(
                                    &n.compressible_ranges,
                                    &n.protected_ranges,
                                )
                            })
                            .unwrap_or_default();
                        let absorbed = guard.state.stats.absorbed_tokens;
                        (count, ranges, absorbed)
                    };
                    if count == 0 {
                        ctx.notify(
                            "info",
                            &format!(
                                "当前无可压缩范围（保护区外没有可压缩消息）。phi 上 compress 只写块账本、不减少上游 token；真正回收上下文的是 absorb（已回收 {absorbed} tokens，用 /acp absorb 看明细）。"
                            ),
                        );
                    } else {
                        ctx.notify(
                            "info",
                            &format!(
                                "当前有 {count} 个可压缩范围（见下），但 phi 无请求体重写钩子，compress 只写块账本、不减少上游 token；真正回收上下文的是 absorb（已回收 {absorbed} tokens，用 /acp absorb 看明细）。\n{ranges}"
                            ),
                        );
                    }
                }
                "absorb" => {
                    let (
                        enabled,
                        gate,
                        always_above,
                        min_tokens,
                        keep_prefix,
                        keep_suffix,
                        absorbed,
                        entries,
                        by_tool,
                    ) = {
                        let guard = shared.borrow();
                        let cfg = &guard.config;
                        // 按工具聚合回收量：定位「谁在制造上下文」。
                        let mut by_tool: Vec<(String, u64, usize)> = Vec::new();
                        for rec in &guard.state.absorbed_outputs {
                            match by_tool.iter_mut().find(|(name, _, _)| name == &rec.tool_name) {
                                Some((_, tokens, count)) => {
                                    *tokens += rec.tokens;
                                    *count += 1;
                                }
                                None => by_tool.push((rec.tool_name.clone(), rec.tokens, 1)),
                            }
                        }
                        by_tool.sort_by_key(|(_, tokens, _)| std::cmp::Reverse(*tokens));
                        (
                            cfg.absorb_enabled,
                            cfg.absorb_context_threshold_pct,
                            cfg.absorb_always_above_tokens,
                            cfg.absorb_min_tool_tokens,
                            cfg.absorb_keep_prefix_chars,
                            cfg.absorb_keep_suffix_chars,
                            guard.state.stats.absorbed_tokens,
                            guard.state.absorbed_outputs.len(),
                            by_tool,
                        )
                    };
                    if !enabled {
                        ctx.notify(
                            "warning",
                            "absorb 已关闭——这是 phi 上唯一能真正减少上游 token 的通道，建议 /acp config absorb true",
                        );
                        return Ok(());
                    }
                    let tools = if by_tool.is_empty() {
                        "（暂无）".to_string()
                    } else {
                        by_tool
                            .iter()
                            .map(|(name, tokens, count)| format!("{name} {tokens} tok ×{count}"))
                            .collect::<Vec<_>>()
                            .join(" · ")
                    };
                    ctx.notify(
                        "info",
                        &format!(
                            "📥 absorb 诊断（phi 上唯一真正减少上游 token 的通道）\n\
                             开关：on · 门槛：使用率 ≥{:.0}% 或单条 ≥{} tok · 最小 {} tok · 保留 {} + {} 字符\n\
                             已回收：{} tokens / {} 条（句柄上限 {}）\n\
                             按工具：{}\n\
                             取回原文：acp_decompress <句柄>（如 a3）",
                            gate * 100.0,
                            always_above,
                            min_tokens,
                            keep_prefix,
                            keep_suffix,
                            absorbed,
                            entries,
                            crate::absorb_store::MAX_ENTRIES,
                            tools,
                        ),
                    );
                }
                "enable" | "disable" => {
                    let enabled = subcommand == "enable";
                    {
                        let mut guard = shared.borrow_mut();
                        guard.config.enabled = enabled;
                    }
                    match persist(&shared) {
                        Some(err) => ctx.notify("error", &err),
                        None => ctx.notify(
                            "info",
                            if enabled {
                                "✅ phi-acp 已启用"
                            } else {
                                "⏸️ phi-acp 已禁用"
                            },
                        ),
                    }
                }
                "config" => {
                    if tokens.len() < 3 {
                        ctx.notify("warning", "用法：/acp config <key> <value>");
                        return Ok(());
                    }
                    let key = tokens[1];
                    let value = tokens[2];
                    {
                        let mut guard = shared.borrow_mut();
                        match key {
                            "context-limit" => match value.parse::<u64>() {
                                Ok(v) => guard.config.model_context_limit = v,
                                Err(_) => {
                                    ctx.notify("error", "context-limit 需要整数");
                                    return Ok(());
                                }
                            },
                            "render-tags" => guard.config.render_tags = value.to_string(),
                            "min-compress" => match value.parse::<usize>() {
                                Ok(v) => guard.config.min_compress_range = v,
                                Err(_) => {
                                    ctx.notify("error", "min-compress 需要整数");
                                    return Ok(());
                                }
                            },
                            "max-summary-ratio" => match value.parse::<f64>() {
                                Ok(v) => guard.config.max_summary_ratio = v,
                                Err(_) => {
                                    ctx.notify("error", "max-summary-ratio 需要小数（如 0.5，0 = 关闭）");
                                    return Ok(());
                                }
                            },
                            "host-tokens" => match value.parse::<bool>() {
                                Ok(v) => guard.config.use_host_tokens = v,
                                Err(_) => {
                                    ctx.notify("error", "host-tokens 需要 true/false");
                                    return Ok(());
                                }
                            },
                            "absorb" => match value.parse::<bool>() {
                                Ok(v) => guard.config.absorb_enabled = v,
                                Err(_) => {
                                    ctx.notify("error", "absorb 需要 true/false");
                                    return Ok(());
                                }
                            },
                            "absorb-min-tokens" => match value.parse::<u64>() {
                                Ok(v) => guard.config.absorb_min_tool_tokens = v,
                                Err(_) => {
                                    ctx.notify("error", "absorb-min-tokens 需要整数");
                                    return Ok(());
                                }
                            },
                            "absorb-keep-prefix" => match value.parse::<usize>() {
                                Ok(v) => guard.config.absorb_keep_prefix_chars = v,
                                Err(_) => {
                                    ctx.notify("error", "absorb-keep-prefix 需要整数");
                                    return Ok(());
                                }
                            },
                            "absorb-keep-suffix" => match value.parse::<usize>() {
                                Ok(v) => guard.config.absorb_keep_suffix_chars = v,
                                Err(_) => {
                                    ctx.notify("error", "absorb-keep-suffix 需要整数");
                                    return Ok(());
                                }
                            },
                            "absorb-threshold-pct" => match value.parse::<f64>() {
                                Ok(v) => guard.config.absorb_context_threshold_pct = v,
                                Err(_) => {
                                    ctx.notify("error", "absorb-threshold-pct 需要小数（如 0.5）");
                                    return Ok(());
                                }
                            },
                            "absorb-always-above" => match value.parse::<u64>() {
                                Ok(v) => guard.config.absorb_always_above_tokens = v,
                                Err(_) => {
                                    ctx.notify("error", "absorb-always-above 需要整数（0 = 关闭）");
                                    return Ok(());
                                }
                            },
                            "auto-nudge" => match value.parse::<bool>() {
                                Ok(v) => guard.config.auto_nudge_enabled = v,
                                Err(_) => {
                                    ctx.notify("error", "auto-nudge 需要 true/false");
                                    return Ok(());
                                }
                            },
                            "growth-tokens" => match value.parse::<u64>() {
                                Ok(v) => guard.config.nudge_growth_tokens = v,
                                Err(_) => {
                                    ctx.notify("error", "growth-tokens 需要整数");
                                    return Ok(());
                                }
                            },
                            "min-growth-tokens" => match value.parse::<u64>() {
                                Ok(v) => guard.config.nudge_min_growth_tokens = v,
                                Err(_) => {
                                    ctx.notify("error", "min-growth-tokens 需要整数");
                                    return Ok(());
                                }
                            },
                            "min-context-pct" => match value.parse::<f64>() {
                                Ok(v) => guard.config.nudge_min_context_pct = v,
                                Err(_) => {
                                    ctx.notify("error", "min-context-pct 需要小数（如 0.3）");
                                    return Ok(());
                                }
                            },
                            "max-context-pct" => match value.parse::<f64>() {
                                Ok(v) => guard.config.nudge_max_context_pct = v,
                                Err(_) => {
                                    ctx.notify("error", "max-context-pct 需要小数（如 0.65）");
                                    return Ok(());
                                }
                            },
                            "tier2-trigger" => match value.parse::<usize>() {
                                Ok(v) => guard.config.tier2_trigger = v,
                                Err(_) => {
                                    ctx.notify("error", "tier2-trigger 需要整数");
                                    return Ok(());
                                }
                            },
                            "tier3-trigger" => match value.parse::<usize>() {
                                Ok(v) => guard.config.tier3_trigger = v,
                                Err(_) => {
                                    ctx.notify("error", "tier3-trigger 需要整数");
                                    return Ok(());
                                }
                            },
                            other => {
                                ctx.notify("error", &format!("未知配置项：{other}"));
                                return Ok(());
                            }
                        }
                    }
                    match persist(&shared) {
                        Some(err) => ctx.notify("error", &err),
                        None => ctx.notify("info", &format!("已设置 {key} = {value}")),
                    }
                }
                "rules" => {
                    let lines = {
                        let guard = shared.borrow();
                        crate::rules::format_rules_list(&guard.state)
                    };
                    if lines.is_empty() {
                        ctx.notify("info", "没有持久规则。");
                    } else {
                        ctx.notify("info", &lines);
                    }
                }
                "reset" => {
                    let reply = ctx.confirm("重置 phi-acp？", "将清空当前会话的观测视图与压缩状态。");
                    if reply.ok {
                        shared.borrow_mut().reset_session();
                        ctx.notify("info", "✅ 已重置");
                    } else {
                        ctx.notify("info", "已取消");
                    }
                }
                _ => ctx.notify("info", USAGE),
            }
            Ok(())
        }),
    );
}

fn persist(shared: &Shared) -> Option<String> {
    let guard = shared.borrow();
    guard
        .persist_config()
        .err()
        .map(|e| format!("{EXTENSION_NAME} 保存配置失败：{e}"))
}

/// 供状态展示用的扩展名。
pub const NAME: &str = config::EXTENSION_NAME;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_should_mention_subcommands() {
        assert!(USAGE.contains("status"));
        assert!(USAGE.contains("compress"));
        assert!(USAGE.contains("absorb"));
        assert!(USAGE.contains("reset"));
        assert!(USAGE.contains("growth-tokens"));
        assert!(USAGE.contains("tier2-trigger"));
    }
}
