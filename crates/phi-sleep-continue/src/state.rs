// state.rs — 无人值守自动继续的纯逻辑：配置、可重试错误判定、提问选项摘要。
//
// 由 pi 版 sleep-continue 扩展的 src/index.ts 移植。
//
// 与 pi 版的差异（受 phi 宿主能力限制）：
// - pi 用 `setInterval` 看门狗 + `ctx.abort()` 处理假死；phi 没有 abort RPC、
//   也没有定时器回调，故看门狗整体移除。
// - pi 用 `await sleep(delay)` 做指数退避重试；phi 的 `turn_stopping` 是同步
//   回调，无法在两次续跑之间等待，故退避改为「即时重试 + 连续失败计数」。
// - pi 用 `event.source` 区分人手输入与插件注入；phi 的 `UserInputEvent` 只有
//   文本字段，故一律视为人手输入。

use std::collections::BTreeSet;
use std::path::PathBuf;

use phi_ext_common::json::{Value, ValueAsArray, ValueAsScalar, ValueObjectAccess};

use crate::approval::{ApprovalStore, Route};
use crate::config::{self, ApprovalConfig};

/// 默认自动继续文本（作为「任务指令」追加在固定触发行之后）。
pub const DEFAULT_CONTINUE_TEXT: &str = "继续";
/// 默认迭代上限。
pub const DEFAULT_MAX: u32 = 100;
/// 固定触发行：语义永不缺席，负责告诉模型「何时调用 `stop_sleep` 收尾」。
///
/// 参照 pi-extension-watchdog 的固定触发行设计：用户自定义的继续文本只作为
/// 「任务指令」追加在其后，不会替换本行语义，从而避免无人值守在任务完成后
/// 仍被无限催促。
pub const TRIGGER_LINE: &str = "【自动催促·非用户输入】若任务尚未完成，请直接继续执行，无需回复本条；若任务已完成，或需要等待用户决策，请不要再改动，用一句话说明后调用 stop_sleep 结束本轮。";
/// 携带重试原因时的固定前缀（用于区分插件注入与真实用户输入）。
pub const RETRY_PREFIX: &str = "上次运行遇到问题：";

/// 需要自动使用推荐选项的提问类工具名。
pub fn question_tool_names() -> BTreeSet<&'static str> {
    ["ask_user_question", "ask_user", "question", "questionnaire"]
        .into_iter()
        .collect()
}

/// 插件运行期状态（进程内存，重启扩展后清零，与 pi 版心智一致）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SleepState {
    /// 是否启用无人值守。
    pub enabled: bool,
    /// 自动注入的继续文本。
    pub continue_text: String,
    /// 已自动继续次数（含重试）。
    pub count: u32,
    /// 迭代上限。
    pub max: u32,
    /// 连续失败次数（仅用于展示与诊断）。
    pub consecutive_errors: u32,
    /// 待重试的原因（由 `tool_result` 收集）。
    pub pending_retry_reason: Option<String>,
    /// 常驻模式：`stop_sleep` 只把监控挂起，用户下一条真实输入自动恢复。
    pub keep_alive: bool,
    /// 常驻模式下被 `stop_sleep` 挂起（下一条用户输入可唤醒）。
    pub suspended: bool,
    /// `stop_sleep` 已被调用，等待 `turn_stopping` 消费后结束本轮。
    pub stop_requested: bool,
    /// 当前会话 ID（由 `subscribe(SessionStart)` 记录）。
    pub session_id: String,
    /// 自动审批配置（持久化在 `~/.phi/extensions/phi-sleep-continue/config.json`）。
    pub approval: ApprovalConfig,
    /// 自动审批配置路径。
    pub config_path: PathBuf,
    /// 会话级审批记录。
    pub approval_store: ApprovalStore,
    /// 最近一次工具调用命中的审批路由。
    pub last_route: Option<Route>,
    /// 最近一次被自动审批阻止的动作（供 `/sleep-approval approve` 精确放行）。
    pub last_denied: Option<crate::approval::ReviewSubject>,
    /// 配置解析告警（待命令展示）。
    pub config_warning: Option<String>,
}

impl Default for SleepState {
    fn default() -> Self {
        Self {
            enabled: false,
            continue_text: DEFAULT_CONTINUE_TEXT.to_string(),
            count: 0,
            max: DEFAULT_MAX,
            consecutive_errors: 0,
            pending_retry_reason: None,
            keep_alive: false,
            suspended: false,
            stop_requested: false,
            session_id: String::new(),
            approval: ApprovalConfig::default(),
            config_path: config::config_path(),
            approval_store: ApprovalStore::default(),
            last_route: None,
            last_denied: None,
            config_warning: None,
        }
    }
}

