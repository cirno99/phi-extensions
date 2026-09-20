// commands.rs — 无人值守自动继续的斜杠命令。
//
// 对应 pi 版的 /sleep-on /sleep-off /sleep-set /sleep-max /sleep-status。
// pi 的 /sleep-stall 依赖看门狗（phi 无 abort/定时器能力），已移除。
// 另参照 pi-auto-approval 增加 /sleep-approval（自动审批开关、模式与名单）。
//
// 注意：phi 只有命令处理器能拿到 `Context`（可 notify / set_status），
// 拦截与订阅回调都拿不到，因此 footer 状态与 toast 全部收敛到命令里刷新。

use phi_ext::phi;

use crate::config::{self, ApprovalMode};
use crate::state::Shared;

/// 注册全部斜杠命令。
pub fn register(ext: &mut phi::Extension, shared: Shared) {
    register_on(ext, shared.clone());
    register_off(ext, shared.clone());
    register_set(ext, shared.clone());
    register_max(ext, shared.clone());
    register_status(ext, shared.clone());
    register_approval(ext, shared);
}

/// 刷新 footer 状态行。
fn refresh(ctx: &mut phi::Context<'_>, shared: &Shared) {
    let footer = shared.borrow().footer();
    ctx.set_status(&footer);
}

/// `/sleep-on [继续文本]` — 开启无人值守。
fn register_on(ext: &mut phi::Extension, shared: Shared) {
    ext.register_command(
        "sleep-on",
        phi::Command::new(
            "开启无人值守自动继续（用法：/sleep-on [继续文本]）",
            move |args, ctx| {
                let text = args.trim();
                let (footer, continue_text, max) = {
                    let mut state = shared.borrow_mut();
                    if !text.is_empty() {
                        state.continue_text = text.to_string();
                    }
                    state.enabled = true;
                    state.reset_budget();
                    state.touch();
                    (
                        state.footer(),
                        state.continue_text.clone(),
                        state.max,
                    )
                };
                ctx.set_status(&footer);
                ctx.notify(
                    "info",
                    &format!(
                        "\u{1F319} 自动继续已开启：每次 settled 后发送“{continue_text}”（上限 {max} 次）"
                    ),
                );
                Ok(())
            },
        ),
    );
}

/// `/sleep-off` — 关闭无人值守。
fn register_off(ext: &mut phi::Extension, shared: Shared) {
    ext.register_command(
        "sleep-off",
        phi::Command::new("关闭无人值守自动继续", move |_args, ctx| {
            {
                let mut state = shared.borrow_mut();
                state.enabled = false;
                state.pending_retry_reason = None;
            }
            refresh(ctx, &shared);
            ctx.notify("info", "\u{1F319} 自动继续已关闭");
            Ok(())
        }),
    );
}

/// `/sleep-set <文本>` — 设置继续文本并开启。
fn register_set(ext: &mut phi::Extension, shared: Shared) {
    ext.register_command(
        "sleep-set",
        phi::Command::new(
            "设置继续文本并开启（用法：/sleep-set <文本>）",
            move |args, ctx| {
                let text = args.trim();
                if text.is_empty() {
                    ctx.notify(
                        "warning",
                        "用法：/sleep-set <继续文本>，例如 /sleep-set 继续按计划推进",
                    );
                    return Ok(());
                }
                let (footer, continue_text) = {
                    let mut state = shared.borrow_mut();
                    state.continue_text = text.to_string();
                    state.enabled = true;
                    state.reset_budget();
                    state.touch();
                    (state.footer(), state.continue_text.clone())
                };
                ctx.set_status(&footer);
                ctx.notify(
                    "info",
                    &format!("\u{1F319} 自动继续已开启，继续文本：“{continue_text}”"),
                );
                Ok(())
            },
        )
        .needs_args(),
    );
}

/// `/sleep-max <次数>` — 设置迭代上限。
fn register_max(ext: &mut phi::Extension, shared: Shared) {
    ext.register_command(
        "sleep-max",
        phi::Command::new(
            "设置迭代上限（用法：/sleep-max <次数>）",
            move |args, ctx| {
                let parsed = args.trim().parse::<u32>().ok().filter(|n| *n > 0);
                let Some(max) = parsed else {
                    ctx.notify("warning", "用法：/sleep-max <正整数>，例如 /sleep-max 200");
                    return Ok(());
                };
                let (footer, count) = {
                    let mut state = shared.borrow_mut();
                    state.max = max;
                    (state.footer(), state.count)
                };
                ctx.set_status(&footer);
                ctx.notify("info", &format!("\u{1F319} 迭代上限已设为 {max}（当前 {count}/{max}）"));
                Ok(())
            },
        )
        .needs_args(),
    );
}

/// `/sleep-approval` 用法说明。
const APPROVAL_USAGE: &str = "\u{1F6E1}\u{FE0F} /sleep-approval 用法：\n\
  status            — 显示自动审批状态与名单\n\
  on | off          — 开关自动审批\n\
  safe              — 只放行可证明安全的动作（默认）\n\
  permissive        — 未命中 deny 的动作一律放行\n\
  allow <模式>      — 追加放行项（工具名或命令前缀，`*` 结尾为前缀匹配）\n\
  deny <模式>       — 追加阻止项\n\
  allowlist <命令>  — 追加「安全只读命令」白名单\n\
  approve           — 精确放行最近一次被阻止的动作\n\
  clear             — 清空会话内已批准的动作\n\
  reset             — 恢复默认配置（需确认）";

/// 保存自动审批配置并返回错误信息（成功时为 `None`）。
fn persist(shared: &Shared) -> Option<String> {
    let guard = shared.borrow();
    config::save(&guard.approval, &guard.config_path)
        .err()
        .map(|err| format!("配置保存失败：{err}"))
}

