//! 扩展运行期共享状态。
//!
//! phi 的拦截/订阅回调不带 `Context`（无法 notify / set_status），因此所有跨回调
//! 状态收敛到 `Rc<RefCell<Runtime>>`。回调无法访问宿主的完整消息历史，本扩展维护
//! 一份自己观测到的消息视图（用户输入 + 工具调用/结果）。

use std::collections::{BTreeSet, HashMap};

use phi_ext_common::arena::Scratch;
use phi_ext_common::config as cfg;

use crate::absorb;
use crate::compress;
use crate::config::{self, AcpConfig};
use crate::render::RenderStrategy;
use crate::types::{CompressRangeSpec, CompressionState, ContentType, CoreMessage, Role};

/// 扩展内部共享状态别名。
pub type Shared = phi_ext_common::Shared<Runtime>;

/// 扩展运行期状态。
pub struct Runtime {
    /// 扩展配置。
    pub config: AcpConfig,
    /// 压缩状态。
    pub state: CompressionState,
    /// 观测到的消息视图。
    pub messages: Vec<CoreMessage>,
    /// 下一个观测消息 id。
    next_message_seq: u64,
    /// 进行中的工具调用：toolCallId → 消息 id。
    active_tool_calls: HashMap<String, String>,
    /// 连续提醒次数（防止死循环）。
    pub consecutive_nudges: u32,
    /// 待展示给用户的告警（回调无法 notify，只能缓存）。
    pending_notices: Vec<String>,
    /// 复用的竞技场：单次压缩管线内的临时内存。
    scratch: Scratch,
    /// 本会话是否已注入过压缩契约。
    ///
    /// 契约会被拼进用户消息并永久留在会话历史里，因此**每会话只需一次**；
    /// 每轮重复注入就是持续推大上下文。
    contract_injected: bool,
}

impl Runtime {
    /// 创建运行时并加载配置与状态。
    pub fn new() -> Self {
        let config = config::load();
        let state = load_state();
        Self {
            config,
            state,
            messages: Vec::new(),
            next_message_seq: 1,
            active_tool_calls: HashMap::new(),
            consecutive_nudges: 0,
            pending_notices: Vec::new(),
            scratch: Scratch::with_capacity(16 * 1024),
            contract_injected: false,
        }
    }

    /// 内核配置（每次由扩展配置派生）。
    pub fn kernel_config(&self) -> crate::types::Config {
        self.config.to_kernel_config()
    }

    /// 渲染策略。
    pub fn render_strategy(&self) -> RenderStrategy {
        self.config.render_strategy()
    }

    /// 竞技场（供单次调用临时分配）。
    pub fn scratch(&mut self) -> &mut Scratch {
        &mut self.scratch
    }

    /// 取走本会话需要注入的压缩契约（已注入过则返回空串）。
    ///
    /// 只在 `before_agent_start` 调用。宿主每次会把它拼到用户消息尾部，
    /// 该文本就永久留在会话历史里——所以每会话只发一次。
    pub fn take_contract_injection(&mut self, contract: &str) -> String {
        if self.contract_injected {
            return String::new();
        }
        self.contract_injected = true;
        contract.to_string()
    }

    /// 记录一条用户输入。
    pub fn record_user_input(&mut self, text: &str) {
        let id = self.next_id();
        self.messages
            .push(CoreMessage::text(id, Role::User, text.to_string()));
    }

    /// 记录一次工具调用。
    pub fn record_tool_call(&mut self, tool_call_id: &str, tool_name: &str, input: &str) {
        let id = self.next_id();
        self.active_tool_calls
            .insert(tool_call_id.to_string(), id.clone());
        self.messages.push(CoreMessage {
            id,
            role: Role::Assistant,
            content_type: ContentType::ToolCall,
            text: Some(input.to_string()),
            tool_name: Some(tool_name.to_string()),
            tool_call_id: Some(tool_call_id.to_string()),
            thinking_tokens: None,
        });
    }

