# phi-extensions

把 pi (TypeScript) 扩展重写为 **phi (Rust) 扩展**。扩展通过 PXB
（Phi eXtension Binary）协议与宿主通信，使用官方 `phi-ext` SDK。

## 扩展一览

| crate | 来源（pi 扩展） | 说明 |
|---|---|---|
| [`phi-rtk-optimizer`](crates/phi-rtk-optimizer) | `pi-rtk-optimizer.js` | RTK 命令重写（`rtk rewrite` 子进程）+ 工具输出压缩（ansi/build/test/git/linter/search/source/truncate） |
| [`phi-sleep-continue`](crates/phi-sleep-continue) | `sleep-continue.js` | 无人值守自动继续 + 提问拦截 + 失败重试，并参照 [pi-auto-approval](https://github.com/Europa2061/pi-auto-approval) 增加规则化自动审批 |
| [`phi-cache-optimizer`](crates/phi-cache-optimizer) | `pi-cache-optimizer.js` | 缓存优化配置 + 能力诊断（可落地子集，见下） |
| [`phi-acp`](crates/phi-acp) | `billion-context` + `acp-kernel` | ACP 上下文压缩：模型驱动、三级 LSM、可解压/可检索（合并移植，见下） |
| [`phi-deepseek-enhanced`](crates/phi-deepseek-enhanced) | `deepseek-enhanced.ts` | We-need 推理风格锚点 + `str_replace_editor` 工具 + Eternal Minimal 运行时守卫（可落地子集，见下） |
| [`phi-ast-grep`](crates/phi-ast-grep) | `pi-ast-grep` | AST 感知的代码搜索与改写（`ast_grep_search` / `ast_grep_replace`），宿主需自备 ast-grep 二进制 |
| [`phi-ext-common`](crates/phi-ext-common) | — | 共享工具库：路径、配置原子读写、ANSI、文本截断、用量统计、竞技场分配器、jemalloc |

> `pi-statusline` **未移植**：phi 宿主已自带状态栏。其中两项纯计算已补进
> `phi-ext-common::stats`——**缓存命中率**（`cache_hit_rate`）与
> **token 速率**（`tokens_per_second` / `format_rate`，单位 `tps`）。

## 安装

```bash
scripts/install.sh          # 构建 release 并安装到 ~/.phi/extensions/
scripts/install.sh --debug  # 构建 debug
```

安装后按 `Ctrl+K → extensions → reload`，或重启 phi。

手动安装：把 `target/release/<name>` 与 `crates/<name>/phi.yaml` 放到
`~/.phi/extensions/<name>/` 即可。

## 使用

### rtk-optimizer

| 入口 | 说明 |
|---|---|
| `/rtk show\|path\|verify\|stats\|clear-stats\|reset\|help` | 配置与诊断 |

配置：`~/.phi/extensions/phi-rtk-optimizer/config.json`（字段与 pi 版一致，
含 `mode` / `outputCompaction.*`，旧配置缺少 `readCompaction` 时按旧默认解释）。

### sleep-continue

| 入口 | 说明 |
|---|---|
| `/sleep-on [文本]` `/sleep-off` `/sleep-set <文本>` `/sleep-max <N>` `/sleep-mode [once\|keep]` `/sleep-status` | 自动继续 |
| `/sleep-approval status\|on\|off\|safe\|permissive\|allow <模式>\|deny <模式>\|allowlist <命令>\|approve\|clear\|reset` | 自动审批（默认**开**，模式 `permissive`） |

配置：`~/.phi/extensions/phi-sleep-continue/config.json`。
**说明**：
- 自动审批默认开启、模式 `permissive`：只拦你显式 `deny` 的动作，不会误伤 `cargo test` 等常用命令；需要更紧的护栏时用 `/sleep-approval safe`。
- 每次催促带固定触发行（告诉模型「任务完成后调用 `stop_sleep` 收尾」），用户文本作为「任务指令」追加；`stop_sleep` 在 `once` 模式彻底关闭、在 `keep` 模式仅挂起（下一条真实输入自动恢复）。

### cache-optimizer

| 入口 | 说明 |
|---|---|
| `/cache-optimizer status\|doctor\|enable\|disable\|config …\|stats\|reset\|help` | 配置与能力诊断 |

配置：`~/.phi/extensions/phi-cache-optimizer/config.json`。

### acp（上下文压缩）

把 [billion-context](https://github.com/ranxianglei/billion-context)（宿主插件）与
[acp-kernel](https://github.com/ranxianglei/acp-kernel)（压缩算法内核）**合并**为一个
扩展：内核（ref 分配、边界解析、三级 LSM 块、推荐/提醒、紧急截断、解压、检索）
逐条移植为纯 Rust 逻辑并完整单测；插件外壳适配为 phi 的钩子与工具。

| 入口 | 说明 |
|---|---|
| `compress` 工具 | 模型写摘要压缩一个 ref 区间；返回新建块账本（`bN=mAAAAA–mBBBBB`） |
| `acp_decompress` / `acp_search` | 恢复被压缩块的内容（默认只上溯一层：嵌套活跃子块显示摘要；`full:true` 递归到原始消息；**无状态**，块保持压缩、重复调用免费；内容 >10000 字符写入临时文件）或可逆吸收的原文（句柄 `aN`） / 按相关度检索**全部块（含失活）+ 已折叠的历史消息** |
| `acp_status` | 使用率、块统计（含 absorbed）与当前可压缩范围 |
| `acp_rule` | 记录永不压缩的持久规则（`add`/`list`/`remove`/`clear`；单条 ≤300 字符、最多 50 条、拒绝重复；带 `[ruleN]` 供删除） |
| `/acp status\|absorb\|compress\|enable\|disable\|config …\|rules\|reset\|help` | 状态面板、absorb 诊断、可压缩范围查看与配置 |

`before_agent_start` 每会话**只注入一次**精炼压缩契约；持久规则只在**内容变化 / 新会话 /
宿主压缩后**重发（两者都会被拼进用户消息并永久留在历史里，每轮重发 = 每轮永久 +N token）；
`turn_stopping` 按增长量发提醒（带连续上限防死循环）；`user_input` / `tool_call` /
`tool_result` 维护本扩展自己的消息观测视图。配置：`~/.phi/extensions/phi-acp/config.json`，
状态：`~/.phi/extensions/phi-acp/state/sessions/<会话 id>.json`（**每会话一个**，见下）。

**在 phi 上什么真正省 token**：`tool_result` 拦截里把巨型工具输出换成「头 + 尾 +
`[acp absorb]` 标记」并回写（`absorbEnabled`，默认开）——这是宿主上唯一能把内容从
上游请求里**真正去掉**的通道（与 rtk 同路）。`compress` 产生的块摘要做不到这一点：
phi 没有消息历史 / 请求体重写钩子，摘要只落在扩展自己的 `state.json` 里，用于给模型
提供 ref 索引与可检索的块账本。因此：

- 需要控制上下文大小 → 调 `absorb*`（`absorb-min-tokens` / `absorb-keep-prefix` /
  `absorb-keep-suffix` / `absorb-threshold-pct` / `absorb-always-above` / `absorb-exclude-tools`）。
- 想知道 absorb 到底回收了多少、谁在制造上下文 → `/acp absorb`（当前阈值、累计回收、按工具明细、句柄上限）。
- 需要写摘要时 → `compress` 仍然有价值（模型可 `acp_search` / `acp_decompress`）；`/acp compress` 只**展示**可压缩范围，不再主动让模型压缩（那条指令是净亏损）。

**状态按会话存（本轮修掉的第二个大缺口）**：上游 `billion-context` 的压缩状态是**按会话**
存的（`src/paths.ts` 的 `sessionsDir()`：*"Sessions dir: one JSON file per session"*；
`src/persist.ts` 落盘 `PersistedSession { id, state }`）。本扩展早先只有一个全局
`state.json`，于是**新会话会继承上一个会话的 ref 索引与块账本**：`acp_status` 报出宿主机
历史里根本不存在的可压缩范围，模型照着这些 ref 调 `compress` 必然被 ref 门拒绝——这正是
「压缩失败 / 无作用」的一个直接来源。现在状态落到 `state/sessions/<会话 id>.json`，
可逆吸收的原文也按会话分目录（句柄编号是每会话的，共用目录会让 `a1` 互相覆盖，
`acp_decompress` 返回**别的会话的内容**）。

会话 id 的来源值得记一笔：**phi 的 `SessionStart` 事件不带当前会话 id**
（`internal/extension/proc.go:755` 只转发 `Reason` 与 `PreviousSessionID`，
`ext.SessionStartEvent.SessionID` 被丢掉）。因此会话 id 从会话**文件名**解析
（`internal/session/manager.go:113` 的 `<timestamp>_<32位十六进制>.jsonl`）。
该文件是惰性创建的，所以 `/new` 之后一段时间文件探测仍指向上一个会话——
代码把 `reason == "new"` 当权威信号，并在键未知时**不落盘**（否则新会话的空状态会
覆盖掉上一个会话的账本，而那是被压内容唯一的记录）。旧版的全局 `state.json` 会在
首次启动时**改名**进会话目录（改名而非复制：否则每个新会话都会再采纳一次同一个旧文件）。

顺带修掉一个更隐蔽的 ref 错配：观测消息的**原始 id**（`msg1` / `msg2` …）此前是纯进程内
计数器，不落盘。重启后它从 1 重新数，新消息就拿到**已经用过的**原始 id；而
`assign_refs` 见到 `byRaw` 里已有该 id 就跳过分配（「首次分配后永不重分配」），于是新消息
默默继承了旧消息的 ref——模型按 ref 压缩时压到的是另一段内容。上游没有这个问题，因为
`CoreMessage.id` 由宿主提供、天然唯一；本扩展只能用计数器造，所以计数器进了状态文件
（`nextMessageSeq`）。

**检索引擎（换掉了上游的遗留路径）**：`acp_search` 此前是 acp-kernel
`compress.ts::scoreRelevance` 的逐字移植——即上游的**遗留**子串路径 `core.search`
（注意：`billion-context` 至今也还在用它，迁移计划是 acp-kernel issue #44）。上游在
`search/SEARCH.md` 里点名了它三处短板：只看**活跃**块（失活块里的历史再也搜不到）、只做
子串计数（`the` ≈ `theater`，不懂形态 / 错拼 / CJK 词边界）、不检索消息。现已整体移植上游
推荐的生产路径 `searchBlocks`（`src/search/` → `src/search/`：`tokenizer` / `stemmer` /
`doc_cache` / `registry` / `substring` / `bm25` / `fuzzy` / `hybrid`；默认 `hybrid` =
0.7·BM25(词干) + 0.3·fuzzy bigram，再按角色加权 user 1.5 / assistant 1.0 / tool 0.6 /
block 1.0）。语料是**全部块（含失活）+ 已折叠的历史消息**，消息命中会标出「压缩掉它的块」，
形成上游的 search → decompress 闭环。本地小语料差分基准：遗留子串 MRR 0.625 vs hybrid
1.000（上游 32 块 / 48 查询上是 0.797 vs 0.898）。

一处**有意偏差**：上游用 `Intl.Segmenter("zh", { granularity: "word" })`（ICU CLDR 词典）
切 CJK 词；Rust 没有内建等价物，而 acp-kernel 明确以「零运行时依赖」为设计前提，因此 CJK
一律用**重叠 bigram** 近似词条（多字段出 bigram，单字文本出单字）。后果：召回不减
（`身份验证` 仍命中 `身份验证流程`），但上游靠词典避免的 `试验证明` 误命中 `验证` **会发生**
——该偏差由 `tokenize_known_gap_cjk_false_hit_without_a_dictionary` 测试钉住，不会被误当成
「已修复」。上游的 `semantic`（embedding 余弦，异步、默认不注册）未移植：phi 扩展没有异步
检索通道。

**可逆吸收（与两个原版对齐的关键修复）**：原版的 absorb 是**可逆**的——被吸收
的工具输出仍留在宿主历史里，`decompress` 随时能取回，因此吸收只花上下文、不丢信息。
phi 上扩展拿不到历史，只能在 `tool_result` 拦截时替换模型看到的那条消息，旧实现把
中段直接丢弃（标记里写「re-run the tool if you need the missing middle」），模型回头
需要细节时只能重跑 `read` / `build`——代价往往比省下的 token 还大，这正是「效果远不如
原版」的核心原因之一。现在吸收改为可逆：原文落到 `state/absorbed/<handle>.txt`，stub
里带上句柄（如 `a12`），模型用 `acp_decompress a12` 即可逐字取回（`absorb-store` 不可用
时自动回退到旧措辞，不向模型承诺取不回来的句柄）。仓库上限 256 条，超出按插入顺序淘汰。

**为什么 `compress` 在 phi 上压不掉上下文（与 `billion-context` 的根本差异）**：
`billion-context` 是 HTTP 代理 / 原生插件，靠 `before_provider_request` 重写**真实请求体**；
`acp-kernel` 的 `processTurn` 返回被 `prune` 过的消息数组，由宿主发往上游。两者都能把
压缩后的历史**真正**从请求里删掉。phi 扩展协议只有 15 个钩子（`tool_call` / `tool_result` /
`before_agent_start` / `user_input` / `turn_stopping` / 只读事件），**既无请求体重写、也拿不到
消息历史**：内核只能在本扩展的「影子视图」上跑，`prune` 的结果被丢弃，块摘要只写进
`state.json`。因此在 phi 上唯一能减少上游 token 的通道就是 `tool_result` 回写（absorb），
`compress` 块只是可检索的账本。这不是移植遗漏，而是宿主能力边界——也是本扩展把
`compress` 与 absorb 分开、并把默认回收策略压在 absorb 上的原因。

**真正能让上下文变小的只有宿主压缩**：`internal/agent/engine.go` 在**没有工具调用**的
回合末尾（`len(msg.ToolCalls) == 0`）才调 `runCompact`，当
`contextTokens > context_window - 16384` 时保留约 20K 消息 + ≤13.1K 摘要、其余历史丢弃。
⚠️ **关键后果**：长时间的工具调用循环（助手每回合都发 tool call）永远不会进入这个分支，
因此**即使已远超 context_window 也不会触发原生压缩**，上下文单调上涨直到上游报
context-overflow（engine.go 的 overflow 恢复路径才会强制压缩）。想让上下文回落，必须
**用一个纯文本回复结束回合**。这也是 `/acp status` 在超限且无可压缩范围时提示「结束本回合」
的原因。扩展拿不到它保留了哪些消息，但能收到 `session_compact` 事件。收到后
其余历史丢弃。扩展拿不到它保留了哪些消息，但能收到 `session_compact` 事件。收到后
（`Runtime::on_host_compaction`）清空自己的观测视图与 token 快照（旧消息已从上游请求里
消失，继续留着会让 `/acp status` 的估算与可压缩范围指向已不存在的内容）、重新注入一次
契约，但**保留块账本**——块摘要是被压内容的唯一记录，仍可 `acp_search` / `acp_decompress`。
另外：`turn_stopping` 返回 `continue` 会**跳过**同轮的 `runCompact`，所以自动提醒默认关
（`autoNudgeEnabled`）。

**注入成本**：phi 会把 `SystemPromptAppend` 拼到**用户消息**后面并永久留在会话历史里
（`ext/go/types.go`："appended to the user message"），`turn_stopping` 的转向消息同样
被当作 user 消息 append。因此每轮注入的文本永远无法被压缩回收：契约改成每会话一次
（~180 token），提醒里的完整规则改成只在首次该层级携带。历史版本每轮注入 ~1.5K token
（四段提示词全文），比它压掉的还多。

**压缩频率调优**：扩展层对内核提醒阈值做了更积极的覆盖（可用 `/acp config` 调整）——
`growth-tokens=20000` / `min-growth-tokens=10000` / `min-context-pct=0.30` /
`max-context-pct=0.90` / `tier2-trigger=3` / `tier3-trigger=6`。相比内核默认
（阈值 50k、两次提醒间新增 22.5k、使用率 45% 才提醒），压缩触发得更频繁。

**摘要质量：相对体积上限**：`compress` 默认拒绝 token 数超过被压内容 50% 的摘要
（`maxSummaryRatio`，`/acp config max-summary-ratio` 可调，0 = 关闭）——摘要和被压内容
差不多大就不是压缩，只是把原文又写了一遍。此外 tier-1 规则里补了 SIZE TARGET，每会话
注入的契约也点明「是压缩不是重写」。

**`minCompressRange` 门槛的例外**：`compress` 默认拒绝字符数低于
`min-compress`（5000）的范围，但**当请求已覆盖当前可压缩内容的 ≥ 80% 时例外放行**——
此时「再合并更多消息」已凑不出多少，拒绝只会把可压缩内容永久搁置、上下文单调堆积。
因此只压「可压缩全集中的一小片」（覆盖率 < 80%）才会被拦（提示语此时才准确）。
阈值见 `compress::MIN_RANGE_COVERAGE_PCT`。

**巨型输出的强制吸收**：`absorb` 的使用率门槛负责「先长后收」的波动，但它不应成为
巨型输出的免死金牌——早期会话（水位远低于门槛）或高门槛配置下，一条几万 token 的
构建 / 测试日志会完整留在历史里，直到水位涨到门槛才被处理。`absorb-always-above`
（`absorbAlwaysAboveTokens`，默认 1500）让**任何** token 数 ≥ 该值的工具输出无论当前
水位多低都立即压成 stub；取 0 关闭该例外，退回纯门槛行为。

**与 pi 版的差异**：billion-context 在 pi 里是一个改基地址 + 改 `fetch` 的 HTTP
代理（`before_provider_request` 重写请求体）。phi **没有请求体钩子、也拿不到
消息历史**，因此代理层无法落地；本扩展保留内核与插件交互面，在自身观测到的
消息视图上运行内核，并用 `tool_result` 回写兑现真实收益（见上）。

### deepseek-enhanced

把 [Oh My Pi deepseek-enhanced.ts](https://github.com/mytianyi0712/DeepSeek-Enhanced-for-Oh-My-Pi) 移植为 phi 扩展。

| 入口 | 说明 |
|---|---|
| `str_replace_editor` 工具 | `view` / `create` / `str_replace` / `insert`，行为与 pi 版一致 |
| `/deepseek status\|on\|off\|anchor\|repeat\|every\|minimal\|transport\|strip\|reset\|help` | 配置与状态 |

`before_agent_start` 注入 “We need to …” 推理风格锚点并剥离用户消息里的
Today/cwd 系统提醒；`tool_call` 在 `minimal` 模式下阻止非核心工具直呼。
配置：`~/.phi/extensions/phi-deepseek-enhanced/config.json`。

**锚点为什么要每轮重复**：完整锚点只在会话首轮（以及上下文压缩后）注入一次，
随会话增长它会被后续对话历史淹没，而模型对「远离上下文尾部的指令」遵循度
衰减很快——这正是「很少能进入 We-need 思维链」的主因。因此 `anchor_repeat`
（默认开）让 `before_agent_start` 在其余轮次往当前用户消息**末尾**补一段极简
风格提醒，`anchor_repeat_every`（默认 1）控制间隔轮数；`/deepseek repeat off`
可退回「只在首轮注入」的旧行为。

**与 pi 版的差异**（phi 宿主能力缺失）：

- **拿不到模型信息**：PXB 子进程协议不向扩展推送 model，无法只对 DeepSeek 生效，
  改为全局开关。
- **无法收缩工具清单**：无 `GetAllTools` / `SetActiveTools` RPC，模型始终能看到全部
  工具；Eternal Minimal 退化为「运行时阻止直呼」，因此 `minimal` 默认关闭。
- **无法落地 `xd://` 网关**：扩展不能调用别的工具，read/write 的 `xd://` 语义是 OMP
  宿主内建能力。
- **无请求体钩子 / 拿不到消息历史 / 拿不到 assistant 推理文本**：provider 载荷过滤、
  thinking 与 256k token 上限、消息过滤、CoT 回归再注入均无法实现。
### ast-grep

把 [pi-ast-grep](https://github.com/code-yeongyu/pi-ast-grep) 移植为 phi 扩展：AST 感知的代码搜索与改写。

| 入口 | 说明 |
|---|---|
| `ast_grep_search` 工具 | AST 结构搜索（25 种语言，`$VAR` / `$$$` 元变量），无副作用、可并行 |
| `ast_grep_replace` 工具 | AST 结构改写，**默认 dry-run**（`dryRun: false` 才落盘），顺序执行 |
| `/ast-grep` | 显示解析到的二进制路径与版本 |

**与 pi 版的唯一有意差异**：不做自动下载，要求宿主 PATH 上已有 ast-grep 二进制
（`sg` 或 `ast-grep`）。找不到时工具返回安装提示：
`npm install -g @ast-grep/cli` / `cargo install ast-grep --locked` / `brew install ast-grep`。

`ast_grep_search` 的 pattern 必须是**完整 AST 节点**（不是正则）；零匹配时会给出
反模式提示（如 `\d`、`[a-z]`、`foo|bar`）并建议改用内置 `grep` 做纯文本搜索。

## pi → phi 的关键差异

phi 的 Rust SDK 与 pi 的扩展 API 并不等价。完整映射表见 [PLAN.md](PLAN.md)，
这里列出最影响行为的几条：

- **拿不到系统提示词**：phi 的 `before_agent_start` 只给用户提示词，
  返回项只有「改写提示词」与「追加系统提示词」。因此 cache-optimizer 的
  前缀稳定化 / skills 压缩 / session-overview 去抖**无法实现**。
- **没有请求体钩子**：无 `before_provider_request` / `before_provider_headers`，
  无法注入 `prompt_cache_key`、cache retention，也无法调 temperature / top_p。
- **只有命令处理器能交互**：`tool_call` / `tool_result` 等拦截回调拿不到
  `Context`，不能 `notify` / `set_status`。需要告知用户的告警改为缓存，
  由对应命令统一展示。
- **没有定时器与 abort**：sleep-continue 的看门狗、指数退避重试、Esc 中断
  检测均无对应能力，改为即时重试 + 连续失败计数。
- **没有用量事件**：phi 不向扩展推送 usage；缓存命中率与 token 速率改为读取
  宿主持久化的会话 JSONL（`~/.phi/session/<cwd>/<id>.jsonl`）里的 usage 计算，
  由 `/cache-optimizer stats` 展示，并同步到宿主底部状态行（`ctx.set_status`）。
  注意：composer 上那行 token 标签由宿主渲染，扩展无法写入，只能写其下方的
  扩展状态区。
- **转向只能靠 `turn_stopping`**：pi 的 `sendMessage(steer)` 在 phi 对应
  `on_turn_stopping` 返回 `Continue + message`，只在「本轮无工具调用、
  即将结束」时触发一次；据此加了连续转向上限防止死循环。
- **工具处理器拿不到 `Context`**：`cwd` / `session_id` 只在命令处理器可用；
  ast-grep 改读父进程 `/proc/<ppid>/cwd` 定位项目目录，并把子进程工作目录
  设为该目录，保证模型传入的相对路径按项目解析。

## 性能

扩展是长驻进程，且工具输出压缩 / 审批判定在每次工具调用上执行，因此这几处做了针对性优化。

### 实测（release，单核）

输入：93 KB / 2505 行的 `cargo build` 输出（2000 条编译进度行、1 个错误块、500 条警告）。

| 阶段 | 优化前 | 优化后 | 变化 |
|---|---|---|---|
| build 过滤（逐行判定） | 36.0 ms | 11.7 ms | **3.1x** |
| 全流程 `compact_tool_result` | 41.7 ms | 28.2 ms | **1.48x** |

（上表为 200 次调用的累计耗时。）优化后约 **7000 次压缩/秒**、输入吞吐约 **660 MB/s**。

### 做了什么

- **去掉逐行正则**（收益最大）：build 过滤器原先每行跑 19 条正则，实测占压缩全流程
  **86%** 的耗时。这些模式全是字面前缀，改为**首字节派发 + `starts_with`**，
  绝大多数行在 1–2 次字节比较内被排除。linter / search 也加了零成本预筛。
- **竞技场（bumpalo）**：`phi-ext-common::arena` 的 `Scratch` 接进 rtk 压缩管线与
  sleep-continue 审批路径。行索引、错误块、失败块、锚点行、源码过滤结果全部改为
  借用 `&str` 或写在竞技场里，只在最后向全局分配器要一个 `String`。
  预热后工作集稳定在 ~68 KB，`finish()` 只回拨指针、不归还 chunk。
- **消除重复解析**：同一条 bash 命令原先被归一化 6 次（build/test/git/linter 各自一次），
  现在只在入口归一化一次。
- **去掉每次调用的克隆**：rtk 的 `tool_result` 不再 `config.clone()`（配置含多个
  `Vec<String>`），改为字段级解构；sleep-continue 的动作摘要改为**惰性构造**——
  只读工具 / 工作区内写入 / 安全命令这些「直接放行」的路径（编码 agent 的绝大多数调用）
  不再遍历入参、不再算哈希。
- **去重**：`now_ms` 由 4 份收敛为 1 份（`phi-ext-common::time`）；
  ANSI 剥离由 2 份收敛为 1 份（`phi-ext-common::ansi`）。

### 顺带修掉的 bug

rtk 原先自带的 ANSI 正则实现漏剥带私有参数的 CSI 序列：

```
输入  ESC[?25l hidden ESC[?25h
正则  ESC[?25l hidden ESC[?25h   ← 未剥离（`?` 不在 [0-9;] 内）
字节扫描  hidden                   ← 正确
```

统一到 `phi-ext-common::ansi` 的单遍字节扫描后已修正，并补了回归测试。

### 一个诚实的结论

**单纯减少堆分配并没有带来明显提速**（复用竞技场与每次新建竞技场的耗时几乎相同，
40.96 ms vs 42.56 ms）——因为瓶颈在正则，不在分配器。真正有效的是去掉逐行正则。
因此剩余的优化空间集中在：`strip_ansi`（约 51 µs/次，已是单遍扫描）。
`test_output` 的失败块匹配已按首字节派发（`is_failure_start`），普通行不再触发正则。

### 第二轮优化（行为不变）

- **acp 去掉每 turn 的多余深拷贝**：`runtime.rs` 的 `process()` 不再 `outcome.state.clone()`，
  直接 `std::mem::take` 移入运行时；`compress.rs` 的 `hide_consumed_compress_calls` 在无消费
  调用时原样返回消息视图，不再 `messages.to_vec()`。
- **acp 会话 token 增量读取**：`session_tokens.rs` 记住上次文件偏移，只读自上次以来新增的
  字节（会话文件只追加），把每 turn 的读盘量从 512KB 尾部降到本 turn 追加量。
- **rtk 字符数只算一次**：`compact_tool_result` 算好的 `original_chars` / `compacted_chars`
  透传给 `OutputMetrics::track`，不再重复全量扫描；统计记录加上限（`MAX_RECORDS = 1000`，
  按插入顺序淘汰）避免长驻进程内存只增不减。
- **`apply_truncation` 字节预筛**：字节数是字符数上界，未超上限时跳过字符计数。
- **`phi-ext-common::text` 快路径**：`char_count` 对纯 ASCII 直接返回字节数；`is_anchor_line`
  不再为十六进制前缀分配 `String`。
- **去重**：`project_dir_name` 由 acp / cache-optimizer 各一份收敛到 `phi-ext-common::paths`。
- **JSON 库统一**：全面改用 simd-json，移除 `serde_json` 依赖。`serde_json` 仅在
  `simd-json` 内部作为可选 bench 依赖保留；workspace 与各扩展的直接依赖已删除。
  `phi-ext-common::config` 的宽松解析（`to_bool` / `to_int` / `to_enum`）改为收
  `Option<&Value>`，并新增 `child(parent, key)` 辅助（替代 `Value::Null` 占位）；
  `phi-ext-common::json` 重导出 `json!` 宏与所需 trait，并提供 `to_vec` / `to_value`。
  acp 的 `tools.rs` schema 构造、sleep-continue 的 `write_stable`、deepseek 的
  `str_replace_editor` 入参解析等也都改走 `phi_ext_common::json`。

## 开发

```bash
cargo test --workspace     # 380 个测试（含 5 个 PXB 生命周期端到端冒烟测试）
cargo build --release      # 构建全部扩展
cargo clippy --workspace --all-targets   # 静态检查（当前零告警）
```

每个扩展都带一个 `tests/pbx_handshake.rs`：以子进程方式启动二进制，
走完 `Hello → HelloAck → Register* → Ready → Shutdown → ShutdownAck`，
并断言注册出来的命令/工具名。