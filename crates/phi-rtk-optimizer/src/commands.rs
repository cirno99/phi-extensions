// commands.rs — `/rtk` 斜杠命令。
//
// 由 pi 版 pi-rtk-optimizer 的 src/command-register.ts + src/config-modal.ts
// 的命令行部分移植。
//
// 与 pi 版的差异：pi 用交互式弹窗（TUI 组件）改配置，phi 的命令处理器只有
// `notify` / `confirm` / `submit` 三种交互面，因此这里改为「子命令 + 文本输出」，
// 只有 `reset` 用一次确认框。

use phi_ext::phi;

use crate::config::{self, RtkIntegrationConfig};
use crate::runtime::Shared;

/// `/rtk` 用法说明。
const USAGE: &str = "\u{1F4CB} /rtk 子命令：\n\
  show        — 显示当前配置与运行期状态\n\
  path        — 显示配置文件路径\n\
  verify      — 重新探测 rtk 可执行文件\n\
  stats       — 显示输出压缩收益统计\n\
  clear-stats — 清空输出压缩收益统计\n\
  reset       — 把配置重置为默认值（需确认）\n\
  help        — 显示本说明";

/// 注册 `/rtk` 命令。
pub fn register(ext: &mut phi::Extension, shared: Shared) {
    ext.register_command(
        "rtk",
        phi::Command::new("配置 RTK 命令重写与输出压缩集成", move |args, ctx| {
            let mut guard = shared.borrow_mut();

            // 回调阶段攒下的告警在这里统一展示（拦截回调拿不到 Context）。
            let notices = guard.take_notices();
            for notice in &notices {
                ctx.notify("warning", notice);
            }

            let tokens: Vec<&str> = args.split_whitespace().collect();
            let subcommand = tokens.first().copied().unwrap_or("help");

            match subcommand {
                "show" => {
                    let summary = render_show(&guard);
                    ctx.notify("info", &summary);
                }
                "path" => {
                    ctx.notify("info", &format!("RTK config: {}", guard.config_path.display()));
                }
                "verify" => {
                    guard.refresh_status();
                    let summary = render_status(&guard);
                    ctx.notify("info", &format!("RTK 可用性探测结果：\n{summary}"));
                }
                "stats" => {
                    if guard.metrics.is_empty() {
                        ctx.notify("info", "RTK output compaction metrics: no data yet.");
                    } else {
                        let summary = guard.metrics.summary();
                        ctx.notify("info", &summary);
                    }
                }
                "clear-stats" => {
                    let before = guard.metrics.len();
                    guard.metrics.clear();
                    ctx.notify("info", &format!("\u{1F9F9} 已清空 {before} 条压缩统计"));
                }
                "reset" => {
                    if !ctx.confirm(
                        "RTK 配置重置",
                        &format!(
                            "把 {} 重置为默认配置？\n\n当前值将被覆盖（默认：启用、rewrite 模式、开启输出压缩）。",
                            guard.config_path.display()
                        ),
                    )
                    .ok
                    {
                        ctx.notify("info", "未做任何修改（已取消）。");
                        return Ok(());
                    }
                    let defaults = RtkIntegrationConfig::default();
                    match config::save(&defaults, &guard.config_path) {
                        Ok(()) => {
                            guard.config = defaults;
                            guard.config_warning = None;
                            ctx.notify("info", "\u{2705} RTK 配置已重置为默认值");
                        }
                        Err(err) => {
                            ctx.notify("error", &format!("\u{274C} 重置失败：{err}"));
                        }
                    }
                }
                "help" => {
                    ctx.notify("info", USAGE);
                }
                other => {
                    ctx.notify("warning", &format!("未知子命令 `{other}`。\n\n{USAGE}"));
                }
            }

            Ok(())
        }),
    );
}

/// 渲染 `show` 的输出。
fn render_show(guard: &crate::runtime::Runtime) -> String {
    let config_json = phi_ext_common::json::to_string_pretty(&guard.config)
        .unwrap_or_else(|err| format!("<配置序列化失败：{err}>"));
    let metrics = guard.metrics.summary();
    format!(
        "\u{2699}\u{FE0F} RTK 配置（{}）\n{}\n\n{}\n\n{}",
        guard.config_path.display(),
        config_json,
        render_status(guard),
        metrics
    )
}

/// 渲染运行期状态。
fn render_status(guard: &crate::runtime::Runtime) -> String {
    let status = &guard.status;
    let mut lines = vec![format!(
        "rtk 可用：{}",
        if status.rtk_available { "是 ✅" } else { "否 ❌" }
    )];
    if let Some(executable) = &status.executable {
        lines.push(format!("解析器：{}", executable.resolver));
        lines.push(format!("调用命令：{}", executable.command));
        if let Some(path) = &executable.resolved_path {
            lines.push(format!("可执行文件：{path}"));
        }
        if let Some(warning) = &executable.warning {
            lines.push(format!("解析告警：{warning}"));
        }
    }
    if let Some(error) = &status.last_error {
        lines.push(format!("最近错误：{error}"));
    }
    if let Some(checked) = status.last_checked_at {
        lines.push(format!("上次探测：{checked}（Unix 毫秒）"));
    }
    if let Some(warning) = &guard.config_warning {
        lines.push(format!("配置告警：{warning}"));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_should_list_all_subcommands() {
        for subcommand in [
            "show",
            "path",
            "verify",
            "stats",
            "clear-stats",
            "reset",
            "help",
        ] {
            assert!(USAGE.contains(subcommand), "用法缺少 {subcommand}");
        }
    }
}