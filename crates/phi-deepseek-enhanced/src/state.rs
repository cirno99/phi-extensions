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

/// 本轮应注入的锚点形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorInjection {
    /// 本轮不注入。
    None,
    /// 注入完整锚点（会话首轮，或上下文压缩后重新武装）。
    Full,
    /// 注入极简风格提醒（防止首轮完整锚点被后续历史淹没）。
    Reminder,
}

/// 扩展运行状态。
pub struct State {
    /// 当前生效的配置。
    pub config: Config,
    /// 配置文件路径。
    pub config_path: PathBuf,
    /// 配置加载告警（若有）。
    pub warning: Option<String>,
    /// 本会话是否已注入过完整锚点。
    anchor_injected: bool,
    /// 本会话经历的 `before_agent_start` 轮数（用于计算提醒间隔）。
    turns: u32,
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
            turns: 0,
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
        self.turns = 0;
        self.compactions = 0;
        self.blocked_calls = 0;
    }

    /// 记录一次上下文压缩；若配置要求，则允许下次重新注入完整锚点。
    pub fn note_compaction(&mut self) {
        self.compactions += 1;
        if self.config.reanchor_after_compact {
            self.anchor_injected = false;
        }
    }

    /// 进入新一轮（每次 `before_agent_start` 调一次），用于计算提醒间隔。
    pub fn begin_turn(&mut self) {
        self.turns = self.turns.saturating_add(1);
    }

    /// 本会话已进行的轮数。
    pub fn turns(&self) -> u32 {
        self.turns
    }

    /// 判定本轮是否注入锚点，并就地推进内部状态。
    ///
    /// 规则：
    /// - 关闭注入、或用户提示词里已经带着锚点时，什么都不做；
    /// - 首轮（以及压缩后的下一轮）注入完整锚点；
    /// - 其余轮次在 `anchor_repeat` 开启时按 `anchor_repeat_every` 的间隔
    ///   注入极简提醒——完整锚点留在历史里会迅速失效，贴尾提醒才是真正
    ///   让模型持续保持 We-need 风格的杠杆。
    pub fn anchor_injection(&mut self, prompt: &str) -> AnchorInjection {
        if !self.config.inject_anchor || anchor::contains_anchor(prompt) {
            return AnchorInjection::None;
        }
        if !self.anchor_injected {
            self.anchor_injected = true;
            return AnchorInjection::Full;
        }
        if !self.config.anchor_repeat {
            return AnchorInjection::None;
        }
        let every = self.config.anchor_repeat_every.max(1);
        if self.turns % every == 0 {
            AnchorInjection::Reminder
        } else {
            AnchorInjection::None
        }
    }

    /// 生成锚点文本（按当前配置决定工具访问说明）。
    pub fn anchor_prompt(&self) -> String {
        let direct = self.config.allowed_direct_tools();
        anchor::build_anchor_prompt(&anchor::tool_note(self.config.minimal, &direct))
    }

    /// 生成本轮的极简风格提醒（minimal 下附带当前工具访问说明）。
    ///
    /// 见 [`anchor::build_anchor_reminder`]：完整锚点只在首轮注入，而守卫状态
    /// 可能在会话中途被 `/deepseek minimal on|off` 改变，因此每轮提醒都要重新
    /// 读取当前配置，保证提示与拦截行为一致。
    pub fn anchor_reminder(&self) -> String {
        let direct = self.config.allowed_direct_tools();
        anchor::build_anchor_reminder(self.config.minimal, &direct)
    }

    /// footer 状态行文本。
    pub fn footer(&self) -> String {
        if !self.config.enabled {
            return "🐋 DeepSeek Enhanced：已关闭".to_string();
        }
        let minimal = if self.config.minimal { "开" } else { "关" };
        format!(
            "🐋 DeepSeek Enhanced：minimal {minimal}｜锚点 {} 轮｜阻止 {} 次",
            self.turns, self.blocked_calls
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> State {
        State::new(Config::default(), PathBuf::from("/tmp/config.json"), None)
    }

    #[test]
    fn first_turn_should_inject_full_anchor_only_once() {
        let mut state = state();
        state.begin_turn();
        assert_eq!(state.anchor_injection("hi"), AnchorInjection::Full);
        state.begin_turn();
        // 第二轮不再是完整锚点（默认改为提醒）。
        assert_ne!(state.anchor_injection("hi"), AnchorInjection::Full);
        state.reset_for_session();
        state.begin_turn();
        assert_eq!(state.anchor_injection("hi"), AnchorInjection::Full);
    }

    #[test]
    fn reminder_should_follow_on_every_turn_by_default() {
        let mut state = state();
        for turn in 1..=4 {
            state.begin_turn();
            let expected = if turn == 1 {
                AnchorInjection::Full
            } else {
                AnchorInjection::Reminder
            };
            assert_eq!(state.anchor_injection("hi"), expected, "第 {turn} 轮");
        }
        assert_eq!(state.turns(), 4);
    }

    #[test]
    fn reminder_should_respect_configured_interval() {
        let mut state = state();
        state.config.anchor_repeat_every = 3;
        let mut seen = Vec::new();
        for _ in 0..6 {
            state.begin_turn();
            seen.push(state.anchor_injection("hi"));
        }
        assert_eq!(
            seen,
            vec![
                AnchorInjection::Full,     // 第 1 轮
                AnchorInjection::None,     // 第 2 轮
                AnchorInjection::Reminder, // 第 3 轮
                AnchorInjection::None,     // 第 4 轮
                AnchorInjection::None,     // 第 5 轮
                AnchorInjection::Reminder, // 第 6 轮
            ]
        );
    }

    #[test]
    fn interval_should_clamp_zero_to_one() {
        let mut state = state();
        state.config.anchor_repeat_every = 0;
        state.begin_turn();
        assert_eq!(state.anchor_injection("hi"), AnchorInjection::Full);
        state.begin_turn();
        assert_eq!(state.anchor_injection("hi"), AnchorInjection::Reminder);
    }

    #[test]
    fn repeat_off_should_never_inject_reminder() {
        let mut state = state();
        state.config.anchor_repeat = false;
        state.begin_turn();
        assert_eq!(state.anchor_injection("hi"), AnchorInjection::Full);
        for _ in 0..3 {
            state.begin_turn();
            assert_eq!(state.anchor_injection("hi"), AnchorInjection::None);
        }
    }

    #[test]
    fn injection_off_should_be_a_no_op() {
        let mut state = state();
        state.config.inject_anchor = false;
        state.begin_turn();
        assert_eq!(state.anchor_injection("hi"), AnchorInjection::None);
    }

    #[test]
    fn prompt_already_carrying_anchor_should_suppress_injection() {
        let mut state = state();
        state.begin_turn();
        let prompt = anchor::build_anchor_reminder(false, &[]);
        assert_eq!(state.anchor_injection(&prompt), AnchorInjection::None);
        // 未消耗掉首轮名额：下一轮仍然是完整锚点。
        state.begin_turn();
        assert_eq!(state.anchor_injection("hi"), AnchorInjection::Full);
    }

    #[test]
    fn compaction_should_rearm_full_anchor_when_configured() {
        let mut state = state();
        state.begin_turn();
        assert_eq!(state.anchor_injection("hi"), AnchorInjection::Full);
        state.note_compaction();
        state.begin_turn();
        assert_eq!(state.anchor_injection("hi"), AnchorInjection::Full);
        assert_eq!(state.compactions, 1);
    }

    #[test]
    fn compaction_should_not_rearm_when_disabled() {
        let mut state = state();
        state.config.reanchor_after_compact = false;
        state.begin_turn();
        assert_eq!(state.anchor_injection("hi"), AnchorInjection::Full);
        state.note_compaction();
        state.begin_turn();
        assert_eq!(state.anchor_injection("hi"), AnchorInjection::Reminder);
    }

    #[test]
    fn anchor_prompt_should_reflect_minimal_mode() {
        let mut state = state();
        state.config.minimal = true;
        assert!(state.anchor_prompt().contains("Eternal Minimal"));
    }

    #[test]
    fn anchor_reminder_should_reflect_minimal_mode() {
        let mut state = state();
        state.config.minimal = true;
        let reminder = state.anchor_reminder();
        assert!(reminder.contains("Eternal Minimal"));
        assert!(reminder.contains("bash, str_replace_editor"));
        // 关掉后提醒不再提工具约束。
        state.config.minimal = false;
        assert!(!state.anchor_reminder().contains("Eternal Minimal"));
    }

    #[test]
    fn footer_should_report_turns_and_blocks() {
        let mut state = state();
        state.begin_turn();
        state.blocked_calls = 3;
        let footer = state.footer();
        assert!(footer.contains("锚点 1 轮"), "{footer}");
        assert!(footer.contains("阻止 3 次"), "{footer}");
    }
}