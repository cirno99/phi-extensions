# phi-extensions

把 pi (TypeScript) 扩展重写为 **phi (Rust) 扩展**。扩展通过 PXB
（Phi eXtension Binary）协议与宿主通信，使用官方 `phi-ext` SDK。

## 扩展一览

| crate | 来源（pi 扩展） | 说明 |
|---|---|---|
| [`phi-asymptotic-thinking`](crates/phi-asymptotic-thinking) | `asymptotic-thinking.js` | 渐近式思考六态状态机：3 个工具 + 状态守卫转向 + `/asymptotic-*` 命令 |
| [`phi-rtk-optimizer`](crates/phi-rtk-optimizer) | `pi-rtk-optimizer.js` | RTK 命令重写（`rtk rewrite` 子进程）+ 工具输出压缩（ansi/build/test/git/linter/search/source/truncate） |
| [`phi-sleep-continue`](crates/phi-sleep-continue) | `sleep-continue.js` | 无人值守自动继续 + 提问拦截 + 失败重试，并参照 [pi-auto-approval](https://github.com/Europa2061/pi-auto-approval) 增加规则化自动审批 |
| [`phi-cache-optimizer`](crates/phi-cache-optimizer) | `pi-cache-optimizer.js` | 缓存优化配置 + 能力诊断（可落地子集，见下） |
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

### asymptotic-thinking

| 入口 | 说明 |
|---|---|
| `asymptotic-think_set-task-info` | 设定任务画像（难度 / 大类型 / 小类型，含 Zig） |
| `asymptotic-think_transition` | 状态流转（六态，带合法性校验） |
| `asymptotic-think_status` | 查询状态机快照与建议推理参数 |
| `/asymptotic-status` | 展示快照并写入会话 |
| `/asymptotic-toggle [on\|off]` | 开关框架 |

### rtk-optimizer

| 入口 | 说明 |
|---|---|
| `/rtk show\|path\|verify\|stats\|clear-stats\|reset\|help` | 配置与诊断 |

配置：`~/.phi/extensions/phi-rtk-optimizer/config.json`（字段与 pi 版一致，
含 `mode` / `outputCompaction.*`，旧配置缺少 `readCompaction` 时按旧默认解释）。

### sleep-continue

| 入口 | 说明 |
|---|---|
| `/sleep-on [文本]` `/sleep-off` `/sleep-set <文本>` `/sleep-max <N>` `/sleep-status` | 自动继续 |
| `/sleep-approval status\|on\|off\|safe\|permissive\|allow <模式>\|deny <模式>\|allowlist <命令>\|approve\|clear\|reset` | 自动审批 |

配置：`~/.phi/extensions/phi-sleep-continue/config.json`。
**注意**：自动审批只在无人值守（`/sleep-on`）开启时生效，避免影响日常交互。

### cache-optimizer

| 入口 | 说明 |
|---|---|
| `/cache-optimizer status\|doctor\|enable\|disable\|config …\|stats\|reset\|help` | 配置与能力诊断 |

配置：`~/.phi/extensions/phi-cache-optimizer/config.json`。

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
- **没有用量事件**：无法统计缓存命中率与 token 速率；计算函数已就绪待用。
- **转向只能靠 `turn_stopping`**：pi 的 `sendMessage(steer)` 在 phi 对应
  `on_turn_stopping` 返回 `Continue + message`，只在「本轮无工具调用、
  即将结束」时触发一次；asymptotic-thinking 因此加了连续转向上限防止死循环。

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
因此剩余的优化空间集中在：`strip_ansi`（约 51 µs/次，已是单遍扫描）、
以及 `test_output` 的失败块匹配（失败时逐行跑 7 条正则，尚未做首字节派发）。

## 开发

```bash
cargo test --workspace     # 291 个测试（含 4 个 PXB 生命周期端到端冒烟测试）
cargo build --release      # 构建全部扩展
cargo clippy --workspace --all-targets   # 静态检查（当前零告警）
```

每个扩展都带一个 `tests/pbx_handshake.rs`：以子进程方式启动二进制，
走完 `Hello → HelloAck → Register* → Ready → Shutdown → ShutdownAck`，
并断言注册出来的命令/工具名。

代码生成：`scripts/gen-prompts.mjs` 从 pi 版 asymptotic-thinking 的 27 个
TS 提示词模块生成 `prompts.rs`，并内置一份 Zig 提示词（pi 上游没有 Zig 模块），
使语言覆盖包含 Zig。

```bash
bun scripts/gen-prompts.mjs \
  ~/.pi/agent/extensions/asymptotic-thinking/src \
  crates/phi-asymptotic-thinking/src/prompts.rs
```