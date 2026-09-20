// main.rs — phi-acp 扩展入口。
//
// 由 billion-context（宿主插件）与 acp-kernel（压缩内核）合并移植而来。
//
// phi → 本扩展的钩子映射：
// - `user_input`   → 记录用户消息、重置提醒计数。
// - `tool_call`    → 记录工具调用（进入观测视图）。
// - `tool_result`  → 记录工具结果（进入观测视图）。
// - `before_agent_start` → 追加压缩哲学 + 持久规则到系统提示词。
// - `turn_stopping` → 增长驱动的提醒（nudge），可转向继续。
// - `session_start` → 重置提醒计数。
//
// 受 phi 宿主能力限制而无法移植的部分（详见 README / PLAN）：
// - 无消息历史访问：本扩展维护自己观测到的消息视图。
// - 无请求体重写钩子：无法在请求前直接改写消息数组；压缩通过块摘要 + 工具回写体现。

use std::rc::Rc;

use phi_ext::{phi, pxb};

use phi_acp::commands;
use phi_acp::config::EXTENSION_NAME;
use phi_acp::prompts::Prompts;
use phi_acp::runtime::Runtime;
use phi_acp::tools;

fn main() -> Result<(), phi::Error> {
    let mut ext = phi::Extension::new(EXTENSION_NAME, env!("CARGO_PKG_VERSION"));
    let shared: Rc<std::cell::RefCell<Runtime>> = Rc::new(std::cell::RefCell::new(Runtime::new()));

    commands::register(&mut ext, shared.clone());
    tools::register(&mut ext, shared.clone());
    register_user_input(&mut ext, shared.clone());
    register_tool_call(&mut ext, shared.clone());
    register_tool_result(&mut ext, shared.clone());
    register_before_agent_start(&mut ext, shared.clone());
    register_turn_stopping(&mut ext, shared.clone());
    register_events(&mut ext, shared);

    ext.run()
}

/// `user_input`：记录用户消息。
fn register_user_input(ext: &mut phi::Extension, shared: Rc<std::cell::RefCell<Runtime>>) {
    ext.on_user_input(move |ev| {
        if ev.text.trim().is_empty() {
            return None;
        }
        let mut guard = shared.borrow_mut();
        guard.record_user_input(&ev.text);
        guard.reset_nudges();
        None
    });
}

/// `tool_call`：记录工具调用。
fn register_tool_call(ext: &mut phi::Extension, shared: Rc<std::cell::RefCell<Runtime>>) {
    ext.on_tool_call(move |ev| {
        // 记录所有工具调用（包括本扩展自己的 compress），摘要与 compress 调用是
        // 载重元数据，compress 工具在保护规则里被硬保护。
        let input = String::from_utf8_lossy(&ev.input).to_string();
        let mut guard = shared.borrow_mut();
        if !guard.config.enabled {
            return None;
        }
        guard.record_tool_call(&ev.tool_call_id, &ev.tool_name, &input);
        None
    });
}

/// `tool_result`：记录工具结果，并把 absorb 的替换文本回写给宿主。
///
/// 返回 `Some(ToolResultResult { content: .. })` 会替换模型看到的那条结果，
/// 这是 phi 宿主上**唯一**能把内容从上游请求里真正去掉的通道（与 rtk 同路）。
/// 只写 state.json 的 compress 块对宿主历史无效。
fn register_tool_result(ext: &mut phi::Extension, shared: Rc<std::cell::RefCell<Runtime>>) {
    ext.on_tool_result(move |ev| {
        let replacement = {
            let mut guard = shared.borrow_mut();
            if !guard.config.enabled {
                return None;
            }
            guard.record_tool_result(&ev.tool_call_id, &ev.tool_name, &ev.content, ev.is_error)
        };
        replacement.map(|content| phi::ToolResultResult {
            content: Some(content),
            ..Default::default()
        })
    });
}

/// `before_agent_start`：附加一份**精简**压缩契约。
///
/// ⚠️ phi 没有「每轮重写系统提示」的钩子：`SystemPromptAppend` 被拼到**用户消息**
/// 后面并永久留在会话历史里（`ext/go/types.go` 写明 "appended to the user message"，
/// 对应 `internal/agent/engine.go` 的 `content + "\n\n" + extra`）。因此这里每注入
/// 一个 token 就是每轮永久 +1 token。
///
/// 历史版本把 `HOW_TO_COMPRESS` / `TIER2` / `TIER3` 全文（约 5.9K 字符 ≈ 1.5K
/// token）每轮都塞进来，**压缩永远压不掉它**，净效果是把上下文推大。
/// 现在每会话**只注入一次**一段精炼契约（见 `Prompts::contract`），长篇规则改由
/// nudge 在真要写摘要时按需携带。
fn register_before_agent_start(ext: &mut phi::Extension, shared: Rc<std::cell::RefCell<Runtime>>) {
    ext.on_before_agent_start(move |_ev| {
        let prompts = Prompts::default();
        // 契约每会话只发一次；持久规则随每次摘录（它们本就是用户显式要求常驻的）。
        let (contract, rules) = {
            let mut guard = shared.borrow_mut();
            if !guard.config.enabled {
                return None;
            }
            let contract = guard.take_contract_injection(&prompts.contract);
            let rules = tools::format_rules_for_prompt(&guard.state);
            (contract, rules)
        };
        let mut append = contract;
        if !rules.is_empty() {
            if !append.is_empty() {
                append.push_str("\n\n");
            }
            append.push_str(&rules);
        }
        if append.is_empty() {
            return None;
        }
        Some(phi::BeforeAgentStartResult {
            prompt: None,
            system_prompt_append: append,
        })
    });
}

/// `turn_stopping`：增长驱动的提醒。
fn register_turn_stopping(ext: &mut phi::Extension, shared: Rc<std::cell::RefCell<Runtime>>) {
    ext.on_turn_stopping(move |_ev| {
        let (should, text) = {
            let mut guard = shared.borrow_mut();
            if !guard.config.enabled {
                return None;
            }
            let outcome = guard.process();
            let nudge = outcome.nudge.as_ref()?;
            if !nudge.should_inject {
                return None;
            }
            let prompts = Prompts::default();
            let text = phi_acp::nudge::render_nudge_text(
                nudge,
                &prompts,
                outcome.context_breakdown.as_ref(),
            );
            (true, text)
        };
        if !should {
            return None;
        }
        let mut guard = shared.borrow_mut();
        if !guard.note_nudge() {
            // 连续提醒达到上限，放行停止，避免死循环。
            return None;
        }
        guard.persist();
        Some(phi::TurnStoppingResult {
            continue_: true,
            message: text,
            reason: "acp nudge".to_string(),
        })
    });
}

/// 生命周期事件。
fn register_events(ext: &mut phi::Extension, shared: Rc<std::cell::RefCell<Runtime>>) {
    let session = shared.clone();
    ext.subscribe(pxb::Event::SessionStart, move |_ev| {
        // 新会话：契约需重新注入一次（新会话历史里还没有）。
        session.borrow_mut().on_session_start();
    });

    // 会话切换 / 关闭时清掉 token 缓存：否则 `session_tokens` 会拿着上一个会话
    // 的文件偏移去读新会话的文件，使用率判断错位。
    let switching = shared;
    ext.subscribe(pxb::Event::SessionShutdown, move |_ev| {
        phi_acp::session_tokens::reset_cache();
        switching.borrow_mut().reset_nudges();
    });
}