impl SleepState {
    /// 从环境变量构造初始状态（与 pi 版同名变量）。
    ///
    /// `enabled` 恒为 `false`：无人值守只能由 `/sleep-on` 显式开启，
    /// 环境变量无法开启，避免用户误配导致半夜自动烧 token。
    pub fn from_env() -> Self {
        let mut state = Self::default();
        state.reload_config();
        if let Ok(text) = std::env::var("PI_SLEEP_CONTINUE_TEXT") {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                state.continue_text = trimmed.to_string();
            }
        }
        if let Ok(max) = std::env::var("PI_SLEEP_CONTINUE_MAX") {
            if let Ok(parsed) = max.trim().parse::<u32>() {
                if parsed > 0 {
                    state.max = parsed;
                }
            }
        }
        state
    }

    /// 重新读取自动审批配置（文件缺失时沿用默认值）。
    pub fn reload_config(&mut self) {
        let (config, warning) = config::load(&self.config_path);
        self.approval = config;
        self.config_warning = warning;
    }

    /// 自动审批状态的一行摘要（用于 `/sleep-status`）。
    pub fn approval_summary(&self) -> String {
        let mode = match self.approval.mode {
            config::ApprovalMode::Safe => "safe",
            config::ApprovalMode::Permissive => "permissive",
        };
        let route = self.last_route.map_or("—", Route::name);
        let pending = self
            .last_denied
            .as_ref()
            .map_or("无".to_string(), |subject| subject.action_summary.clone());
        format!(
            "自动审批：{}（模式 {}）· 最近路由 {} · 已批准 {} · 连续拒绝 {} · 待放行 {}",
            if self.approval.enabled {
                "开 ✅"
            } else {
                "关 ❌"
            },
            mode,
            route,
            self.approval_store.approved_count(),
            self.approval_store.consecutive_denials(),
            pending
        )
    }

    /// 人手接管后重置预算。
    pub fn reset_budget(&mut self) {
        self.count = 0;
        self.consecutive_errors = 0;
        self.pending_retry_reason = None;
        self.stop_requested = false;
    }

    /// 开关状态的一行 footer 摘要。
    pub fn footer(&self) -> String {
        if !self.enabled {
            return String::new();
        }
        if self.suspended {
            format!("\u{1F319} 自动继续已挂起 {}/{}", self.count, self.max)
        } else {
            format!("\u{1F319} 自动继续 {}/{}", self.count, self.max)
        }
    }

    /// 组装一次自动催促的消息：固定触发行 + 用户追加的任务指令。
    ///
    /// 触发行永不缺席，负责告诉模型何时调用 `stop_sleep` 收尾；用户自定义的
    /// 继续文本只作为「任务指令」追加，不替换触发行语义。
    pub fn nudge_message(&self) -> String {
        format!("{TRIGGER_LINE}\n任务指令：{}", self.continue_text)
    }

    /// 记录一次 `stop_sleep` 调用，等待 `turn_stopping` 消费。
    pub fn request_stop(&mut self) {
        self.stop_requested = true;
    }

    /// `turn_stopping` 消费 `stop_sleep`。返回 `true` 表示本轮应直接结束。
    ///
    /// 常驻模式下仅挂起（等待下一条用户输入恢复）；非常驻模式彻底关闭。
    pub fn consume_stop(&mut self) -> bool {
        if !self.stop_requested {
            return false;
        }
        self.stop_requested = false;
        self.pending_retry_reason = None;
        if self.keep_alive {
            self.suspended = true;
        } else {
            self.enabled = false;
        }
        true
    }

    /// 真实用户输入接管：清除常驻挂起、重置预算。返回是否由挂起恢复。
    pub fn take_user_over(&mut self) -> bool {
        let resumed = self.suspended;
        self.suspended = false;
        self.reset_budget();
        resumed
    }
}

/// 扩展内部共享状态别名。
///
/// phi 的拦截/订阅回调签名是 `FnMut(Event) -> Option<Result> + 'static`，
/// 不带 `Context` 也不带 `&mut self`，因此跨回调状态统一收敛到
/// `Rc<RefCell<SleepState>>`，克隆进每个闭包。
pub type Shared = std::rc::Rc<std::cell::RefCell<SleepState>>;

