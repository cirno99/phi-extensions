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
        let mut guard = shared.borrow_mut();
        if !guard.config.enabled {
            return None;
        }
        // 只在真正要记录时才把入参转成字符串（禁用时不做这次分配）。
        let input = String::from_utf8_lossy(&ev.input);
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
        let append = {
            let mut guard = shared.borrow_mut();
            if !guard.config.enabled {
                return None;
            }
            // 契约每会话一次；规则只在「内容变化 / 新会话 / 宿主压缩后」重发——
            // 两者都会被拼进用户消息并永久留在历史里，每轮重发就是每轮永久 +N token。
            let rules = phi_acp::rules::format_rules_for_prompt(&guard.state);
            guard.take_prompt_append(&prompts.contract, &rules)
        };
        if append.is_empty() {
            return None;
        }
        Some(phi::BeforeAgentStartResult {
            prompt: None,
            system_prompt_append: append,
        })
    });
}

/// `turn_stopping`：高水位提醒（仅在 `autoNudgeEnabled` 打开时）。
///
/// # 为什么默认不发
///
/// 这条钩子返回 `continue_` 会做两件在 phi 上都是净亏的事：把转向消息当作
/// user 消息永久 append 进历史，并**跳过**同轮的宿主 `runCompact`
/// （`internal/agent/engine.go`：`continue` 之后才轮到 compaction）——
/// 而提醒想换来的 `compress` 块在 phi 上压不掉任何宿主历史（见 `crate::absorb`）。
/// 因此默认关闭；真正能删 token 的是静默的 absorb。
///
/// 即使不发提醒，仍然跑一次 `process()`：推荐/块同步等状态需要在每轮结束
/// 时更新（`/acp status` 与 `compress` 工具都读它）。
fn register_turn_stopping(ext: &mut phi::Extension, shared: Rc<std::cell::RefCell<Runtime>>) {
    ext.on_turn_stopping(move |_ev| {
        let (should, text) = {
            let mut guard = shared.borrow_mut();
            if !guard.config.enabled {
                return None;
            }
            let auto = guard.config.auto_nudge_enabled;
            // absorb 统计只在内存里累加（见 `record_tool_result`），这里统一落盘。
            // 放在 `return None` 之前：即使不发提醒，本 turn 的回收量也要持久化，
            // 否则重启后 `/acp status` 的 absorbed 会归零。
            guard.persist_if_dirty();
            if !auto {
                // 提醒关闭时不跑内核管线：默认路径上这是最贵的一步（assign_refs /
                // sync_blocks / prune / recommend 全量扫消息与块），而它的产物只服务
                // 于提醒；`/acp status` 与 `compress` 工具会各自按需触发 `process()`。
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
    ext.subscribe(pxb::Event::SessionStart, move |ev| {
        // 新会话：契约需重新注入一次（新会话历史里还没有）；若会话身份变了
        // 还要换账本——否则新会话会继承上一个会话的 ref 索引与块账本，
        // `acp_status` 报出的可压缩范围在宿主机历史里根本不存在。
        // `ev.reason` 取 `startup` / `resume` / `new`（`controller.go` 的
        // `emitSessionStart`）；`ev.previous_session_id` 是刚离开的会话。
        session
            .borrow_mut()
            .on_session_start(&ev.reason, &ev.previous_session_id);
    });

    // 宿主原生压缩后，观测视图里的旧消息已从上游请求里消失，必须重新同步，
    // 否则 ref 索引 / 可压缩范围会指向已不存在的内容（见 `Runtime::on_host_compaction`）。
    let compacted = shared.clone();
    ext.subscribe(pxb::Event::SessionCompact, move |_ev| {
        compacted.borrow_mut().on_host_compaction();
    });

    // 会话切换 / 关闭时清掉 token 缓存：否则 `session_tokens` 会拿着上一个会话
    // 的文件偏移去读新会话的文件，使用率判断错位。顺带释放检索特征缓存
    // （对应上游 `clearDocFeatures`，可选：字符上限本身已经限制了它）。
    let switching = shared;
    ext.subscribe(pxb::Event::SessionShutdown, move |_ev| {
        phi_acp::session_tokens::reset_cache();
        phi_acp::search::doc_cache::clear_doc_features();
        switching.borrow_mut().reset_nudges();
    });
}
