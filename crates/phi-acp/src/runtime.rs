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
    /// 观测视图的 token 总数（增量维护，避免每次全量重算）。
    view_tokens: u64,
    /// 进行中的工具调用：toolCallId → 消息 id。
    active_tool_calls: HashMap<String, String>,
    /// 连续提醒次数（防止死循环）。
    pub consecutive_nudges: u32,
    /// 待展示给用户的告警（回调无法 notify，只能缓存）。
    pending_notices: Vec<String>,
    /// 复用的竞技场：单次压缩管线内的临时内存。
    scratch: Scratch,
    /// 当前状态归属的会话键（`None` = 还认不出来，见 [`crate::session`]）。
    ///
    /// 键未知时**不落盘**：宁可这次不写，也不要把新会话的状态写进上一个会话的
    /// 文件（那会毁掉那个会话的块账本——它是被压内容唯一的记录）。
    session_key: Option<String>,
    /// 被判定为「上一个会话」的键。
    ///
    /// `/new` 之后会话文件是**惰性创建**的（phi 在第一条消息时才 `os.Create`），
    /// 于是在新文件出现之前，[`crate::session::current_session_key`] 会反复
    /// 指向上一个会话。若不记住并拒绝它，[`Self::refresh_session_key`] 就会
    /// 把新会话又接到旧账本上——bug 原样复现。
    stale_key: Option<String>,
    /// 本会话是否已注入过压缩契约。
    ///
    /// 契约会被拼进用户消息并永久留在会话历史里，因此**每会话只需一次**；
    /// 每轮重复注入就是持续推大上下文。
    contract_injected: bool,
    /// 上次注入给宿主的持久规则渲染文本（空串表示「当前无规则」）。
    ///
    /// 规则会随系统提示词注入，而 phi 把它拼进用户消息并永久留在历史里；
    /// 因此只在**内容变化**时重发，而不是每轮重发（见 [`Self::take_prompt_append`]）。
    rules_injected: String,
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
    ///
    /// 状态**按会话**加载（见 [`crate::session`]）：进程启动时会话文件通常已存在，
    /// 因此能直接定位到正确的账本；若还认不出来（`SessionStart` 尚未到达、
    /// 或会话文件尚未落盘），先拿初始状态，等能认出来时再切。
    pub fn new() -> Self {
        let config = config::load();
        let session_key = crate::session::current_session_key();
        let state = load_state_for(session_key.as_deref(), true);
        crate::absorb_store::set_session_key(session_key.clone());
        Self::assemble(config, state, session_key)
    }

    /// 组装运行时。
    fn assemble(config: AcpConfig, state: CompressionState, session_key: Option<String>) -> Self {
        Self {
            config,
            state,
            messages: Vec::new(),
            view_tokens: 0,
            active_tool_calls: HashMap::new(),
            consecutive_nudges: 0,
            pending_notices: Vec::new(),
            scratch: Scratch::with_capacity(16 * 1024),
            session_key,
            stale_key: None,
            contract_injected: false,
            rules_injected: String::new(),
            persist_enabled: true,
            dirty: false,
        }
    }

    /// 创建一个**不落盘**的运行时（单测专用）。
    ///
    /// 单测必须用它：`Runtime::new()` 指向用户真实的 state.json，测试里任何
    /// `persist()` 都会把用户的压缩块抹掉。
    ///
    /// 这里连用户的**会话目录也不读**（`session_key = None`）：`Runtime::new()`
    /// 会去解析当前会话 id，而单测进程并不是会话进程，解析出来的可能是别的
    /// 项目的会话；更危险的是 [`load_state_for`] 会把旧版全局 `state.json`
    /// **改名**进会话目录——那是破坏性的，绝不能由测试触发。
    #[cfg(test)]
    pub fn new_isolated() -> Self {
        // 会话身份也隔离掉：默认实现会去读用户真实的会话目录，单测会因此变得
        // 非确定性（取决于跑测试时机器上恰好有哪些会话）。
        crate::session::set_key_override(Some(None));
        let mut runtime =
            Self::assemble(config::load(), crate::state::create_initial_state(), None);
        runtime.persist_enabled = false;
        // 连可逆吸收的原文也不要落到用户真实的 `state/absorbed` 目录。
        crate::absorb_store::set_enabled(false);
        runtime
    }

    /// 当前状态归属的会话键（`None` = 还没认出来）。
    pub fn session_key(&self) -> Option<&str> {
        self.session_key.as_deref()
    }

    /// 单测用：注入会话键（不落盘，不影响 `session_key` 之外的任何东西）。
    #[cfg(test)]
    pub fn set_session_key_for_test(&mut self, key: Option<&str>) {
        self.session_key = key.map(str::to_string);
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

    /// 取走本会话需要注入的提示词附加段（压缩契约 + 持久规则）。
    ///
    /// 只在 `before_agent_start` 调用。宿主每次会把它拼到**用户消息尾部**，
    /// 该文本就永久留在会话历史里——因此：
    /// - 契约每会话只发一次；
    /// - 规则只在「内容变化 / 新会话 / 宿主压缩后」注入。规则条数少、变化稀疏，
    ///   每轮重复注入等于每轮永久 +N token，正是本扩展要避免的反模式。
    pub fn take_prompt_append(&mut self, contract: &str, rules: &str) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if !self.contract_injected {
            self.contract_injected = true;
            if !contract.is_empty() {
                parts.push(contract);
            }
        }
        if rules != self.rules_injected {
            self.rules_injected = rules.to_string();
            if !rules.is_empty() {
                parts.push(rules);
            }
        }
        parts.join("\n\n")
    }

    /// 记录一条用户输入。
    pub fn record_user_input(&mut self, text: &str) {
        let id = self.next_id();
        self.view_tokens += crate::tokenize::count_tokens(text);
        self.messages
            .push(CoreMessage::text(id, Role::User, text.to_string()));
    }

    /// 记录一次工具调用。
    pub fn record_tool_call(&mut self, tool_call_id: &str, tool_name: &str, input: &str) {
        let id = self.next_id();
        self.view_tokens += crate::tokenize::count_tokens(input);
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
        let config = self.config.to_absorb_config();
        // 只在真正命中吸收时才向模型承诺「可取回」；否则回退到旧措辞。
        // 句柄编号直接传计数器值，由 `plan_absorb` 在命中后才格式化成字符串
        // （未命中路径不再每条工具结果都分配一个句柄）。
        let reversible = crate::absorb_store::is_enabled();
        let plan = absorb::plan_absorb(
            tool_name,
            &cleaned,
            is_error,
            usage,
            &config,
            self.state.next_absorb_id,
            reversible,
        );

        let (text, replacement) = match plan {
            Some(plan) => {
                // 命中：提交句柄计数器（未命中路径不消耗句柄）。
                crate::state::commit_absorb_id(&mut self.state);
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
        self.view_tokens += crate::tokenize::count_tokens(&text);
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

    /// 分配一条观测消息的**原始 id**。
    ///
    /// 计数器存在 [`CompressionState`] 里（不是运行时字段）：它必须跨进程重启
    /// 保持单调，否则新消息会拿到已经用过的原始 id，从而在
    /// [`crate::refs::assign_refs`] 那里被误认为「已分配过」而继承旧 ref。
    fn next_id(&mut self) -> String {
        let seq = self.state.next_message_seq.max(1);
        self.state.next_message_seq = seq + 1;
        format!("msg{seq}")
    }

    /// 记下一次可逆吸收，并在超过上限时淘汰最旧的条目（同时删掉其磁盘原文）。
    ///
    /// 上限存在的意义：句柄账本与磁盘文件会随会话无限增长，而真正会被回头
    /// 查阅的巨型输出通常只有最近几十条。
    fn remember_absorbed(&mut self, plan: &absorb::AbsorbPlan, tool_name: &str) {
        self.state
            .absorbed_outputs
            .push(crate::types::AbsorbedOutput {
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
        self.view_tokens
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
        // 把状态**移入**内核管线（而不是传引用让它内部深拷贝）：内核按值接收、
        // 就地修改，省掉每 turn 两次全量状态拷贝（blocks / message_refs 随会话
        // 增长，每轮复制代价可观）；返回后再把新状态移回运行时。
        let state = std::mem::take(&mut self.state);
        let mut outcome =
            compress::process_turn(&self.messages, state, &config, token_count, strategy);
        self.state = std::mem::take(&mut outcome.state);
        outcome
    }

    /// 应用一批压缩范围。
    pub fn apply(&mut self, ranges: &[CompressRangeSpec]) -> crate::types::ApplyCompressionOutcome {
        let config = self.kernel_config();
        let before = self.state.blocks.len();
        let mut outcome =
            compress::apply_compression(ranges, &self.messages, &self.state, &config, None);
        if outcome.result.blocks_created > 0 {
            self.state = std::mem::take(&mut outcome.state);
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

    /// 新会话开始。
    ///
    /// # 旧版的错（已修）
    ///
    /// 早先这里只重置提醒计数与注入标记，理由是「无法可靠区分新会话与会话内新
    /// turn」。**那个理由不成立**：`SessionStart` 是与 `TurnStart` 分开的事件，
    /// 而且带着 `reason`（`startup` / `resume` / `new`）。于是 `/new` 之后，上一个
    /// 会话的观测视图、块账本与 ref 索引全部被继承下来：`acp_status` 报出宿主
    /// 历史里不存在的可压缩范围，模型照着压缩必然被 ref 门拒绝——就是「压缩失败 /
    /// 无作用」的一个直接来源。
    ///
    /// # 现在
    ///
    /// 会话键变了就换账本（[`crate::session`] 说明了键从哪来）。
    ///
    /// - `reason == "new"`：宿主历史为空，**一定**是换会话。即使键还解析不出来
    ///   （会话文件惰性创建，此刻看到的是上一个会话的文件），也必须把内存状态
    ///   重置并把 `session_key` 置空——**置空后不落盘**，否则会把新会话的空状态
    ///   写进上一个会话的文件，毁掉它的账本。
    /// - 其它 reason：拿解析出的键与当前键比对，不同才切。
    pub fn on_session_start(&mut self, reason: &str, previous_session_id: &str) {
        self.reset_nudges();
        // 新会话要把契约重新注入一次。
        self.contract_injected = false;
        // 规则同理：新会话历史里还没有它们。
        self.rules_injected.clear();

        let detected = crate::session::current_session_key();
        if reason == "new" {
            // `/new` 是权威信号（宿主历史为空）：文件探测此刻可能还指着上一个
            // 会话（新会话文件尚未落盘），因此要能识别出「探测到的是刚离开的
            // 会话」，而不能照单全收。
            let leaving = if previous_session_id.is_empty() {
                None
            } else {
                Some(crate::session::sanitize_key(previous_session_id))
            };
            let stale = (detected.is_some() && detected == self.session_key)
                || (leaving.is_some() && detected == leaving);
            if stale {
                self.switch_session(None);
            } else {
                self.switch_session(detected);
            }
            return;
        }
        // 非 `/new` 路径：只在**确实解析出**会话键、且与当前不同时才换账本。
        // 解析失败（cwd 探测抖动等）绝不能当作「换会话」——那会把内存里的账本
        // 丢掉，而 `stale_key` 还会阻止它被重新采纳。
        if let Some(key) = detected {
            if self.session_key.as_deref() != Some(key.as_str()) {
                self.switch_session(Some(key));
            }
        }
    }

    /// 换到另一个会话的账本（`None` = 新会话但键还认不出来）。
    fn switch_session(&mut self, next: Option<String>) {
        // 旧账本先落盘（可能还有未持久化的 absorb 统计）。
        self.persist();
        self.stale_key = self.session_key.take();
        self.session_key = next.clone();
        crate::absorb_store::set_session_key(next.clone());
        self.reset_view();
        // 键未知时拿**初始**状态：绝不能退回旧全局 `state.json`，那正是要修的 bug。
        self.state = match next.as_deref() {
            Some(key) => load_state_for(Some(key), false),
            None => crate::state::create_initial_state(),
        };
        // 键未知时 `persist()` 直接返回——这是故意的：否则会把新会话的空状态
        // 写进**上一个**会话的文件，毁掉它的块账本（被压内容唯一的记录）。
        self.persist();
    }

    /// 会话键未知时尝试解析；解析到就接到该会话的账本上。
    ///
    /// 用在 [`Self::persist_if_dirty`]：`/new` 之后新会话文件要等第一条消息
    /// 落盘才出现，而 `turn_stopping` 已是那之后，因此能自愈。
    fn refresh_session_key(&mut self) {
        if self.session_key.is_some() {
            return;
        }
        let Some(key) = crate::session::current_session_key() else {
            return;
        };
        // 拒绝重新采纳刚被判定为「上一个会话」的键（文件探测滞后时它会反复出现）。
        if self.stale_key.as_deref() == Some(key.as_str()) {
            return;
        }
        self.session_key = Some(key.clone());
        crate::absorb_store::set_session_key(Some(key.clone()));
        // 只在磁盘上确实有一份账本时才覆盖内存状态：`/new` 后的第一 turn 可能
        // 已经在内存里建了块，而那时文件还不存在（惰性创建）。
        let loaded = load_state_for(Some(&key), false);
        if !loaded.blocks.is_empty() {
            self.state = loaded;
        }
    }

    /// 丢弃属于**上一个**会话的观测视图与待展示告警。
    ///
    /// ref 是每会话的：新会话的消息从 `m00001` 重新编号，与它自己的块账本对齐。
    fn reset_view(&mut self) {
        self.messages.clear();
        self.view_tokens = 0;
        self.active_tool_calls.clear();
        self.consecutive_nudges = 0;
        self.contract_injected = false;
        self.rules_injected.clear();
        self.pending_notices.clear();
        self.dirty = false;
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
    /// 写入**当前会话**的状态文件（`state/sessions/<会话键>.json`，见
    /// [`crate::session`]）。
    ///
    /// 两种情况静默跳过：
    /// - 单测创建的运行时（[`Self::new_isolated`]）——避免污染用户真实状态；
    /// - 会话键未知（`/new` 之后、新会话文件落盘之前）——宁可本次不写，
    ///   也不能把新会话的空状态写进上一个会话的文件。
    pub fn persist(&self) {
        if !self.persist_enabled {
            return;
        }
        let Some(key) = self.session_key.as_deref() else {
            return;
        };
        let _ = cfg::save_atomic(&config::session_state_path(key), &self.state);
    }

    /// 标记状态已变化，等待下次 [`Self::persist`] 落盘。
    ///
    /// 顺手解析一次会话键：状态一旦有变化就不该因为「键还没认出来」而丢失，
    /// 而走到这里时宿主必然已经写过消息（会话文件已存在）。
    pub fn mark_dirty(&mut self) {
        self.refresh_session_key();
        self.dirty = true;
    }

    /// 若状态有变化则落盘，并清掉脏标记。
    ///
    /// 用在每 turn 都会经过的钩子里：absorb 统计这类「慢慢累加」的字段
    /// 需要定期落盘，但不能每 turn 无条件写盘。
    ///
    /// 顺手解析会话键：`/new` 之后新会话文件要到第一条消息落盘才出现，
    /// 而本方法在 `turn_stopping` 里调用，已经是那之后，因此能自愈。
    pub fn persist_if_dirty(&mut self) {
        self.refresh_session_key();
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
        self.view_tokens = 0;
        self.active_tool_calls.clear();
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
    /// 与 `message_refs` 不动：让新消息拿到递增的新原始 id 与新 ref，
    /// 避免与历史块里的旧 ref 撞号。
    pub fn on_host_compaction(&mut self) {
        self.messages.clear();
        self.view_tokens = 0;
        self.active_tool_calls.clear();
        self.state.token_snapshot.clear();
        // 契约可能已随被摘要的旧历史一起消失，下个 agent start 重新注入一次。
        self.contract_injected = false;
        // 规则同理（它们可能也已随旧历史消失）。
        self.rules_injected.clear();
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

/// 加载指定会话的持久化状态（缺失或损坏时返回初始状态）。
///
/// `key` 为 `None`（认不出会话）时退回旧版的全局 `state.json`，保证
/// 认不出会话时也不至于完全丢失账本。
///
/// `adopt_legacy` 只应为 `true` 一次——即进程启动时（[`Runtime::new`]）：
/// 它会将旧版全局 `state.json` **改名**进会话目录（见 [`adopt_legacy_state`]）。
/// 单测必须传 `false`，否则跑一次测试就把用户真实的旧状态文件搬走了。
fn load_state_for(key: Option<&str>, adopt_legacy: bool) -> CompressionState {
    let Some(key) = key else {
        return cfg::load_or_default(&config::state_path());
    };
    let path = config::session_state_path(key);
    if adopt_legacy && !path.exists() {
        adopt_legacy_state(&path);
    }
    cfg::load_or_default(&path)
}

/// 把旧版的全局 `state.json` 采纳为本会话的账本。
///
/// # 为什么
///
/// 升级到「每会话状态」的用户，磁盘上只有旧版的全局 `state.json`。不采纳的话，
/// 他的块账本（被压内容**唯一**的记录）就凭空消失了。
///
/// # 为什么是改名而不是复制
///
/// 复制的话，每个新会话都会再采纳一次同一个旧文件——新会话又继承旧账本，
/// 正好是本次要修掉的 bug。改名后旧文件不复存在，采纳只会发生一次。
fn adopt_legacy_state(target: &std::path::Path) {
    let legacy = config::state_path();
    if !legacy.exists() {
        return;
    }
    if std::fs::create_dir_all(config::sessions_state_dir()).is_err() {
        return;
    }
    let _ = std::fs::rename(&legacy, target);
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
        let first = runtime.take_prompt_append("CONTRACT", "");
        assert_eq!(first, "CONTRACT");
        assert!(runtime.take_prompt_append("CONTRACT", "").is_empty());
        // 新会话重新开放一次。
        runtime.on_session_start("startup", "");
        assert_eq!(runtime.take_prompt_append("CONTRACT", ""), "CONTRACT");
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
        runtime.take_prompt_append("CONTRACT", "");

        runtime.on_host_compaction();

        assert!(runtime.messages.is_empty(), "观测视图应清空");
        assert_eq!(runtime.state.blocks.len(), 1, "块账本必须保留");
        // 契约可重新注入（可能已随被摘要的旧历史消失）。
        assert_eq!(runtime.take_prompt_append("CONTRACT", ""), "CONTRACT");
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
        assert_eq!(
            crate::absorb_store::load(&handle).as_deref(),
            Some(big.as_str())
        );

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

    /// 回归：未命中吸收不得消耗句柄，否则句柄号会出现空洞（旧实现在判定前就
    /// 推进了 `nextAbsorbId`，注释却声称「未命中不会凭空消耗句柄」）。
    #[test]
    fn absorb_miss_should_not_consume_a_handle() {
        let mut runtime = absorb_runtime();
        runtime.config.absorb_min_tool_tokens = 100_000; // 门槛高到必然未命中
        assert!(runtime
            .record_tool_result("c1", "bash", "tiny output", false)
            .is_none());
        assert_eq!(
            runtime.state.next_absorb_id, 1,
            "未命中不得推进计数器（初始值为 1）"
        );

        runtime.config.absorb_min_tool_tokens = 100;
        let big = "x".repeat(40_000);
        let replacement = runtime
            .record_tool_result("c2", "bash", &big, false)
            .expect("应吸收");
        assert!(replacement.len() < big.len(), "命中应折叠为 stub");
        assert_eq!(
            runtime.state.next_absorb_id, 2,
            "首次命中应消费 a1（计数器 1 → 2），而不是被跳过后的 a3"
        );
    }

    fn test_block(id: &str) -> crate::types::CompressionBlock {
        crate::types::CompressionBlock {
            block_id: id.to_string(),
            summary: format!("summary of {id}"),
            active: true,
            ..Default::default()
        }
    }

    /// 回归：`/new` 绝不能继承上一个会话的观测视图与块账本。
    ///
    /// 历史症状：新会话里 `acp_status` 报出宿主机历史里**不存在**的可压缩范围，
    /// 模型照着这些 ref 调 `compress` 必然被 ref 门拒绝——表面症状就是
    /// 「压缩失败 / 无作用」。
    #[test]
    fn new_session_must_not_inherit_the_previous_ledger() {
        let mut runtime = Runtime::new_isolated();
        runtime.set_session_key_for_test(Some("sess-old"));
        runtime.state.blocks.push(test_block("b1"));
        runtime.record_user_input("old session message");
        assert_eq!(runtime.messages.len(), 1);

        // `/new` 时新会话文件尚未落盘（惰性创建），探测结果 == 刚离开的会话。
        crate::session::set_key_override(Some(Some("sess-old".to_string())));
        runtime.on_session_start("new", "sess-old");

        assert!(runtime.messages.is_empty(), "观测视图必须清空");
        assert!(runtime.state.blocks.is_empty(), "块账本必须重置");
        assert_eq!(runtime.session_key(), None, "键未知期间不得落盘");
        assert_eq!(
            runtime.state.next_message_seq, 1,
            "ref 是每会话的，新会话重新编号"
        );
    }

    /// 回归：键未知时**绝不**重新采纳刚离开的会话。
    ///
    /// 会话文件是惰性创建的，于是 `/new` 之后一段时间里
    /// [`crate::session::current_session_key`] 会反复指向上一个会话；不挡住它，
    /// 新会话就又接到旧账本上，bug 原样复现。
    #[test]
    fn unkeyed_session_must_not_re_adopt_the_stale_key() {
        let mut runtime = Runtime::new_isolated();
        runtime.set_session_key_for_test(Some("sess-old"));
        runtime.state.blocks.push(test_block("b1"));
        crate::session::set_key_override(Some(Some("sess-old".to_string())));
        runtime.on_session_start("new", "sess-old");
        assert_eq!(runtime.session_key(), None);

        // 新会话文件还没出现：解析结果仍是旧会话 → 必须拒绝。
        runtime.mark_dirty();
        runtime.persist_if_dirty();
        assert_eq!(runtime.session_key(), None, "不得把旧会话的账本接回来");
        assert!(runtime.state.blocks.is_empty(), "不得恢复旧块");

        // 新会话文件出现后，才接上它自己的账本。
        crate::session::set_key_override(Some(Some("sess-new".to_string())));
        runtime.mark_dirty();
        runtime.persist_if_dirty();
        assert_eq!(runtime.session_key(), Some("sess-new"));
    }

    /// `startup` 且会话键变了（重启进的是另一个会话）→ 换账本。
    #[test]
    fn startup_with_a_different_key_should_switch_the_ledger() {
        let mut runtime = Runtime::new_isolated();
        runtime.set_session_key_for_test(Some("sess-a"));
        runtime.state.blocks.push(test_block("b1"));
        runtime.record_user_input("from session a");

        crate::session::set_key_override(Some(Some("sess-b".to_string())));
        runtime.on_session_start("startup", "");

        assert!(runtime.messages.is_empty(), "观测视图属于上一个会话");
        assert!(runtime.state.blocks.is_empty(), "块账本属于上一个会话");
        assert_eq!(runtime.session_key(), Some("sess-b"));
    }

    /// `startup` 且会话键没变（重启后回到同一会话）→ 保留账本。
    #[test]
    fn startup_with_the_same_key_should_keep_the_ledger() {
        let mut runtime = Runtime::new_isolated();
        runtime.set_session_key_for_test(Some("sess-a"));
        runtime.state.blocks.push(test_block("b1"));

        crate::session::set_key_override(Some(Some("sess-a".to_string())));
        runtime.on_session_start("startup", "");

        assert_eq!(runtime.state.blocks.len(), 1, "同一会话必须保留块账本");
        assert_eq!(runtime.session_key(), Some("sess-a"));
    }

    /// 回归：会话键未知时 `persist()` 必须什么都不写——尤其不得写旧的全局
    /// `state.json`（那是别的会话的账本）。
    #[test]
    fn persist_without_a_session_key_must_not_touch_the_legacy_file() {
        let path = crate::config::state_path();
        let before = std::fs::read(&path).ok();

        let mut runtime = Runtime::new_isolated();
        runtime.persist_enabled = true;
        runtime.set_session_key_for_test(None);
        runtime.persist();

        assert_eq!(
            std::fs::read(&path).ok(),
            before,
            "键未知时不得写旧全局 state.json"
        );
    }

    /// 回归：原始消息 id 计数器必须跨重启保持单调。
    ///
    /// 计数器不落盘的话，重启后从 1 重新数，新消息会拿到**已经用过的**原始 id；
    /// [`crate::refs::assign_refs`] 见到 `byRaw` 里已有该 id 就跳过分配
    /// （「首次分配后永不重分配」），新消息于是默默继承了旧消息的 ref——
    /// 模型按 ref 压缩时压到的是另一段内容。
    #[test]
    fn raw_message_ids_must_not_be_recycled_across_a_restart() {
        let mut runtime = Runtime::new_isolated();
        runtime.record_user_input("first");
        runtime.record_user_input("second");
        assert_eq!(runtime.messages[0].id, "msg1");
        assert_eq!(runtime.messages[1].id, "msg2");

        // 模拟进程重启：状态从磁盘读回（这里复用同一份），运行时字段全部重建。
        let reloaded = runtime.state.clone();
        let mut restarted = Runtime::new_isolated();
        restarted.state = reloaded;
        restarted.record_user_input("after restart");
        assert_eq!(
            restarted.messages[0].id, "msg3",
            "重启后不得重用 msg1 / msg2"
        );
    }

    /// 计数器必须真的进状态文件（而不是只活在内存里）。
    #[test]
    fn next_message_seq_should_round_trip_through_serde() {
        let mut state = crate::state::create_initial_state();
        state.next_message_seq = 7;
        let json = phi_ext_common::json::to_vec(&state).expect("序列化");
        let back: CompressionState = phi_ext_common::json::parse(&json).expect("反序列化");
        assert_eq!(back.next_message_seq, 7);
    }

    /// 老状态文件没有这个字段 → 默认 1（与修复前的行为一致，不会更糟）。
    #[test]
    fn legacy_state_without_the_counter_should_default_to_one() {
        let back: CompressionState =
            phi_ext_common::json::parse_str(r#"{"nextBlockId":1,"nextRunId":1}"#)
                .expect("反序列化");
        assert_eq!(back.next_message_seq, 1);
    }

    /// 回归：解析不出会话键时（cwd 探测抖动等）**不得**当作换会话——
    /// 那会丢掉内存里的账本，而 `stale_key` 还会阻止它被重新采纳。
    #[test]
    fn unresolvable_key_must_not_be_treated_as_a_session_change() {
        let mut runtime = Runtime::new_isolated();
        runtime.set_session_key_for_test(Some("sess-a"));
        runtime.state.blocks.push(test_block("b1"));
        runtime.record_user_input("still session a");

        // 探测失败：返回 None。
        crate::session::set_key_override(Some(None));
        runtime.on_session_start("resume", "sess-a");

        assert_eq!(runtime.session_key(), Some("sess-a"), "键不该被清掉");
        assert_eq!(runtime.state.blocks.len(), 1, "账本不该被丢掉");
        assert_eq!(runtime.messages.len(), 1, "观测视图不该被清掉");
    }
}