/// `/sleep-approval` — 自动审批开关、模式与名单。
fn register_approval(ext: &mut phi::Extension, shared: Shared) {
    ext.register_command(
        "sleep-approval",
        phi::Command::new(
            "配置无人值守自动审批（/sleep-approval status|on|off|safe|permissive|allow|deny|allowlist|clear|reset）",
            move |args, ctx| {
                let tokens: Vec<&str> = args.split_whitespace().collect();
                let subcommand = tokens.first().copied().unwrap_or("status");

                match subcommand {
                    "status" => {
                        let (summary, path, allow, deny, allowlist) = {
                            let guard = shared.borrow();
                            (
                                guard.approval_summary(),
                                guard.config_path.display().to_string(),
                                guard.approval.allow.clone(),
                                guard.approval.deny.clone(),
                                guard.approval.safe_command_allowlist.clone(),
                            )
                        };
                        let render = |items: &[String]| {
                            if items.is_empty() {
                                "（空）".to_string()
                            } else {
                                items.join(", ")
                            }
                        };
                        ctx.notify(
                            "info",
                            &format!(
                                "{summary}\n配置文件：{path}\n放行名单：{}\n阻止名单：{}\n安全命令白名单：{}",
                                render(&allow),
                                render(&deny),
                                render(&allowlist)
                            ),
                        );
                    }
                    "on" | "off" => {
                        let enabled = subcommand == "on";
                        {
                            let mut guard = shared.borrow_mut();
                            guard.approval.enabled = enabled;
                        }
                        match persist(&shared) {
                            Some(err) => ctx.notify("error", &err),
                            None => ctx.notify(
                                "info",
                                if enabled {
                                    "\u{1F6E1}\u{FE0F} 自动审批已开启"
                                } else {
                                    "\u{1F6E1}\u{FE0F} 自动审批已关闭"
                                },
                            ),
                        }
                    }
                    "safe" | "permissive" => {
                        let mode = if subcommand == "safe" {
                            ApprovalMode::Safe
                        } else {
                            ApprovalMode::Permissive
                        };
                        {
                            let mut guard = shared.borrow_mut();
                            guard.approval.mode = mode;
                        }
                        match persist(&shared) {
                            Some(err) => ctx.notify("error", &err),
                            None => ctx.notify(
                                "info",
                                &format!("\u{1F6E1}\u{FE0F} 自动审批模式已设为 {subcommand}"),
                            ),
                        }
                    }
                    "allow" | "deny" | "allowlist" => {
                        let Some(pattern) = tokens.get(1) else {
                            ctx.notify("warning", APPROVAL_USAGE);
                            return Ok(());
                        };
                        {
                            let mut guard = shared.borrow_mut();
                            let target = match subcommand {
                                "allow" => &mut guard.approval.allow,
                                "deny" => &mut guard.approval.deny,
                                _ => &mut guard.approval.safe_command_allowlist,
                            };
                            if !target.iter().any(|item| item == pattern) {
                                target.push((*pattern).to_string());
                            }
                        }
                        match persist(&shared) {
                            Some(err) => ctx.notify("error", &err),
                            None => ctx.notify(
                                "info",
                                &format!("\u{2705} 已追加 {subcommand} 项：{pattern}"),
                            ),
                        }
                    }
                    "approve" => {
                        let pending = shared.borrow().last_denied.clone();
                        match pending {
                            Some(subject) => {
                                shared
                                    .borrow_mut()
                                    .approval_store
                                    .approve_exact(&subject.action_hash);
                                ctx.notify(
                                    "info",
                                    &format!(
                                        "\u{2705} 已精确放行该动作（本次会话内有效）：{}",
                                        subject.action_summary
                                    ),
                                );
                            }
                            None => ctx.notify("info", "当前没有被自动审批阻止的动作。"),
                        }
                    }
                    "clear" => {
                        let mut guard = shared.borrow_mut();
                        guard.approval_store.clear();
                        guard.last_denied = None;
                        ctx.notify("info", "\u{1F9F9} 已清空会话内已批准的动作");
                    }
                    "reset" => {
                        if !ctx
                            .confirm(
                                "自动审批配置重置",
                                "把自动审批配置恢复为默认值（safe 模式、空名单）？",
                            )
                            .ok
                        {
                            ctx.notify("info", "未做任何修改（已取消）。");
                            return Ok(());
                        }
                        {
                            let mut guard = shared.borrow_mut();
                            guard.approval = config::ApprovalConfig::default();
                        }
                        match persist(&shared) {
                            Some(err) => ctx.notify("error", &err),
                            None => ctx.notify("info", "\u{2705} 自动审批配置已重置"),
                        }
                    }
                    _ => ctx.notify("warning", APPROVAL_USAGE),
                }
                Ok(())
            },
        ),
    );
}

/// `/sleep-status` — 查看当前状态。
fn register_status(ext: &mut phi::Extension, shared: Shared) {
    ext.register_command(
        "sleep-status",
        phi::Command::new("查看无人值守自动继续状态", move |_args, ctx| {
            let state = shared.borrow();
            let lines = [
                format!("开关：{}", if state.enabled { "开 ✅" } else { "关 ❌" }),
                format!("继续文本：“{}”", state.continue_text),
                format!("进度：{}/{}", state.count, state.max),
                format!("连续失败：{}", state.consecutive_errors),
                format!(
                    "待重试：{}",
                    state.pending_retry_reason.as_deref().unwrap_or("无")
                ),
                state.approval_summary(),
            ];
            let footer = state.footer();
            drop(state);
            ctx.set_status(&footer);
            ctx.notify("info", &format!("\u{1F319} 睡眠继续状态\n{}", lines.join("\n")));
            Ok(())
        }),
    );
}