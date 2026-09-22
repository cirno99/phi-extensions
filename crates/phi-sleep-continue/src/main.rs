// main.rs — phi-sleep-continue 扩展入口（无人值守自动继续）。
//
// 由 pi 版 sleep-continue 扩展的 src/index.ts 移植。
//
// pi → phi 的钩子映射：
// - 核心 1（拦截提问工具，自动选推荐项）→ `on_tool_call` 返回 `Block + reason`，等价。
// - 核心 2（收集可重试错误）→ `on_tool_result` 观察 `is_error` 文本，等价。
// - 核心 3（settled 后自动继续）→ `on_turn_stopping` 返回 `Continue + message`。
//   phi 只在「本轮无工具调用、即将结束」时给一次转向机会，正好对应
//   pi 的 `agent_settled`。
//
// 另参照 pi-extension-watchdog（https://github.com/GreenHatHG/pi-extension-watchdog）
// 补齐「任务完成即收尾」的能力：
// - 固定触发行（`state::TRIGGER_LINE`）+ 用户任务指令，避免自定义文本冲淡语义；
// - `stop_sleep` 工具（`tools.rs`）：AI 主动结束/挂起本轮，解决仅靠上限判断
//   不了「任务是否真的完成」而反复空催的问题；
// - `mode=keep` 常驻模式：`stop_sleep` 只挂起，用户下一条真实输入自动恢复。
//
// 受 phi 宿主能力限制而无法移植的部分（详见 PLAN.md）：
// - 看门狗 + `ctx.abort()`：phi 无 abort RPC，也无定时器回调。
// - 指数退避 `await sleep(delay)`：`turn_stopping` 是同步回调，无法在两次
//   续跑之间等待；改为即时重试 + 连续失败计数。
// - Esc 中断检测：phi 的 `AgentEnd` 事件不带 `aborted` 标志。
// - 每次续跑的 toast：拦截/订阅回调拿不到 `Context`，仅在命令里刷新 footer。
//
// 另参照 pi-auto-approval（https://github.com/Europa2061/pi-auto-approval）
// 增加了一层「规则化自动审批」：无人值守开启时，工具调用按
// 只读 / 工作区内写入 / 安全只读命令 / 白名单 的规则放行，其余一律阻止，
// 避免半夜自动执行危险动作。分类器与人工回退因 phi 无模型调用、
// `tool_call` 期也无 UI 而移除，详见 PLAN.md。

mod approval;
mod commands;
mod config;
mod state;
mod tools;

use std::cell::RefCell;
use std::rc::Rc;

use phi_ext::{phi, pxb};
use phi_ext_common::arena::Scratch;

use state::{is_retryable, question_block_reason, summarize_recommended_choices, Shared};

/// 收集错误文本时保留的最大字符数。
const ERROR_SNIPPET_CHARS: usize = 2000;
/// 回给模型的错误原因保留的最大字符数。
const REASON_SNIPPET_CHARS: usize = 300;

fn main() -> Result<(), phi::Error> {
    let mut ext = phi::Extension::new("phi-sleep-continue", env!("CARGO_PKG_VERSION"));
    let shared = state::shared();
    // 审批判定在每次工具调用上执行：竞技场单独放在一个共享单元里，
    // 与 `SleepState` 分开借用以避免 `Bump` 卷入状态类型的派生约束。
    let scratch = Rc::new(RefCell::new(Scratch::with_capacity(4 * 1024)));

    commands::register(&mut ext, shared.clone());
    tools::register(&mut ext, shared.clone());
    register_tool_call(&mut ext, shared.clone(), scratch);
    register_tool_result(&mut ext, shared.clone());
    register_turn_stopping(&mut ext, shared.clone());
    register_user_input(&mut ext, shared.clone());
    register_events(&mut ext, shared);

    ext.run()
}

/// 按字符边界截断，避免切断多字节字符导致 panic。
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect()
}

