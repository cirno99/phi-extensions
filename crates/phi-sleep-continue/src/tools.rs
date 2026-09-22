// tools.rs — 无人值守的「AI 主动收尾」工具。
//
// 参照 pi-extension-watchdog 的 `stop_watchdog`：仅靠迭代上限（`max`）无法判断
// 任务是否真的完成，无人值守会在任务收尾后仍被反复催促。因此给模型一个显式
// 的收尾通道：收到固定触发行（`state::TRIGGER_LINE`）后，若任务完成或需等待
// 用户决策，就调用本工具结束本轮。
//
// 与 pi 版的差异：
// - pi 在首次启动监控时才注册工具（未启用不占 token）；phi 的工具列表在握手
//   时一次性上报、运行期无法增删，故本工具常驻注册（多占一个极小定义）。
// - pi 的 `stop_watchdog` 可直接 abort 当前回合；phi 无 abort 能力，改为置位
//   `stop_requested`，由随后的 `turn_stopping` 消费后结束本轮。

use phi_ext::phi;

use crate::state::Shared;

/// 工具名。AI 在任务完成或需等待用户决策时调用，结束本轮自动继续。
pub const STOP_TOOL_NAME: &str = "stop_sleep";

/// 注册 `stop_sleep` 工具。
pub fn register(ext: &mut phi::Extension, shared: Shared) {
    let tool = phi::Tool::new(
        STOP_TOOL_NAME,
        "结束无人值守自动继续。仅当收到「【自动催促·非用户输入】」消息后，确认任务已完成\
         或需等待用户决策时调用；调用后本轮结束，不再自动继续。普通对话不要调用。",
        phi::Schema::object(),
        move |_args: &[u8]| -> Result<phi::ToolResult, String> {
            let active = {
                let mut guard = shared.borrow_mut();
                let active = guard.enabled && !guard.suspended;
                guard.request_stop();
                active
            };
            let content = if active {
                "OK. 已记录收尾请求，本轮结束后不再自动继续。"
            } else {
                "无人值守未在运行，无需调用 stop_sleep。"
            };
            Ok(phi::ToolResult {
                content: content.to_string(),
                ..Default::default()
            })
        },
    );
    ext.register_tool(tool);
}
