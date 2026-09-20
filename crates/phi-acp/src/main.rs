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

/// `tool_result`：记录工具结果。
fn register_tool_result(ext: &mut phi::Extension, shared: Rc<std::cell::RefCell<Runtime>>) {
    ext.on_tool_result(move |ev| {
        let mut guard = shared.borrow_mut();
        if !guard.config.enabled {
            return None;
        }
        guard.record_tool_result(&ev.tool_call_id, &ev.tool_name, &ev.content);
        None
    });
}

/// `before_agent_start`：追加压缩哲学与持久规则到系统提示词。
fn register_before_agent_start(ext: &mut phi::Extension, shared: Rc<std::cell::RefCell<Runtime>>) {
    ext.on_before_agent_start(move |_ev| {
        let prompts = Prompts::default();
        let rules = {
            let guard = shared.borrow();
            if !guard.config.enabled {
                return None;
            }
            tools::format_rules_for_prompt(&guard.state)
        };
        let mut append = String::new();
        append.push_str("ACP CONTEXT COMPRESSION\n\n");
        append.push_str(&prompts.compress_philosophy);
        append.push_str("\n\n");
        append.push_str(&prompts.how_to_compress_rules);
        if !rules.is_empty() {
            append.push_str("\n\n");
            append.push_str(&rules);
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
            let nudge = outcome.nudge?;
            if !nudge.should_inject {
                return None;
            }
            let prompts = Prompts::default();
            let text = phi_acp::nudge::render_nudge_text(&nudge, &prompts, None);
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
        session.borrow_mut().reset_nudges();
    });
}
