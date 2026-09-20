// main.rs — phi-asymptotic-thinking 扩展入口。
//
// 由 pi 版 asymptotic-thinking 扩展的 src/index.ts 移植。
//
// pi → phi 的钩子映射：
// - `before_agent_start`（注入引导 + 系统提示词规则）→ `on_before_agent_start`
//   （phi 只能改写用户提示词或追加系统提示词，无法注入独立消息，因此引导文本
//   与框架规则一并追加到系统提示词）。
// - `turn_end`（steer 提醒）→ `on_turn_stopping`（`Continue + message`）。
//   phi 只在「本轮无工具调用、即将结束」时给一次转向机会，语义上正好覆盖
//   「该流转却没流转」的违规场景；连续转向超过上限后放行停止，避免死循环。
// - `before_provider_request`（改 temperature/topP）→ 无对应钩子，仅保留
//   建议参数供 `/asymptotic-status` 展示（见 types.rs 的推理参数表）。
// - `session_start` / `session_shutdown` → `subscribe`，仅用于记录会话 ID。

mod commands;
mod prompts;
mod runtime;
mod state_machine;
mod store;
mod templates;
mod tools;
mod types;

use phi_ext::{phi, pxb};

use runtime::Shared;
use state_machine::WarnLevel;
use types::{SessionState, State};

/// 追加到系统提示词的框架规则块（对应 pi 版的 `asymptotic_framework` section）。
fn framework_rules_block() -> String {
    format!(
        "========================== 渐近式思考框架START =========================================\n\
         来源：扩展内联 framework-rules（编译版自包含）\n\
         ---\n\n\
         {}\n\n\
         ========================== 渐近式思考框架END ===========================================",
        templates::FRAMEWORK_RULES
    )
}

fn main() -> Result<(), phi::Error> {
    let mut ext = phi::Extension::new(store::EXTENSION_NAME, env!("CARGO_PKG_VERSION"));
    let rt = runtime::shared();

    ext.register_tool(tools::task_info_tool(rt.clone()));
    ext.register_tool(tools::transition_tool(rt.clone()));
    ext.register_tool(tools::status_tool(rt.clone()));
    commands::register(&mut ext, rt.clone());

    register_before_agent_start(&mut ext, rt.clone());
    register_turn_stopping(&mut ext, rt.clone());
    register_session_events(&mut ext, rt.clone());

    ext.run()
}

/// `before_agent_start`：把状态机引导 + 框架规则追加到系统提示词。
///
/// END / 未初始化统一视为 START（保留累计任务数），与 pi 版一致。
fn register_before_agent_start(ext: &mut phi::Extension, rt: Shared) {
    ext.on_before_agent_start(move |_ev| {
        let guard = rt.borrow_mut();
        if !guard.store.is_enabled() {
            return None;
        }

        let mut state = guard.load();
        if state.state == Some(State::End) || state.state.is_none() {
            let task_turn_count = state.task_turn_count;
            state = SessionState {
                state: Some(State::Start),
                task_turn_count,
                ..SessionState::default()
            };
            let _ = guard.save(&state);
        }

        let current = state.state.unwrap_or(State::Start);
        let mut guide = templates::build_template(
            current,
            state.task_turn_count,
            state.master_task_type,
            state.sub_task_type,
            state.difficulty,
            &state.visited,
        );
        if current != State::Start {
            guide.push_str(&templates::continuation_notice(current));
        }

        Some(phi::BeforeAgentStartResult {
            prompt: None,
            system_prompt_append: format!("{guide}\n\n{}", framework_rules_block()),
        })
    });
}

/// `turn_stopping`：状态守卫 + 违规提醒 + 超限警告。
fn register_turn_stopping(ext: &mut phi::Extension, rt: Shared) {
    ext.on_turn_stopping(move |_ev| {
        let mut guard = rt.borrow_mut();
        if !guard.store.is_enabled() {
            return None;
        }

        let mut state = guard.load();
        let current = state.state.unwrap_or(State::Start);
        if current == State::End {
            return None;
        }

        let advice = state_machine::bump_and_warn(&mut state);
        let hard_stop = matches!(advice.as_ref(), Some(a) if a.level == WarnLevel::HardStop);
        let advice_text = advice.map(|a| a.text).unwrap_or_default();
        let _ = guard.save(&state);

        // 严重超时：不再续跑，放行停止（pi 版此处也只做告知）。
        if hard_stop {
            guard.pending_reminder = Some(advice_text);
            return None;
        }

        let transition_called = guard.transition_called;
        guard.transition_called = false;

        let interval_due = !advice_text.is_empty() && state_machine::should_emit_reminder(&state);
        // 违规（本轮未流转）优先；否则按提醒间隔节流。
        if transition_called && !interval_due {
            return None;
        }
        if guard.consecutive_steers >= runtime::MAX_CONSECUTIVE_STEERS {
            return None;
        }

        let max_turns = state_machine::max_state_turns(current, state.difficulty);
        let mut reminder = templates::state_guard(current, state.state_turn_count, max_turns);
        if !transition_called {
            reminder.push_str(&templates::violation_warning());
        }
        reminder.push_str(&advice_text);

        let message = templates::wrap_task(state.task_turn_count, &reminder);
        guard.consecutive_steers += 1;
        guard.last_steer = Some(message.clone());
        Some(phi::TurnStoppingResult {
            continue_: true,
            message,
            reason: "asymptotic-thinking 状态守卫".into(),
        })
    });
}

/// `session_start` / `session_shutdown`：记录会话 ID 与重置转向计数。
fn register_session_events(ext: &mut phi::Extension, rt: Shared) {
    {
        let rt = rt.clone();
        ext.subscribe(pxb::Event::SessionStart, move |ev| {
            let mut guard = rt.borrow_mut();
            if !ev.session_id.is_empty() {
                guard.session_id = ev.session_id;
            }
            guard.consecutive_steers = 0;
            guard.last_steer = None;
        });
    }
    ext.subscribe(pxb::Event::SessionShutdown, move |_ev| {
        let mut guard = rt.borrow_mut();
        guard.session_id.clear();
    });
}