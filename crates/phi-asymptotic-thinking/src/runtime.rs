// runtime.rs — 扩展运行期共享状态。
//
// phi 的拦截/订阅回调签名是 `FnMut(Event) -> Option<Result> + 'static`，
// 不带 `Context`、也不带 `&mut self`，因此所有跨回调状态都收敛到
// `Rc<RefCell<Runtime>>`，克隆进每个闭包。
//
// 与 pi 版的差异：
// - pi 用 `ctx.sessionManager.getSessionId()` 在任意钩子里拿会话 ID；
//   phi 只有命令处理器能拿到，所以 session_id 由 `subscribe(SessionStart)`
//   记录到本结构体，仅用于展示。
// - pi 用 `turn_end` 直接 `sendMessage(steer)`；phi 的等价出口是
//   `on_turn_stopping` 的 `Continue + message`，因此这里额外维护
//   「本轮是否已流转」「连续转向次数」两项，用于违规检测与防死循环。

use std::cell::RefCell;
use std::rc::Rc;

use crate::store::Store;
use crate::types::SessionState;

/// 连续转向上限：模型连续 N 轮既未流转也不收尾时放行停止，避免烧 token 的死循环。
pub const MAX_CONSECUTIVE_STEERS: u32 = 5;

/// 扩展运行期共享状态。
pub struct Runtime {
    /// 状态与开关的持久化句柄。
    pub store: Store,
    /// 当前会话 ID（由 `subscribe(SessionStart)` 记录，未知时为空串）。
    pub session_id: String,
    /// 本轮是否调用过 `asymptotic-think_transition`（供违规检测消费后清零）。
    pub transition_called: bool,
    /// 最近一次 `bump_and_warn` 产生的提醒文本，待 `turn_stopping` 取用。
    pub pending_reminder: Option<String>,
    /// 最近一次实际发给宿主的转向文本（用于去重，避免重复注入同一段）。
    pub last_steer: Option<String>,
    /// 自上次成功流转以来连续转向的次数。
    pub consecutive_steers: u32,
}

impl Runtime {
    /// 使用扩展标准状态目录创建运行时。
    pub fn new() -> Self {
        Self {
            store: Store::default_location(),
            session_id: String::new(),
            transition_called: false,
            pending_reminder: None,
            last_steer: None,
            consecutive_steers: 0,
        }
    }

    /// 读取会话状态。
    pub fn load(&self) -> SessionState {
        self.store.load()
    }

    /// 原子写回会话状态。
    pub fn save(&self, state: &SessionState) -> Result<(), phi_ext_common::config::ConfigError> {
        self.store.save(state)
    }

    /// 流转成功后重置违规/转向计数。
    pub fn on_transition(&mut self) {
        self.transition_called = true;
        self.pending_reminder = None;
        self.last_steer = None;
        self.consecutive_steers = 0;
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

/// 扩展内部共享状态别名。
pub type Shared = Rc<RefCell<Runtime>>;

/// 创建共享运行时。
pub fn shared() -> Shared {
    Rc::new(RefCell::new(Runtime::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_transition_should_reset_steer_accounting() {
        let mut rt = Runtime::new();
        rt.consecutive_steers = 3;
        rt.last_steer = Some("x".into());
        rt.pending_reminder = Some("y".into());
        rt.on_transition();
        assert!(rt.transition_called);
        assert_eq!(rt.consecutive_steers, 0);
        assert!(rt.last_steer.is_none());
        assert!(rt.pending_reminder.is_none());
    }

    #[test]
    fn max_consecutive_steers_should_be_positive() {
        const { assert!(MAX_CONSECUTIVE_STEERS > 0) };
    }
}