/// 核心 1：拦截提问类工具，按「每题首选项 = 推荐项」自动作答；
/// 随后对普通工具调用施加规则化自动审批。
fn register_tool_call(ext: &mut phi::Extension, shared: Shared, scratch: Rc<RefCell<Scratch>>) {
    let question_tools = state::question_tool_names();
    ext.on_tool_call(move |ev| {
        let mut guard = shared.borrow_mut();
        if !guard.enabled {
            return None;
        }

        // `stop_sleep`：AI 主动收尾的控制工具，必须放行，且不受自动审批管辖。
        if ev.tool_name == tools::STOP_TOOL_NAME {
            return None;
        }

        // 提问类工具：自动选推荐项，避免半夜卡在弹窗上。
        if question_tools.contains(ev.tool_name.as_str()) {
            let summary = phi_ext_common::json::value(&ev.input)
                .ok()
                .as_ref()
                .and_then(summarize_recommended_choices);
            return Some(phi::ToolCallResult {
                block: true,
                reason: question_block_reason(&guard, summary.as_deref()),
                ..Default::default()
            });
        }

        // 规则化自动审批。拦截回调拿不到 `Context.cwd`，用扩展进程的工作目录
        // （宿主以会话 cwd 启动扩展进程）。
        if !guard.approval.enabled {
            return None;
        }
        let input = phi_ext_common::json::value(&ev.input).unwrap_or_default();
        let cwd = std::env::current_dir()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();

        let (route, decision, subject) = {
            let mut arena = scratch.borrow_mut();
            let result = approval::evaluate(
                arena.arena(),
                &ev.tool_name,
                &input,
                &cwd,
                &guard.approval,
                &guard.approval_store,
            );
            // 判定结果全部为拥有所有权的值，临时内存可立即整体释放。
            arena.finish();
            result
        };
        guard.last_route = Some(route);

        match decision {
            approval::Decision::Allow => {
                guard.approval_store.record_non_denial();
                guard.last_denied = None;
                None
            }
            approval::Decision::Deny(mut reason) => {
                let denials = guard.approval_store.record_denial();
                guard.last_denied = subject;
                if denials >= guard.approval.max_consecutive_denials {
                    reason.push_str(&format!(
                        "\n已连续被自动审批拒绝 {denials} 次，请停止动作，向用户说明情况并等待人工处理。"
                    ));
                }
                Some(phi::ToolCallResult {
                    block: true,
                    reason,
                    ..Default::default()
                })
            }
        }
    });
}

/// 核心 2：从工具失败结果里收集可重试原因。
fn register_tool_result(ext: &mut phi::Extension, shared: Shared) {
    ext.on_tool_result(move |ev| {
        let mut guard = shared.borrow_mut();
        if !guard.enabled {
            return None;
        }
        if !ev.is_error {
            // 工具成功即视为已恢复，清掉上一次失败的重试说明，
            // 避免后续催促里夹带已过期的错误信息。
            guard.pending_retry_reason = None;
            return None;
        }
        let raw = phi_ext_common::json::to_string(&ev.content).unwrap_or_default();
        let snippet = truncate_chars(&raw, ERROR_SNIPPET_CHARS);
        if is_retryable(&snippet) {
            guard.pending_retry_reason = Some(format!(
                "工具 {} 失败：{}",
                ev.tool_name,
                truncate_chars(&snippet, REASON_SNIPPET_CHARS)
            ));
        }
        None
    });
}

/// 核心 3：即将停下时自动续跑；有待重试原因时携带重试说明。
fn register_turn_stopping(ext: &mut phi::Extension, shared: Shared) {
    ext.on_turn_stopping(move |_ev| {
        let mut guard = shared.borrow_mut();
        if !guard.enabled {
            return None;
        }
        // `stop_sleep`：AI 主动收尾（任务完成或需等待用户决策）。
        if guard.consume_stop() {
            return None;
        }
        if guard.suspended {
            return None;
        }
        if guard.count >= guard.max {
            guard.enabled = false;
            guard.pending_retry_reason = None;
            return None;
        }

        let retry_reason = guard.pending_retry_reason.take();
        guard.count += 1;
        let message = match retry_reason {
            Some(reason) => {
                guard.consecutive_errors += 1;
                format!(
                    "{}{reason}。请重试上一步操作。\n{}",
                    state::RETRY_PREFIX,
                    guard.nudge_message()
                )
            }
            None => {
                guard.consecutive_errors = 0;
                guard.nudge_message()
            }
        };

        Some(phi::TurnStoppingResult {
            continue_: true,
            message,
            reason: "sleep-continue 自动继续".into(),
        })
    });
}

/// 人手输入接管后重置预算。
///
/// phi 的 `UserInputEvent` 没有 `source` 字段，用「文本是否为我们注入的催促」
/// 来判别，避免把自己的注入当成人手输入而反复清零计数。`turn_stopping` 的
/// 转向消息不经过本回调，因此这里见到的多为真实用户输入；常驻模式挂起时
/// 由真实输入自动恢复。
fn register_user_input(ext: &mut phi::Extension, shared: Shared) {
    ext.on_user_input(move |ev| {
        let mut guard = shared.borrow_mut();
        let injected = ev.text.starts_with(state::TRIGGER_LINE)
            || ev.text.starts_with(state::RETRY_PREFIX)
            || ev.text == guard.continue_text;
        if !injected {
            guard.take_user_over();
        }
        None
    });
}

/// 生命周期事件：维护会话 ID 与配置。
fn register_events(ext: &mut phi::Extension, shared: Shared) {
    {
        let shared = shared.clone();
        ext.subscribe(pxb::Event::SessionStart, move |ev| {
            let mut guard = shared.borrow_mut();
            if !ev.session_id.is_empty() {
                guard.session_id = ev.session_id;
            }
            guard.reload_config();
            if matches!(ev.reason.as_str(), "new" | "resume" | "fork") {
                guard.reset_budget();
                guard.suspended = false;
                guard.approval_store.clear();
            }
        });
    }
    {
        let shared = shared.clone();
        ext.subscribe(pxb::Event::SessionShutdown, move |_ev| {
            let mut guard = shared.borrow_mut();
            guard.session_id.clear();
        });
    }
}