/// 创建共享状态。
pub fn shared() -> Shared {
    std::rc::Rc::new(std::cell::RefCell::new(SleepState::from_env()))
}

/// 判断文本里是否出现某个恰好三位的状态码（`429` / `5xx` 等）。
///
/// 只匹配独立的 3 位数字串，因此 `4290`、`1429` 都不会误判。
fn has_status_code(text: &str, pred: impl Fn(u16) -> bool) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i - start == 3 {
            if let Ok(code) = text[start..i].parse::<u16>() {
                if pred(code) {
                    return true;
                }
            }
        }
    }
    false
}

/// `rate limit` 的常见拼写（对应 pi 版的 `/rate.?limit/i`）。
fn has_rate_limit(lower: &str) -> bool {
    ["rate limit", "ratelimit", "rate_limit", "rate-limit"]
        .iter()
        .any(|needle| lower.contains(needle))
}

/// 判断错误文本是否属于「值得重试」的瞬时故障。
///
/// 覆盖：限流（429）、上游 5xx、超时/连接重置/网络与 TLS 抖动，以及中文提示。
pub fn is_retryable(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    let lower = text.to_ascii_lowercase();
    if has_status_code(&lower, |code| code == 429 || (500..=599).contains(&code)) {
        return true;
    }
    if has_rate_limit(&lower) {
        return true;
    }
    const NEEDLES: &[&str] = &[
        "overloaded",
        "upstream",
        "timeout",
        "timed out",
        "etimedout",
        "econnreset",
        "econnrefused",
        "eai_again",
        "enotfound",
        "socket hang up",
        "fetch failed",
        "network",
        "tls",
        "ssl",
    ];
    if NEEDLES.iter().any(|needle| lower.contains(needle)) {
        return true;
    }
    // 仅保留“确实值得重试”的瞬时故障中文提示；「失败」「中断」等泛指
    // 太宽，会把普通业务错误也误判为可重试。
    const CJK: &[&str] = &[
        "暂时",
        "稍后",
        "重试",
        "超时",
        "无响应",
        "假死",
        "连接中断",
        "连接重置",
    ];
    CJK.iter().any(|needle| text.contains(needle))
}