    /// 记录一次工具结果，返回需要回写给宿主的替换文本（absorb 命中时）。
    ///
    /// 返回 `Some` 时调用方**必须**把 `phi::ToolResultResult.content` 设为
    /// 该文本：这是 phi 宿主上唯一能把内容从上游请求里真正去掉的通道
    /// （见 [`crate::absorb`] 模块说明）。不返回则模型仍会看到全量输出。
    pub fn record_tool_result(
        &mut self,
        tool_call_id: &str,
        tool_name: &str,
        content: &str,
        is_error: bool,
    ) -> Option<String> {
        // 工具输出常带大量 ANSI 颜色码（构建 / 测试 / git）。在进入观测视图前
        // 剥掉它们：既降低 token 估算，也避免下游摘要被转义码污染。
        // 剥离结果先落在复用的竞技场里（单次调用的临时内存），再转成 owned。
        let cleaned = if phi_ext_common::ansi::has_ansi(content) {
            let stripped = {
                let arena = self.scratch.arena();
                phi_ext_common::arena::strip_ansi(arena, content).to_string()
            };
            self.scratch.finish();
            stripped
        } else {
            content.to_string()
        };

        // absorb：把巨型输出换成「头 + 尾 + 标记」。命中时同时更新观测视图，
        // 否则 `estimate_tokens` 会系统性高估上下文（视图留着全量文本，
        // 而上游只收到 stub）。
        let usage = self.current_usage();
        let config = self.config.to_kernel_config().absorb.unwrap_or_default();
        let plan = absorb::plan_absorb(tool_name, &cleaned, is_error, usage, &config);

        let (text, replacement) = match plan {
            Some(plan) => {
                self.state.stats.absorbed_tokens += plan.reclaimed_tokens();
                let replacement = plan.text.clone();
                (plan.text, Some(replacement))
            }
            None => (cleaned, None),
        };

        let id = self.next_id();
        self.messages.push(CoreMessage {
            id,
            role: Role::Tool,
            content_type: ContentType::ToolResult,
            text: Some(text),
            tool_name: Some(tool_name.to_string()),
            tool_call_id: Some(tool_call_id.to_string()),
            thinking_tokens: None,
        });
        self.active_tool_calls.remove(tool_call_id);
        replacement
    }

    fn next_id(&mut self) -> String {
        let id = format!("msg{}", self.next_message_seq);
        self.next_message_seq += 1;
        id
    }

    /// 估算当前观测视图的 token 数。
    pub fn estimate_tokens(&self) -> u64 {
        self.messages
            .iter()
            .map(crate::tokenize::count_message_tokens)
            .sum()
    }

    /// 生效的上下文 token 数：优先取宿主会话文件里的真实值，回退到本地估算。
    ///
    /// 宿主把每次 completion 的 usage 持久化到会话 JSONL（见
    /// [`crate::session_tokens`]），这是扩展唯一能拿到真实 token 的途径；
    /// 取不到时（如本地模型不上报 usage）退回 [`Self::estimate_tokens`]。
    pub fn effective_token_count(&self) -> u64 {
        if self.config.use_host_tokens {
            if let Some(tokens) = crate::session_tokens::read_context_tokens() {
                return tokens;
            }
        }
        self.estimate_tokens()
    }

    /// 当前上下文使用率（未知时 0）。
    ///
    /// absorb 的使用率门槛用它。为了避免每 turn 重复读会话文件，只在启用门槛
    /// （`absorbContextThresholdPct > 0`）时才真的去取数。
    fn current_usage(&self) -> f64 {
        if self.config.model_context_limit == 0 || self.config.absorb_context_threshold_pct <= 0.0 {
            return 0.0;
        }
        self.effective_token_count() as f64 / self.config.model_context_limit as f64
    }

    /// 运行内核管线，并把结果状态写回。
    pub fn process(&mut self) -> crate::types::ProcessTurnOutcome {
        let config = self.kernel_config();
        let token_count = self.effective_token_count();
        let strategy = self.render_strategy();
        let mut outcome =
            compress::process_turn(&self.messages, &self.state, &config, token_count, strategy);
        // 内核已产出 owned 的新状态，直接移入运行时，省掉一次全量深拷贝
        // （blocks / message_refs 随会话增长，每 turn 复制代价可观）。
        self.state = std::mem::take(&mut outcome.state);
        outcome
    }

    /// 应用一批压缩范围。
    pub fn apply(&mut self, ranges: &[CompressRangeSpec]) -> crate::types::ApplyCompressionOutcome {
        let config = self.kernel_config();
        let before = self.state.blocks.len();
        let outcome =
            compress::apply_compression(ranges, &self.messages, &self.state, &config, None);
        if outcome.result.blocks_created > 0 {
            self.state = outcome.state.clone();
            let summary = compress::created_blocks_summary(&self.state, before);
            if !summary.is_empty() {
                self.pending_notices.push(summary);
            }
            self.persist();
        }
        outcome
    }

    /// 追加一条待展示告警。
    pub fn push_notice(&mut self, notice: impl Into<String>) {
        self.pending_notices.push(notice.into());
    }

