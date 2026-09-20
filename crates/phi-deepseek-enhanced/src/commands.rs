// commands.rs — `/deepseek` 斜杠命令。
//
// phi 只有命令处理器能拿到 `Context`（可 notify / set_status），因此所有
// 状态展示与配置修改都收敛到这一个命令里。

use phi_ext::phi;

use crate::config;
use crate::state::Shared;

/// 用法说明。
const USAGE: &str = "🐋 /deepseek 用法：\n\
  status                 — 显示当前配置与运行统计\n\
  on | off               — 总开关\n\
  anchor on | off        — 是否注入 We-need 推理锚点\n\
  minimal on | off       — Eternal Minimal 运行时守卫（阻止非核心工具直呼）\n\
  transport on | off     — minimal 下是否允许 read/write 直呼\n\
  strip on | off         — 是否剥离用户消息里的 Today/cwd 系统提醒\n\
  reset                  — 恢复默认配置（需确认）\n\
  help                   — 显示本说明";

/// 保存配置并返回错误信息（成功时为 `None`）。
fn persist(shared: &Shared) -> Option<String> {
    let guard = shared.borrow();
    config::save(&guard.config, &guard.config_path)
        .err()
        .map(|err| format!("配置保存失败：{err}"))
}

/// 把 `on`/`off` 解析为布尔值。
fn parse_toggle(value: &str) -> Option<bool> {
    match value {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    }
}

/// 注册 `/deepseek` 命令。
pub fn register(ext: &mut phi::Extension, shared: Shared) {
    ext.register_command(
        "deepseek",
        phi::Command::new(
            "DeepSeek Enhanced 配置（/deepseek status|on|off|anchor|minimal|transport|strip|reset|help）",
            move |args, ctx| {
                let tokens: Vec<&str> = args.split_whitespace().collect();
                let subcommand = tokens.first().copied().unwrap_or("status");
                match subcommand {
                    "status" => {
                        let (summary, path, footer) = {
                            let guard = shared.borrow();
                            (
                                status_summary(&guard),
                                guard.config_path.display().to_string(),
                                guard.footer(),
                            )
                        };
                        ctx.set_status(&footer);
                        ctx.notify("info", &format!("{summary}\n配置文件：{path}"));
                    }
                    "on" | "off" => {
                        let enabled = subcommand == "on";
                        shared.borrow_mut().config.enabled = enabled;
                        match persist(&shared) {
                            Some(err) => ctx.notify("error", &err),
                            None => ctx.notify(
                                "info",
                                if enabled {
                                    "🐋 DeepSeek Enhanced 已开启"
                                } else {
                                    "🐋 DeepSeek Enhanced 已关闭"
                                },
                            ),
                        }
                    }
                    "anchor" | "minimal" | "transport" | "strip" => {
                        let Some(value) = tokens.get(1).and_then(|raw| parse_toggle(raw)) else {
                            ctx.notify("warning", USAGE);
                            return Ok(());
                        };
                        {
                            let mut guard = shared.borrow_mut();
                            match subcommand {
                                "anchor" => guard.config.inject_anchor = value,
                                "minimal" => guard.config.minimal = value,
                                "transport" => guard.config.transport = value,
                                _ => guard.config.strip_date_cwd_reminder = value,
                            }
                        }
                        match persist(&shared) {
                            Some(err) => ctx.notify("error", &err),
                            None => ctx.notify(
                                "info",
                                &format!("✅ {subcommand} 已设为 {}", if value { "on" } else { "off" }),
                            ),
                        }
                    }
                    "reset" => {
                        if !ctx
                            .confirm(
                                "DeepSeek Enhanced 配置重置",
                                "把配置恢复为默认值（开启、注入锚点、minimal 关闭）？",
                            )
                            .ok
                        {
                            ctx.notify("info", "未做任何修改（已取消）。");
                            return Ok(());
                        }
                        shared.borrow_mut().config = config::Config::default();
                        match persist(&shared) {
                            Some(err) => ctx.notify("error", &err),
                            None => ctx.notify("info", "✅ 配置已重置"),
                        }
                    }
                    "help" => ctx.notify("info", USAGE),
                    _ => ctx.notify("warning", USAGE),
                }
                Ok(())
            },
        ),
    );
}

/// 拼装 `status` 摘要。
fn status_summary(guard: &crate::state::State) -> String {
    let config = &guard.config;
    let toggle = |value: bool| if value { "开 ✅" } else { "关 ❌" };
    let mut lines = vec![
        format!("总开关：{}", toggle(config.enabled)),
        format!("注入锚点：{}", toggle(config.inject_anchor)),
        format!("压缩后重注入：{}", toggle(config.reanchor_after_compact)),
        format!("剥离 Today/cwd 提醒：{}", toggle(config.strip_date_cwd_reminder)),
        format!("Eternal Minimal 守卫：{}", toggle(config.minimal)),
        format!("传输工具直呼：{}", toggle(config.transport)),
        format!("核心工具：{}", config.core_tools.join(", ")),
        format!("传输工具：{}", config.transport_tools.join(", ")),
        format!("本会话压缩次数：{}", guard.compactions),
        format!("被阻止的调用：{}", guard.blocked_calls),
    ];
    if let Some(warning) = &guard.warning {
        lines.push(format!("⚠️ {warning}"));
    }
    format!("🐋 DeepSeek Enhanced 状态\n{}", lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_toggle_should_only_accept_on_off() {
        assert_eq!(parse_toggle("on"), Some(true));
        assert_eq!(parse_toggle("off"), Some(false));
        assert_eq!(parse_toggle("yes"), None);
    }
}