/// 从提问工具入参中提取「每题首选项 = 推荐项」的摘要。
///
/// 入参形如 `{ "questions": [ { "question": "...", "options": [ {"label": "..."} ] } ] }`。
/// 无法识别时返回 `None`。
pub fn summarize_recommended_choices(input: &Value) -> Option<String> {
    let questions = input.get("questions")?.as_array()?;
    let mut lines = Vec::new();
    for question in questions {
        let text = question
            .get("question")
            .or_else(|| question.get("prompt"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let Some(options) = question.get("options").and_then(Value::as_array) else {
            continue;
        };
        let Some(first) = options.first() else {
            continue;
        };
        let label = first
            .get("label")
            .or_else(|| first.get("value"))
            .and_then(Value::as_str)
            .unwrap_or("");
        lines.push(format!("-「{text}」→ 已选推荐项「{label}」"));
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

/// 构造「拦截提问工具」时回给模型的 reason 文本。
pub fn question_block_reason(state: &SleepState, summary: Option<&str>) -> String {
    let choice = match summary {
        Some(summary) => format!("自动选择如下（每题首选项即推荐项）：\n{summary}"),
        None => "默认选择每题的第一个选项（推荐项）。".to_string(),
    };
    [
        "【无人值守模式】检测到向用户提问，已自动按推荐选项继续，无需等待用户点选。",
        &choice,
        &format!(
            "请按上述选择继续执行任务，不要再次调用提问工具；如确需人类决策，请在回复末尾注明“需要人工确认：...”然后停下（不要死循环追问）。当前自动继续进度 {}/{}。",
            state.count, state.max
        ),
    ]
    .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use phi_ext_common::json::json;

    #[test]
    fn is_retryable_should_match_429_and_5xx() {
        assert!(is_retryable("HTTP 429 Too Many Requests"));
        assert!(is_retryable("upstream returned 503"));
        assert!(is_retryable("status 599"));
    }

    #[test]
    fn is_retryable_should_not_match_embedded_or_out_of_range_codes() {
        assert!(!is_retryable("port 1429 is open"));
        assert!(!is_retryable("id 4290 not found"));
        assert!(!is_retryable("status 404"));
        assert!(!is_retryable("all good"));
    }

    #[test]
    fn is_retryable_should_match_network_and_cjk_keywords() {
        assert!(is_retryable("socket hang up"));
        assert!(is_retryable("Rate Limit exceeded"));
        assert!(is_retryable("ECONNRESET"));
        assert!(is_retryable("请求超时，请稍后重试"));
        assert!(is_retryable("上游服务暂时不可用"));
    }

    #[test]
    fn is_retryable_should_treat_empty_as_not_retryable() {
        assert!(!is_retryable(""));
    }

    #[test]
    fn summarize_should_pick_first_option_per_question() {
        let input = json!({
            "questions": [
                { "question": "用哪个数据库？", "options": [ {"label": "PostgreSQL"}, {"label": "MySQL"} ] },
                { "prompt": "是否继续？", "options": [ {"value": "是"}, {"value": "否"} ] }
            ]
        });
        let summary = summarize_recommended_choices(&input).expect("应提取到摘要");
        assert!(summary.contains("用哪个数据库？"));
        assert!(summary.contains("PostgreSQL"));
        assert!(summary.contains("是否继续？"));
        assert!(summary.contains("是"));
        assert!(!summary.contains("MySQL"));
    }

    #[test]
    fn summarize_should_return_none_without_questions() {
        assert!(summarize_recommended_choices(&json!({})).is_none());
        assert!(summarize_recommended_choices(&json!({"questions": []})).is_none());
        assert!(summarize_recommended_choices(&json!({"questions": [{"options": []}]})).is_none());
    }

    #[test]
    fn question_block_reason_should_include_progress() {
        let state = SleepState {
            count: 3,
            max: 10,
            ..SleepState::default()
        };
        let reason = question_block_reason(&state, Some("-「Q」→ 已选推荐项「A」"));
        assert!(reason.contains("无人值守模式"));
        assert!(reason.contains("已选推荐项「A」"));
        assert!(reason.contains("3/10"));
    }

    #[test]
    fn reset_budget_should_clear_counters() {
        let mut state = SleepState {
            count: 5,
            consecutive_errors: 2,
            pending_retry_reason: Some("x".into()),
            ..SleepState::default()
        };
        state.reset_budget();
        assert_eq!(state.count, 0);
        assert_eq!(state.consecutive_errors, 0);
        assert!(state.pending_retry_reason.is_none());
    }

    #[test]
    fn footer_should_be_empty_when_disabled() {
        let mut state = SleepState::default();
        assert!(state.footer().is_empty());
        state.enabled = true;
        state.count = 2;
        assert!(state.footer().contains("2/100"));
    }

    #[test]
    fn default_state_should_be_disabled_and_bounded() {
        let state = SleepState::default();
        assert!(!state.enabled);
        assert_eq!(state.continue_text, DEFAULT_CONTINUE_TEXT);
        assert_eq!(state.max, DEFAULT_MAX);
        assert!(!state.keep_alive);
        assert!(!state.suspended);
    }

    #[test]
    fn nudge_message_should_keep_trigger_line_and_append_instruction() {
        let state = SleepState {
            continue_text: "按计划推进".into(),
            ..SleepState::default()
        };
        let message = state.nudge_message();
        assert!(message.starts_with(TRIGGER_LINE));
        assert!(message.contains("任务指令：按计划推进"));
    }

    #[test]
    fn consume_stop_should_disable_in_once_mode() {
        let mut state = SleepState {
            enabled: true,
            count: 3,
            pending_retry_reason: Some("x".into()),
            ..SleepState::default()
        };
        state.request_stop();
        assert!(state.consume_stop());
        assert!(!state.enabled);
        assert!(!state.suspended);
        assert!(state.pending_retry_reason.is_none());
        // 第二次消费不应再返回 true。
        assert!(!state.consume_stop());
    }

    #[test]
    fn consume_stop_should_suspend_in_keep_mode() {
        let mut state = SleepState {
            enabled: true,
            keep_alive: true,
            ..SleepState::default()
        };
        state.request_stop();
        assert!(state.consume_stop());
        assert!(state.enabled);
        assert!(state.suspended);
        assert!(state.footer().contains("挂起"));
    }

    #[test]
    fn take_user_over_should_resume_and_reset() {
        let mut state = SleepState {
            enabled: true,
            keep_alive: true,
            suspended: true,
            count: 4,
            ..SleepState::default()
        };
        assert!(state.take_user_over());
        assert!(!state.suspended);
        assert_eq!(state.count, 0);
        // 未挂起时返回 false，但仍会重置预算。
        state.count = 2;
        assert!(!state.take_user_over());
        assert_eq!(state.count, 0);
    }
}
