// commands.rs — `/cache-optimizer` 斜杠命令。
//
// 参照 pi 版的同名命令，去掉依赖宿主能力的分支（fix / rollback /
// prompt-cache-key 实际注入等），保留配置、诊断与统计口径说明。

use phi_ext::phi;

use crate::config::{self, FooterMode, PromptCacheKeyMode};
use crate::doctor;
use crate::runtime::Shared;

/// 用法说明。
const USAGE: &str = "\u{1F4CB} /cache-optimizer 子命令：\n\
  status                              — 显示配置与能力诊断（默认）\n\
  doctor                              — 只显示能力诊断报告\n\
  enable | disable                    — 开关扩展\n\
  config footer-mode session|total|process — 设置 footer 统计口径\n\
  config prompt-cache-key auto|omit   — 设置 prompt_cache_key 策略\n\
  stats                               — 显示本会话缓存命中率与 token 速率\n\
  reset                               — 恢复默认配置（需确认）\n\
  help                                — 显示本说明";

/// 注册命令。
pub fn register(ext: &mut phi::Extension, shared: Shared) {
    ext.register_command(
        "cache-optimizer",
        phi::Command::new("配置与诊断 Phi 缓存行为", move |args, ctx| {
            let tokens: Vec<&str> = args.split_whitespace().collect();
            let subcommand = tokens.first().copied().unwrap_or("status");

            if let Some(warning) = shared.borrow().config_warning.clone() {
                ctx.notify("warning", &warning);
            }

            match subcommand {
                "status" => {
                    let (summary, report) = {
                        let guard = shared.borrow();
                        (guard.summary(), doctor::report(&guard.config))
                    };
                    ctx.notify("info", &format!("{summary}\n\n{report}"));
                }
                "doctor" => {
                    let report = doctor::report(&shared.borrow().config);
                    ctx.notify("info", &report);
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
                                "\u{2705} cache-optimizer 已启用"
                            } else {
                                "\u{23F8}\u{FE0F} cache-optimizer 已禁用"
                            },
                        ),
                    }
                }
                "config" => {
                    let key = tokens.get(1).copied();
                    let value = tokens.get(2).copied();
                    match (key, value) {
                        (Some("footer-mode"), Some(mode)) => {
                            let Some(parsed) = parse_footer_mode(mode) else {
                                ctx.notify(
                                    "warning",
                                    "用法：/cache-optimizer config footer-mode session|total|process",
                                );
                                return Ok(());
                            };
                            shared.borrow_mut().config.footer_mode = parsed;
                            match persist(&shared) {
                                Some(err) => ctx.notify("error", &err),
                                None => ctx.notify("info", &format!("\u{2705} footer 口径已设为 {mode}")),
                            }
                        }
                        (Some("prompt-cache-key"), Some(mode)) => {
                            let Some(parsed) = parse_prompt_cache_key(mode) else {
                                ctx.notify(
                                    "warning",
                                    "用法：/cache-optimizer config prompt-cache-key auto|omit",
                                );
                                return Ok(());
                            };
                            shared.borrow_mut().config.prompt_cache_key = parsed;
                            match persist(&shared) {
                                Some(err) => ctx.notify("error", &err),
                                None => ctx.notify(
                                    "info",
                                    &format!(
                                        "\u{2705} prompt_cache_key 策略已设为 {mode}\n\
                                         注意：phi 无请求体钩子，该设置目前只作为记录，不会实际改写请求。"
                                    ),
                                ),
                            }
                        }
                        _ => ctx.notify("warning", USAGE),
                    }
                }
                "stats" => {
                    let cwd = ctx.cwd().to_string();
                    let session_id = ctx.session_id().to_string();
                    let report = crate::usage::load(&cwd, &session_id);
                    ctx.notify("info", &crate::usage::render(report.as_ref()));
                    // 同步到宿主底部状态行（token 状态栏旁的扩展状态区）。
                    ctx.set_status(&crate::usage::footer_status(report.as_ref()));
                }
                "reset" => {
                    if !ctx
                        .confirm(
                            "Cache Optimizer 配置重置",
                            "把配置恢复为默认值（启用、session 口径、auto 策略）？",
                        )
                        .ok
                    {
                        ctx.notify("info", "未做任何修改（已取消）。");
                        return Ok(());
                    }
                    {
                        let mut guard = shared.borrow_mut();
                        guard.config = config::CacheOptimizerConfig::default();
                        guard.config_warning = None;
                    }
                    match persist(&shared) {
                        Some(err) => ctx.notify("error", &err),
                        None => ctx.notify("info", "\u{2705} 配置已重置为默认值"),
                    }
                }
                "help" => ctx.notify("info", USAGE),
                other => ctx.notify("warning", &format!("未知子命令 `{other}`。\n\n{USAGE}")),
            }

            Ok(())
        }),
    );
}

/// 保存配置并返回错误信息。
fn persist(shared: &Shared) -> Option<String> {
    shared.borrow().persist().err()
}

/// 解析 footer 口径。
fn parse_footer_mode(value: &str) -> Option<FooterMode> {
    match value.to_ascii_lowercase().as_str() {
        "session" => Some(FooterMode::Session),
        "total" => Some(FooterMode::Total),
        "process" => Some(FooterMode::Process),
        _ => None,
    }
}

/// 解析 `prompt_cache_key` 策略。
fn parse_prompt_cache_key(value: &str) -> Option<PromptCacheKeyMode> {
    match value.to_ascii_lowercase().as_str() {
        "auto" => Some(PromptCacheKeyMode::Auto),
        "omit" => Some(PromptCacheKeyMode::Omit),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_should_list_all_subcommands() {
        for subcommand in [
            "status",
            "doctor",
            "enable",
            "disable",
            "config",
            "stats",
            "reset",
            "help",
        ] {
            assert!(USAGE.contains(subcommand), "用法缺少 {subcommand}");
        }
    }

    #[test]
    fn parse_footer_mode_should_accept_documented_values() {
        assert_eq!(parse_footer_mode("session"), Some(FooterMode::Session));
        assert_eq!(parse_footer_mode("TOTAL"), Some(FooterMode::Total));
        assert_eq!(parse_footer_mode("process"), Some(FooterMode::Process));
        assert_eq!(parse_footer_mode("nope"), None);
    }

    #[test]
    fn parse_prompt_cache_key_should_accept_documented_values() {
        assert_eq!(parse_prompt_cache_key("auto"), Some(PromptCacheKeyMode::Auto));
        assert_eq!(parse_prompt_cache_key("OMIT"), Some(PromptCacheKeyMode::Omit));
        assert_eq!(parse_prompt_cache_key("nope"), None);
    }
}