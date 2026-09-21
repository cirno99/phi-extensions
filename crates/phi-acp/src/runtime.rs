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
    /// 是否允许把状态写回磁盘。
    ///
    /// 单测里必须关掉：`Runtime::new()` 读的是用户**真实**的 config/state 路径，
    /// 测试一旦触发 `persist()` 就会把真实 `state.json` 覆盖成空状态
    /// （实测：跑一次 `cargo test -p phi-acp` 就把用户的块全部抹掉）。
    persist_enabled: bool,
    /// 自上次落盘以来状态是否变化（避免每 turn 无谓写盘）。
    dirty: bool,
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
            persist_enabled: true,
            dirty: false,
        }
    }

    /// 创建一个**不落盘**的运行时（单测专用）。
    ///
    /// 单测必须用它：`Runtime::new()` 指向用户真实的 state.json，测试里任何
    /// `persist()` 都会把用户的压缩块抹掉。这个方法读到的状态依然是真实的，
    /// 只是永不写回。
    #[cfg(test)]
    pub fn new_isolated() -> Self {
        let mut runtime = Self::new();
        runtime.persist_enabled = false;
        // 连可逆吸收的原文也不要落到用户真实的 `state/absorbed` 目录。
        crate::absorb_store::set_enabled(false);
        runtime
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
        // 使用率未知（−1）时按「刚过门槛」处理：既不因为拿不到数就彻底放弃
        // 回收（那会让上下文无上限堆积），也不因为误判而一次性拉到最激进窗口。
        let raw_usage = self.current_usage();
        let usage = if raw_usage < 0.0 {
            self.config.absorb_context_threshold_pct.max(0.0)
        } else {
            raw_usage
        };
        let config = self.config.to_kernel_config().absorb.unwrap_or_default();
        // 预分配句柄：stub 里带上它，模型可用 `acp_decompress <handle>` 取回原文。
        // 未命中吸收时该计数器不落盘（dirty 未置位），因此不会凭空消耗句柄。
        let handle = crate::state::allocate_absorb_id(&mut self.state);
        // 只有在原文仓库可用时才向模型承诺「可取回」；否则回退到旧措辞。
        let reversible = crate::absorb_store::is_enabled();
        let plan = absorb::plan_absorb(
            tool_name,
            &cleaned,
            is_error,
            usage,
            &config,
            &handle,
            reversible,
        );

        let (text, replacement) = match plan {
            Some(plan) => {
                // 可逆吸收：原文落盘（失败则退化为旧行为——stub 仍带句柄，
                // 但 `acp_decompress` 会如实报告“已过期/未找到”）。
                crate::absorb_store::store(&plan.handle, &cleaned);
                self.remember_absorbed(&plan, tool_name);
                self.state.stats.absorbed_tokens += plan.reclaimed_tokens();
                // 只标脏、不立即写盘：工具结果是每 turn 最热的回调，逐个原子写
                // 会把磁盘打满。真正的落盘交给 `persist_if_dirty`（turn_stopping）。
                self.dirty = true;
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

    /// 记下一次可逆吸收，并在超过上限时淘汰最旧的条目（同时删掉其磁盘原文）。
    ///
    /// 上限存在的意义：句柄账本与磁盘文件会随会话无限增长，而真正会被回头
    /// 查阅的巨型输出通常只有最近几十条。
    fn remember_absorbed(&mut self, plan: &absorb::AbsorbPlan, tool_name: &str) {
        self.state.absorbed_outputs.push(crate::types::AbsorbedOutput {
            handle: plan.handle.clone(),
            tool_name: tool_name.to_string(),
            tokens: plan.original_tokens,
            created_at: crate::time_now_ms(),
        });
        while self.state.absorbed_outputs.len() > crate::absorb_store::MAX_ENTRIES {
            let evicted = self.state.absorbed_outputs.remove(0);
            crate::absorb_store::remove(&evicted.handle);
        }
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

    /// 当前上下文使用率；**未知时返回负数**。
    ///
    /// absorb 的门槛与自适应强度都用它。区分「未知」与「真的空」很关键：
    /// 拿不到宿主真值且本地视图为空时（如会话刚开、或本地模型不上报 usage），
    /// 使用率是未知而非 0——把它当成 0 会让门槛永远拦住一切，absorb 完全失效。
    ///
    /// 为避免每 turn 重复读会话文件，`read_context_tokens` 自带增量缓存（只读
    /// 本 turn 新增的字节），所以这里可以放心调用。
    fn current_usage(&self) -> f64 {
        if self.config.model_context_limit == 0 {
            return -1.0;
        }
        if self.messages.is_empty() && crate::session_tokens::read_context_tokens().is_none() {
            return -1.0;
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
    ///
    /// 单测创建的运行时（[`Self::new_isolated`]）会静默跳过，避免污染用户的
    /// 真实 `state.json`。
    pub fn persist(&self) {
        if !self.persist_enabled {
            return;
        }
        let _ = cfg::save_atomic(&config::state_path(), &self.state);
    }

    /// 标记状态已变化，等待下次 [`Self::persist`] 落盘。
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// 若状态有变化则落盘，并清掉脏标记。
    ///
    /// 用在每 turn 都会经过的钩子里：absorb 统计这类「慢慢累加」的字段
    /// 需要定期落盘，但不能每 turn 无条件写盘。
    pub fn persist_if_dirty(&mut self) {
        if self.dirty {
            self.persist();
            self.dirty = false;
        }
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
        // 块账本连同可逆吸收的原文一起丢弃：`/acp reset` 的语义就是「忘掉一切」。
        crate::absorb_store::clear();
        self.consecutive_nudges = 0;
        self.contract_injected = false;
        self.persist();
    }

    /// 宿主原生压缩（`runCompact`）后的重新同步。
    ///
    /// # 为什么必须处理
    ///
    /// 宿主压缩是 phi 上**唯一**能把上下文真正变小的事件：它把旧历史换成一段
    /// 摘要、只保留最近若干消息（`internal/session/compaction` 的 `keepRecentTokens`）。
    /// 扩展拿不到「保留了哪些消息」，但能确定一件事——**旧消息已从上游请求里
    /// 消失**。而本扩展的观测视图此前一直把它们留在内存里，于是 `/acp status`
    /// 的 token 估算、可压缩范围与 ref 索引都会系统性地指向已经不存在的内容，
    /// 这正是「压缩效果差」的一个来源。
    ///
    /// # 做法
    ///
    /// 清空观测视图（旧消息已随宿主历史消失）与 token 快照，重新注入一次契约
    /// （它可能已随被摘要的旧历史一起消失），但**保留块账本**——块摘要是被压
    /// 内容的唯一记录，仍可 `acp_search` / `acp_decompress`。`next_message_seq`
    /// 与 `message_refs` 不动：让新消息拿到递增的新 ref，避免与历史块里的旧 ref 撞号。
    pub fn on_host_compaction(&mut self) {
        self.messages.clear();
        self.active_tool_calls.clear();
        self.state.token_snapshot.clear();
        // 契约可能已随被摘要的旧历史一起消失，下个 agent start 重新注入一次。
        self.contract_injected = false;
        self.reset_nudges();
        self.mark_dirty();
        self.persist_if_dirty();
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

    /// absorb 测试必须固定使用率门槛与 token 来源，否则 `absorbContextThresholdPct`
    /// 默认值（0.30）会让小视界被门槛拦住、`useHostTokens` 会去读真实会话文件。
    fn absorb_runtime() -> Runtime {
        let mut runtime = Runtime::new_isolated();
        runtime.reset_session();
        runtime.config.absorb_enabled = true;
        runtime.config.absorb_context_threshold_pct = 0.0;
        runtime.config.use_host_tokens = false;
        runtime
    }

    #[test]
    fn record_should_grow_message_view() {
        let mut runtime = Runtime::new_isolated();
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
        let mut runtime = Runtime::new_isolated();
        runtime.reset_session();
        let first = runtime.take_contract_injection("CONTRACT");
        assert_eq!(first, "CONTRACT");
        assert!(runtime.take_contract_injection("CONTRACT").is_empty());
        // 新会话重新开放一次。
        runtime.on_session_start();
        assert_eq!(runtime.take_contract_injection("CONTRACT"), "CONTRACT");
    }

    /// 宿主压缩后必须重新同步：观测视图清空、契约重注入，但块账本保留。
    #[test]
    fn host_compaction_should_resync_view_but_keep_blocks() {
        let mut runtime = Runtime::new_isolated();
        runtime.reset_session();
        runtime.record_user_input("hello");
        // 块账本是压缩内容的唯一记录，压缩后仍应可检索。
        runtime.state.blocks.push(crate::types::CompressionBlock {
            block_id: "b1".into(),
            summary: "kept summary".into(),
            active: true,
            ..Default::default()
        });
        runtime.take_contract_injection("CONTRACT");

        runtime.on_host_compaction();

        assert!(runtime.messages.is_empty(), "观测视图应清空");
        assert_eq!(runtime.state.blocks.len(), 1, "块账本必须保留");
        // 契约可重新注入（可能已随被摘要的旧历史消失）。
        assert_eq!(runtime.take_contract_injection("CONTRACT"), "CONTRACT");
        // ref 分配不回退：新消息的 id 继续递增，避免与历史块的旧 ref 撞号。
        runtime.record_user_input("after compact");
        assert_ne!(runtime.messages[0].id, "msg1");
    }

    #[test]
    fn record_tool_result_should_strip_ansi() {
        let mut runtime = Runtime::new_isolated();
        runtime.reset_session();
        runtime.record_tool_result("c1", "bash", "\u{1b}[31merror\u{1b}[0m: boom", false);
        assert_eq!(runtime.messages[0].text_str(), "error: boom");
    }

    #[test]
    fn absorb_should_shrink_large_tool_result_and_return_replacement() {
        let mut runtime = absorb_runtime();
        runtime.config.absorb_min_tool_tokens = 100;
        let big = format!("HEAD{}TAIL", "x".repeat(40_000));
        let replacement = runtime.record_tool_result("c1", "bash", &big, false);
        let replacement = replacement.expect("巨型输出应被吸收");
        // 回写的文本必须就是观测视图里那份（否则估算会与上游错位）。
        assert_eq!(runtime.messages[0].text_str(), replacement);
        assert!(replacement.len() < big.len());
        assert!(runtime.state.stats.absorbed_tokens > 0);
    }

    /// 可逆吸收：stub 必须带句柄，句柄被登记进 `absorbed_outputs`，且原文能从
    /// 仓库逐字取回（模型用 `acp_decompress <handle>`，不必重跑工具）。
    #[test]
    fn absorb_should_register_reversible_handle_in_stub() {
        let dir = std::env::temp_dir().join(format!(
            "phi-acp-reversible-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        let mut runtime = absorb_runtime();
        // 注意顺序：`absorb_runtime()` 内部的 `new_isolated()` 会关掉仓库，
        // 因此必须在它之后再打开并指向临时目录。
        crate::absorb_store::set_dir_override(Some(dir.clone()));
        crate::absorb_store::set_enabled(true);
        runtime.config.absorb_min_tool_tokens = 100;
        let big = format!("HEAD{}TAIL", "x".repeat(40_000));
        let replacement = runtime
            .record_tool_result("c1", "bash", &big, false)
            .expect("巨型输出应被吸收");

        assert_eq!(runtime.state.absorbed_outputs.len(), 1);
        let handle = runtime.state.absorbed_outputs[0].handle.clone();
        assert!(handle.starts_with('a'), "句柄应形如 aN：{handle}");
        assert!(replacement.contains(&handle), "stub 应引用句柄");
        assert!(replacement.contains("acp_decompress"));
        assert_eq!(runtime.state.absorbed_outputs[0].tool_name, "bash");
        // 原文必须能从仓库逐字取回。
        assert_eq!(crate::absorb_store::load(&handle).as_deref(), Some(big.as_str()));

        crate::absorb_store::set_enabled(false);
        crate::absorb_store::set_dir_override(None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 原文仓库不可用时，stub 必须回退到旧措辞，不向模型承诺一个取不回来的句柄。
    #[test]
    fn absorb_without_store_should_not_promise_a_handle() {
        let mut runtime = absorb_runtime();
        runtime.config.absorb_min_tool_tokens = 100;
        let big = format!("HEAD{}TAIL", "x".repeat(40_000));
        let replacement = runtime
            .record_tool_result("c1", "bash", &big, false)
            .expect("巨型输出应被吸收");
        assert!(!replacement.contains("acp_decompress"));
        assert!(replacement.contains("re-run the tool"));
    }

    #[test]
    fn absorb_should_leave_small_and_error_results_alone() {
        let mut runtime = absorb_runtime();
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

    /// 使用率低于门槛时不动手：这是「上下文先自然长大再回收」的波动前提。
    ///
    /// 关掉「巨型输出例外」以单独验证门槛本身（默认 2000 token 的例外会在
    /// 低水位下也吸走这条 40K 字符 / ~10K token 的输出）。
    #[test]
    fn absorb_should_respect_context_threshold() {
        let mut runtime = absorb_runtime();
        runtime.config.absorb_min_tool_tokens = 100;
        runtime.config.absorb_context_threshold_pct = 0.5;
        runtime.config.absorb_always_above_tokens = 0;
        runtime.config.model_context_limit = 1_000_000;
        let big = "x".repeat(40_000);
        // 观测视图很小 ⇒ 使用率远低于 50% ⇒ 不吸收。
        assert!(runtime
            .record_tool_result("c1", "bash", &big, false)
            .is_none());
    }

    #[test]
    fn nudge_counter_should_cap() {
        let mut runtime = Runtime::new_isolated();
        runtime.reset_session();
        runtime.config.max_consecutive_nudges = 2;
        assert!(runtime.note_nudge());
        assert!(runtime.note_nudge());
        assert!(!runtime.note_nudge());
        runtime.reset_nudges();
        assert!(runtime.note_nudge());
    }

    /// 回归：单测绝不能写用户的真实 state.json。
    ///
    /// 历史事故：`Runtime::new()` 指向 `<phi_home>/extensions/phi-acp/state/
    /// state.json`（用户真实文件），测试里的 `reset_session()` / `apply()`
    /// 会调 `persist()`，于是跑一次 `cargo test` 就把用户的压缩块全部抹平。
    /// 这里用一份哨兵状态验证隔离运行时不会落地。
    #[test]
    fn isolated_runtime_should_never_touch_user_state_file() {
        let path = crate::config::state_path();
        // 记下用户文件的现状（可能不存在）。
        let before = std::fs::read(&path).ok();

        let mut runtime = Runtime::new_isolated();
        runtime.reset_session();
        runtime.mark_dirty();
        runtime.persist_if_dirty();
        runtime.persist();

        let after = std::fs::read(&path).ok();
        assert_eq!(before, after, "隔离运行时不得改写用户 state.json");
    }

    /// absorb 命中只标脏；落盘由 `persist_if_dirty` 完成。
    #[test]
    fn absorb_should_mark_state_dirty() {
        let mut runtime = Runtime::new_isolated();
        runtime.config.absorb_enabled = true;
        runtime.config.absorb_context_threshold_pct = 0.0;
        runtime.config.use_host_tokens = false;
        runtime.config.absorb_min_tool_tokens = 100;
        runtime.reset_session();

        assert!(!runtime.dirty, "初始不应是脏的");
        let big = "x".repeat(40_000);
        assert!(runtime
            .record_tool_result("c1", "bash", &big, false)
            .is_some());
        assert!(runtime.dirty, "absorb 命中后应标脏");
        assert!(runtime.state.stats.absorbed_tokens > 0);

        runtime.persist_if_dirty();
        assert!(!runtime.dirty, "落盘后脏标记应清除");
    }
}
