// state.rs — 会话级运行状态。
//
// 由于 phi 的拦截/订阅回调拿不到 `Context`（无法 notify / set_status），
// 所有可观测状态都收敛在这里，由 `/deepseek status` 统一展示。

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use crate::anchor;
use crate::config::{self, Config};

/// 共享状态句柄。
pub type Shared = Rc<RefCell<State>>;

/// 构造共享状态并载入配置。
pub fn shared() -> Shared {
    let path = config::config_path();
    let (config, warning) = config::load(&path);
    Rc::new(RefCell::new(State::new(config, path, warning)))
}

/// 扩展运行状态。
pub struct State {
    /// 当前生效的配置。
    pub config: Config,
    /// 配置文件路径。
    pub config_path: PathBuf,
    /// 配置加载告警（若有）。
    pub warning: Option<String>,
    /// 本会话是否已注入锚点。
    anchor_injected: bool,
    /// 本会话经历的上下文压缩次数。
    pub compactions: u32,
    /// 被 Eternal Minimal 守卫阻止的调用次数。
    pub blocked_calls: u64,
}

impl State {
    fn new(config: Config, config_path: PathBuf, warning: Option<String>) -> Self {
        Self {
            config,
            config_path,
            warning,
            anchor_injected: false,
            compactions: 0,
            blocked_calls: 0,
        }
    }

    /// 重新载入配置。
    pub fn reload_config(&mut self) {
        let (config, warning) = config::load(&self.config_path);
        self.config = config;
        self.warning = warning;
    }

    /// 新会话开始：重置锚点与计数。
    pub fn reset_for_session(&mut self) {
        self.anchor_injected = false;
        self.compactions = 0;
        self.blocked_calls = 0;
    }

    /// 记录一次上下文压缩；若配置要求，则允许下次重新注入锚点。
    pub fn note_compaction(&mut self) {
        self.compactions += 1;
        if self.config.reanchor_after_compact {
            self.anchor_injected = false;
        }
    }

    /// 尝试占用「本次会话的锚点注入名额」。
    ///
    /// 返回 `true` 表示本次应注入锚点（并已置位）；`false` 表示本会话已注入过。
    pub fn take_anchor(&mut self) -> bool {
        if self.anchor_injected {
            false
        } else {
            self.anchor_injected = true;
            true
        }
    }

    /// 生成锚点文本（按当前配置决定工具访问说明）。
    pub fn anchor_prompt(&self) -> String {
        let direct = self.config.allowed_direct_tools();
        anchor::build_anchor_prompt(&anchor::tool_note(self.config.minimal, &direct))
    }

    /// footer 状态行文本。
    pub fn footer(&self) -> String {
        if !self.config.enabled {
            return "🐋 DeepSeek Enhanced：已关闭".to_string();
        }
        let minimal = if self.config.minimal { "开" } else { "关" };
        format!("🐋 DeepSeek Enhanced：minimal {minimal}｜阻止 {} 次", self.blocked_calls)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> State {
        State::new(Config::default(), PathBuf::from("/tmp/config.json"), None)
    }

    #[test]
    fn take_anchor_should_only_fire_once_per_session() {
        let mut state = state();
        assert!(state.take_anchor());
        assert!(!state.take_anchor());
        state.reset_for_session();
        assert!(state.take_anchor());
    }

    #[test]
    fn compaction_should_rearm_anchor_when_configured() {
        let mut state = state();
        assert!(state.take_anchor());
        state.note_compaction();
        assert!(state.take_anchor());
        assert_eq!(state.compactions, 1);
    }

    #[test]
    fn compaction_should_not_rearm_when_disabled() {
        let mut state = state();
        state.config.reanchor_after_compact = false;
        assert!(state.take_anchor());
        state.note_compaction();
        assert!(!state.take_anchor());
    }

    #[test]
    fn anchor_prompt_should_reflect_minimal_mode() {
        let mut state = state();
        state.config.minimal = true;
        assert!(state.anchor_prompt().contains("Eternal Minimal"));
    }
}