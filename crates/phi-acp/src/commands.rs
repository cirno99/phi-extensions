//! `/acp` 斜杠命令 —— 对应 billion-context 的 `/acp` 面板。

use phi_ext::phi;

use crate::config::{self, EXTENSION_NAME};
use crate::prompts::Prompts;
use crate::runtime::Shared;

/// 用法说明。
const USAGE: &str = "📋 /acp 子命令：\n\
  status                       — 上下文使用率、块统计与可压缩范围（默认）\n\
  compress                     — 立即压缩当前可压缩范围（交由模型调用 compress 工具）\n\
  enable | disable             — 开关扩展\n\
  config <key> <value>         — 设置配置（context-limit / render-tags / min-compress / host-tokens / growth-tokens / min-growth-tokens / min-context-pct / max-context-pct / tier2-trigger / tier3-trigger / absorb / absorb-min-tokens / absorb-keep-prefix / absorb-keep-suffix / absorb-threshold-pct）\n\
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
                        let report = crate::compress::status(
                            &guard.state,
                            guard.effective_token_count(),
                            &guard.config.to_kernel_config(),
                        );
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
                        (report, ranges, absorbed)
                    };
                    ctx.notify(
                        "info",
                        &format!(
                            "ACP: {:.1}% ({}/{} tokens) · active blocks {} · total {} · reclaimed {} tokens\n{}",
                            report.0.context_usage * 100.0,
                            report.0.token_count,
                            report.0.model_context_limit,
                            report.0.active_blocks,
                            report.0.total_blocks,
                            report.0.tokens_compressed,
                            report.1
                        ),
                    );
                }

                "compress" => {
                    let (count, text) = {
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
                        let text = outcome.nudge.as_ref().map(|n| {
                            crate::nudge::render_manual_compress_text(n, &Prompts::default())
                        });
                        (count, text)
                    };
                    match text {
                        Some(text) if count > 0 => {
                            ctx.notify(
                                "info",
                                &format!("已请求压缩 {count} 个范围，模型将调用 compress 工具。"),
                            );
                            ctx.submit(&text);
                        }
                        _ => ctx.notify("info", "当前无可压缩范围。"),
                    }
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
                        crate::tools::format_rules_for_prompt(&guard.state)
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
        assert!(USAGE.contains("reset"));
        assert!(USAGE.contains("growth-tokens"));
        assert!(USAGE.contains("tier2-trigger"));
    }
}