    /// 取走待展示告警。
    pub fn take_notices(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_notices)
    }

    /// 新会话开始时的重置。
    ///
    /// 只重置提醒计数与契约注入标记；**不**清空观测视图 / 压缩状态（无法可靠
    /// 区分「新会话」与「会话内新 turn」，显式清空请用 `/acp reset`）。
    pub fn on_session_start(&mut self) {
        self.reset_nudges();
        // 新会话要把契约重新注入一次。
        self.contract_injected = false;
    }

    /// 记录已提醒（返回 false 表示达到上限，应放行停止）。
    pub fn note_nudge(&mut self) -> bool {
        self.consecutive_nudges += 1;
        self.consecutive_nudges <= self.config.max_consecutive_nudges
    }

    /// 重置提醒计数（成功压缩或新用户输入时调用）。
    pub fn reset_nudges(&mut self) {
        self.consecutive_nudges = 0;
    }

    /// 持久化状态（原子写）。
    pub fn persist(&self) {
        let _ = cfg::save_atomic(&config::state_path(), &self.state);
    }

    /// 保存配置。
    pub fn persist_config(&self) -> Result<(), cfg::ConfigError> {
        config::save(&self.config)
    }

    /// 清空观测视图与压缩状态。
    pub fn reset_session(&mut self) {
        self.messages.clear();
        self.active_tool_calls.clear();
        self.next_message_seq = 1;
        self.state = crate::state::create_initial_state();
        self.consecutive_nudges = 0;
        self.contract_injected = false;
        self.persist();
    }

    /// 当前活跃块覆盖的消息 id 集合。
    pub fn covered_ids(&self) -> BTreeSet<String> {
        crate::state::covered_message_ids(&self.state)
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

/// 加载持久化状态（缺失或损坏时返回初始状态）。
fn load_state() -> CompressionState {
    cfg::load_or_default(&config::state_path())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_should_grow_message_view() {
        let mut runtime = Runtime::new();
        runtime.reset_session();
        runtime.record_user_input("hello");
        runtime.record_tool_call("c1", "read", "{\"path\":\"x\"}");
        runtime.record_tool_result("c1", "read", "file contents", false);
        assert_eq!(runtime.messages.len(), 3);
        assert!(runtime.estimate_tokens() > 0);
    }

    /// 回归：契约会永久留在会话历史里，每会话只能注入一次。
    #[test]
    fn contract_should_be_injected_once_per_session() {
        let mut runtime = Runtime::new();
        runtime.reset_session();
        let first = runtime.take_contract_injection("CONTRACT");
        assert_eq!(first, "CONTRACT");
        assert!(runtime.take_contract_injection("CONTRACT").is_empty());
        // 新会话重新开放一次。
        runtime.on_session_start();
        assert_eq!(runtime.take_contract_injection("CONTRACT"), "CONTRACT");
    }

    #[test]
    fn record_tool_result_should_strip_ansi() {
        let mut runtime = Runtime::new();
        runtime.reset_session();
        runtime.record_tool_result("c1", "bash", "\u{1b}[31merror\u{1b}[0m: boom", false);
        assert_eq!(runtime.messages[0].text_str(), "error: boom");
    }

    #[test]
    fn absorb_should_shrink_large_tool_result_and_return_replacement() {
        let mut runtime = Runtime::new();
        runtime.reset_session();
        runtime.config.absorb_enabled = true;
        runtime.config.absorb_min_tool_tokens = 100;
        let big = format!("HEAD{}TAIL", "x".repeat(40_000));
        let replacement = runtime.record_tool_result("c1", "bash", &big, false);
        let replacement = replacement.expect("巨型输出应被吸收");
        // 回写的文本必须就是观测视图里那份（否则估算会与上游错位）。
        assert_eq!(runtime.messages[0].text_str(), replacement);
        assert!(replacement.len() < big.len());
        assert!(runtime.state.stats.absorbed_tokens > 0);
    }

    #[test]
    fn absorb_should_leave_small_and_error_results_alone() {
        let mut runtime = Runtime::new();
        runtime.reset_session();
        runtime.config.absorb_enabled = true;
        runtime.config.absorb_min_tool_tokens = 100;
        assert!(runtime
            .record_tool_result("c1", "read", "tiny", false)
            .is_none());
        let big = "x".repeat(40_000);
        assert!(runtime
            .record_tool_result("c2", "bash", &big, true)
            .is_none());
        // 已吸收过的内容幂等。
        let first = runtime.record_tool_result("c3", "bash", &big, false);
        assert!(first.is_some());
    }

    #[test]
    fn nudge_counter_should_cap() {
        let mut runtime = Runtime::new();
        runtime.reset_session();
        runtime.config.max_consecutive_nudges = 2;
        assert!(runtime.note_nudge());
        assert!(runtime.note_nudge());
        assert!(!runtime.note_nudge());
        runtime.reset_nudges();
        assert!(runtime.note_nudge());
    }
}
