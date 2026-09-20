//! 扩展运行期共享状态。
//!
//! phi 的拦截/订阅回调不带 `Context`（无法 notify / set_status），因此所有跨回调
//! 状态收敛到 `Rc<RefCell<Runtime>>`。回调无法访问宿主的完整消息历史，本扩展维护
//! 一份自己观测到的消息视图（用户输入 + 工具调用/结果）。

use std::collections::{BTreeSet, HashMap};

use phi_ext_common::arena::Scratch;
use phi_ext_common::config as cfg;

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

    /// 记录一次工具结果。
    pub fn record_tool_result(&mut self, tool_call_id: &str, tool_name: &str, content: &str) {
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
        let id = self.next_id();
        self.messages.push(CoreMessage {
            id,
            role: Role::Tool,
            content_type: ContentType::ToolResult,
            text: Some(cleaned),
            tool_name: Some(tool_name.to_string()),
            tool_call_id: Some(tool_call_id.to_string()),
            thinking_tokens: None,
        });
        self.active_tool_calls.remove(tool_call_id);
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

    /// 运行内核管线，并把结果状态写回。
    pub fn process(&mut self) -> crate::types::ProcessTurnOutcome {
        let config = self.kernel_config();
        let token_count = self.effective_token_count();
        let strategy = self.render_strategy();
        let outcome =
            compress::process_turn(&self.messages, &self.state, &config, token_count, strategy);
        self.state = outcome.state.clone();
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
        runtime.record_tool_result("c1", "read", "file contents");
        assert_eq!(runtime.messages.len(), 3);
        assert!(runtime.estimate_tokens() > 0);
    }

    #[test]
    fn record_tool_result_should_strip_ansi() {
        let mut runtime = Runtime::new();
        runtime.reset_session();
        runtime.record_tool_result("c1", "bash", "\u{1b}[31merror\u{1b}[0m: boom");
        assert_eq!(runtime.messages[0].text_str(), "error: boom");
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
