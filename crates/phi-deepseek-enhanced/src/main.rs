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
//   的 `system_prompt_append`（宿主会把它并进当前用户消息末尾）。该钩子
//   **每轮用户消息都会触发一次**，所以首轮注入完整锚点、后续轮次按间隔
//   追加极简风格提醒，避免锚点被历史淹没后失效。
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
        // 每轮重载配置：`/deepseek minimal on|off` 等中途变更必须立刻生效，
        // 否则守卫状态与锚点里描述的工具集会在本会话内长期不一致。
        guard.reload_config();
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

        guard.begin_turn();

        match guard.anchor_injection(&ev.prompt) {
            state::AnchorInjection::Full => {
                result.system_prompt_append = guard.anchor_prompt();
                changed = true;
            }
            state::AnchorInjection::Reminder => {
                result.system_prompt_append = guard.anchor_reminder();
                changed = true;
            }
            state::AnchorInjection::None => {}
        }

        if changed {
            Some(result)
        } else {
            None
        }
    });
}

/// 非核心工具 → 等价的替代用法。
///
/// 拦截如果只说「不允许」，模型往往会原样重试；给出可直接照做的替代命令，
/// 才能把「试 → 被拒 → 重试」压成一次。工具名取自 phi 宿主内建工具清单。
fn substitute_hint(tool: &str) -> &'static str {
    match tool {
        "read" => "改用 `str_replace_editor` 的 `view` 命令，或 `bash` 的 `cat` / `sed -n`",
        "write" => "改用 `str_replace_editor` 的 `create` / `insert` / `str_replace` 命令",
        "edit" => "改用 `str_replace_editor` 的 `str_replace` 命令",
        "grep" => "改用 `bash` 执行 `rg -n <pattern>`",
        "find" => "改用 `bash` 执行 `rg --files` 或 `fd`",
        "ls" => "改用 `bash` 执行 `ls`",
        "agent_spawn" | "agent_list" | "agent_wait" | "agent_cancel" => {
            "本会话不允许直呼子代理工具，请用 `bash` 完成"
        }
        "mcp_list" | "mcp_inspect" | "mcp_call" => {
            "本会话不允许直呼 MCP 工具，请用 `bash` 完成"
        }
        _ => "请改用允许的工具完成该操作",
    }
}

/// 组装拦截原因：说明白名单、给出替代用法，并在「传输工具被显式关闭」时
/// 额外提示如何放行（否则用户会误以为 read/write 坏了）。
fn block_reason(tool: &str, allowed: &[String], config: &config::Config) -> String {
    let mut reason = format!(
        "Eternal Minimal 阻止对 {tool} 的直接调用；本会话只允许直呼：{}。{}。",
        allowed.join(", "),
        substitute_hint(tool)
    );
    if !config.transport && config.transport_tools.iter().any(|name| name == tool) {
        reason.push_str("\n提示：`transport` 已关闭；`/deepseek transport on` 可放行 read/write。");
    }
    reason
}

/// Eternal Minimal 运行时守卫：阻止对非核心工具的直接调用。
fn register_tool_call(ext: &mut phi::Extension, shared: Shared) {
    ext.on_tool_call(move |ev| {
        let mut guard = shared.borrow_mut();
        if !guard.config.enabled || !guard.config.minimal {
            return None;
        }
        let allowed = guard.config.allowed_direct_tools();
        // 兜底：白名单为空时不拦任何调用（`allowed_direct_tools` 已保证非空，
        // 这里是最后一道保险，避免把整个会话锁死）。
        if allowed.is_empty() || allowed.iter().any(|name| name == &ev.tool_name) {
            return None;
        }
        guard.blocked_calls += 1;
        let reason = block_reason(&ev.tool_name, &allowed, &guard.config);
        Some(phi::ToolCallResult {
            block: true,
            reason,
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

    #[test]
    fn block_reason_should_offer_a_substitute() {
        let allowed = vec!["bash".to_string(), "str_replace_editor".to_string()];
        let config = config::Config::default();
        let reason = block_reason("read", &allowed, &config);
        assert!(reason.contains("bash, str_replace_editor"), "{reason}");
        assert!(reason.contains("str_replace_editor"), "{reason}");
        assert!(block_reason("grep", &allowed, &config).contains("rg"));
    }

    #[test]
    fn block_reason_should_explain_disabled_transport() {
        let allowed = vec!["bash".to_string()];
        let config = config::Config {
            transport: false,
            ..config::Config::default()
        };
        // read 在传输名单里但被显式关闭 → 要提示如何放行。
        assert!(block_reason("read", &allowed, &config).contains("/deepseek transport on"));
        // 不在传输名单里的工具不该出现这段提示。
        assert!(!block_reason("grep", &allowed, &config).contains("transport"));
    }
}