// main.rs — phi-deepseek-enhanced 扩展入口。
//
// 由 Oh My Pi 版 deepseek-enhanced.ts 移植。原扩展为 DeepSeek 模型提供
// 永久的最小工具呈现（bash + str_replace_editor），其余高阶工具全部收敛到
// OMP 原生的 `xd://` 网关，并注入 "We need to ..." 推理风格锚点。
//
// pi → phi 的钩子映射：
// - 注册 `str_replace_editor` 工具            → `register_tool`，等价。
// - `session_start` 里按模型启用最小呈现       → `subscribe(SessionStart)`。
// - `before_agent_start` 注入锚点 / 最小系统提示 → `on_before_agent_start`
//   的 `system_prompt_append`（宿主会把它并进当前用户消息）。
// - `context` 过滤自动上下文 + 隐藏锚点         → 无对应（拿不到消息历史）。
// - `tool_call` 阻止高阶工具直呼                → `on_tool_call` 返回 `Block`，等价。
//
// 受 phi 宿主能力限制而无法移植的部分：
// - **拿不到模型信息**：PXB 子进程协议不向扩展推送 model，因此无法像 pi 那样
//   只对 DeepSeek 生效；改为全局开关（`enabled`）。
// - **无法收缩工具清单**：无 `GetAllTools` / `SetActiveTools` 的 RPC，
//   模型始终能看到全部工具；Eternal Minimal 只能退化为「运行时阻止直呼」。
// - **无法落地 `xd://` 网关**：扩展不能调用别的工具，read/write 的 xd:// 语义
//   是 OMP 宿主内建能力，phi 没有。因此 minimal 模式默认关闭。
// - **拿不到 provider 请求体**：无 `before_provider_request`，无法过滤 tools、
//   剥离消息、强制 thinking/256k token 上限。
// - **拿不到 assistant 推理文本**：无法做 CoT 回归检测与再注入。
// - **拿不到消息历史**：`context` 过滤器不可实现。

mod anchor;
mod commands;
mod config;
mod editor;
mod state;

use phi_ext::{phi, pxb};

use state::Shared;

fn main() -> Result<(), phi::Error> {
    let mut ext = phi::Extension::new(config::EXTENSION_NAME, env!("CARGO_PKG_VERSION"));
    let shared = state::shared();

    // `str_replace_editor` 与 pi 版一致，始终注册。
    editor::register(&mut ext);
    commands::register(&mut ext, shared.clone());

    register_before_agent_start(&mut ext, shared.clone());
    register_tool_call(&mut ext, shared.clone());
    register_events(&mut ext, shared);

    ext.run()
}

/// 剥离用户消息开头的「Today / current working directory」系统提醒。
///
/// 移植自 pi 版的 `stripDateCwdText`：只有当内容以
/// `<system-reminder>\nToday:` 开头、且提醒块内包含
/// `current working directory:` 时才剥离，用于维持前缀缓存稳定。
fn strip_date_cwd_text(content: &str) -> Option<String> {
    const PREFIX: &str = "<system-reminder>\nToday:";
    const CLOSE: &str = "</system-reminder>";
    if !content.starts_with(PREFIX) {
        return None;
    }
    let end = content.find(CLOSE)?;
    if !content[..end].contains("current working directory:") {
        return None;
    }
    let remainder = &content[end + CLOSE.len()..];
    Some(remainder.strip_prefix("\n\n").unwrap_or(remainder).to_string())
}

/// 注入锚点 + 剥离日期/cwd 提醒。
fn register_before_agent_start(ext: &mut phi::Extension, shared: Shared) {
    ext.on_before_agent_start(move |ev| {
        let mut guard = shared.borrow_mut();
        if !guard.config.enabled {
            return None;
        }
        let mut result = phi::BeforeAgentStartResult::default();
        let mut changed = false;

        if guard.config.strip_date_cwd_reminder {
            if let Some(cleaned) = strip_date_cwd_text(&ev.prompt) {
                result.prompt = Some(cleaned);
                changed = true;
            }
        }

        if guard.config.inject_anchor
            && !anchor::contains_anchor(&ev.prompt)
            && guard.take_anchor()
        {
            result.system_prompt_append = guard.anchor_prompt();
            changed = true;
        }

        if changed {
            Some(result)
        } else {
            None
        }
    });
}

/// Eternal Minimal 运行时守卫：阻止对非核心工具的直接调用。
fn register_tool_call(ext: &mut phi::Extension, shared: Shared) {
    ext.on_tool_call(move |ev| {
        let mut guard = shared.borrow_mut();
        if !guard.config.enabled || !guard.config.minimal {
            return None;
        }
        let allowed = guard.config.allowed_direct_tools();
        if allowed.iter().any(|name| name == &ev.tool_name) {
            return None;
        }
        guard.blocked_calls += 1;
        Some(phi::ToolCallResult {
            block: true,
            reason: format!(
                "Eternal Minimal 阻止对 {} 的直接调用；本会话只允许直呼：{}。请改用允许的工具完成该操作。",
                ev.tool_name,
                allowed.join(", ")
            ),
            ..Default::default()
        })
    });
}

/// 生命周期事件：会话开始重置状态，压缩后重新武装锚点。
fn register_events(ext: &mut phi::Extension, shared: Shared) {
    {
        let shared = shared.clone();
        ext.subscribe(pxb::Event::SessionStart, move |_ev| {
            let mut guard = shared.borrow_mut();
            guard.reload_config();
            guard.reset_for_session();
        });
    }
    {
        let shared = shared.clone();
        ext.subscribe(pxb::Event::SessionCompact, move |_ev| {
            shared.borrow_mut().note_compaction();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_should_remove_today_cwd_reminder() {
        let content = "<system-reminder>\nToday: 2024\ncurrent working directory: /x\n</system-reminder>\n\nreal prompt";
        assert_eq!(strip_date_cwd_text(content).as_deref(), Some("real prompt"));
    }

    #[test]
    fn strip_should_keep_unrelated_reminders() {
        let content = "<system-reminder>\nToday: 2024\n</system-reminder>\n\nreal prompt";
        assert_eq!(strip_date_cwd_text(content), None);
    }

    #[test]
    fn strip_should_ignore_plain_prompts() {
        assert_eq!(strip_date_cwd_text("hello"), None);
    }
}