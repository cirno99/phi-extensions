// commands.rs — 斜杠命令实现。

use phi_ext::phi;

use crate::runtime::Shared;
use crate::tools::status_report;

/// footer 状态栏的一行摘要。
fn footer_line(state: &crate::types::SessionState, enabled: bool) -> String {
    if !enabled {
        return String::new();
    }
    let current = state.state.unwrap_or(crate::types::State::Start);
    let max = crate::state_machine::max_state_turns(current, state.difficulty);
    format!(
        "\u{1F300} {} {}/{}",
        current.label(),
        state.state_turn_count,
        max
    )
}

/// 注册斜杠命令。
pub fn register(ext: &mut phi::Extension, rt: Shared) {
    register_toggle(ext, rt.clone());

    ext.register_command(
        "asymptotic-status",
        phi::Command::new("显示渐近式思考状态机快照", move |_args, ctx| {
            let (report, footer) = {
                let guard = rt.borrow();
                let state = guard.store.load();
                let enabled = guard.store.is_enabled();
                (
                    status_report(&state, &guard.session_id, enabled),
                    footer_line(&state, enabled),
                )
            };
            ctx.set_status(&footer);
            ctx.notify("info", "渐近式思考状态快照已写入会话");
            ctx.send_user_message(&report);
            Ok(())
        }),
    );
}

/// 解析开关参数：空串表示翻转，其余为显式开关值。
fn parse_toggle(arg: &str, current: bool) -> Result<bool, String> {
    match arg.trim().to_ascii_lowercase().as_str() {
        "" => Ok(!current),
        "on" | "enable" | "true" | "1" => Ok(true),
        "off" | "disable" | "false" | "0" => Ok(false),
        other => Err(format!(
            "未知参数 `{other}`；用法：/asymptotic-toggle [on|off]"
        )),
    }
}

/// 注册启用/禁用命令。
fn register_toggle(ext: &mut phi::Extension, rt: Shared) {
    ext.register_command(
        "asymptotic-toggle",
        phi::Command::new(
            "启用或禁用渐近式思考框架（/asymptotic-toggle [on|off]）",
            move |args, ctx| {
                let (footer, target) = {
                    let guard = rt.borrow_mut();
                    let current = guard.store.is_enabled();
                    let target = parse_toggle(args, current)?;
                    guard
                        .store
                        .set_enabled(target)
                        .map_err(|e| format!("开关持久化失败：{e}"))?;
                    let state = guard.store.load();
                    (footer_line(&state, target), target)
                };

                ctx.set_status(&footer);
                ctx.notify(
                    "info",
                    if target {
                        "渐近式思考已启用"
                    } else {
                        "渐近式思考已禁用"
                    },
                );
                Ok(())
            },
        ),
    );
}
