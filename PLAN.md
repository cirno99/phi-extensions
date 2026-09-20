# phi-extensions 重写计划

把 `~/.pi/agent/compiled/` 下的 pi (TypeScript) 扩展重写为 phi (Rust) 扩展，
采用 PXB（Phi eXtension Binary）协议，通过 `phi-ext` SDK 接入宿主。

## 目标扩展与状态

| pi 扩展 | phi crate | 状态 |
|---|---|---|
| `asymptotic-thinking.js` | `phi-asymptotic-thinking` | ✅ 完成 |
| `sleep-continue.js` | `phi-sleep-continue` | ✅ 完成（含 pi-auto-approval 的规则化自动审批） |
| `pi-rtk-optimizer.js` | `phi-rtk-optimizer` | ✅ 完成 |
| `pi-cache-optimizer.js` | `phi-cache-optimizer` | ✅ 完成（可落地子集） |
| `pi-statusline.js` | — | ❌ 不移植（phi 宿主已自带状态栏） |

**关于 statusline**：只把其中两项纯计算补进 `phi-ext-common::stats`：

- **缓存命中率**：`cacheRead / (cacheRead + cacheWrite + input)`，上限 100%
- **token 速率**：`数量 / (毫秒 / 1000)`，格式化为 `◆ 123tps`（tps = token/s）

**关于语言覆盖**：所有涉及语言判定的地方都补齐了 **Zig**：

- asymptotic-thinking：新增小任务类型 `ZIG_DEV`（Zig开发），
  并把 28 类子类型的提示词表一起生成（`scripts/gen-prompts.mjs` 内置 Zig 提示词，
  因为 pi 上游没有 Zig 模块）。
- rtk-optimizer：`Language::Zig`（`.zig` / `.zon`），注释与文档注释规则
  （`//`、`/* */`、`///`、`//!`）；构建命令 `zig build`；
  测试命令 `zig test` / `zig build test` 及 Zig 测试运行器输出格式
  （`All N tests passed.`、`N passed; N failed.`、`i/N test.x... FAIL`）；
  linter `zlint`。

## 项目结构

```
crates/
  phi-ext-common/          # 路径、配置原子读写、ANSI、文本、用量统计、竞技场、jemalloc
  phi-asymptotic-thinking/ # 六态状态机（工具 + 状态守卫 + 命令）
  phi-rtk-optimizer/       # rtk 命令重写 + 工具输出压缩
  phi-cache-optimizer/     # 缓存优化配置 + 能力诊断
  phi-sleep-continue/      # 无人值守自动继续 + 规则化自动审批
scripts/
  gen-prompts.mjs          # 从 pi 提示词模块生成 prompts.rs（内置 Zig）
  install.sh               # 构建并安装到 ~/.phi/extensions/
```

## pi → phi 的宿主能力映射（关键约束）

| pi 能力 | phi 对应 | 说明 |
|---|---|---|
| `before_agent_start` 返回 `message`/`systemPrompt`/`systemPromptOptions` | `on_before_agent_start` 返回 `prompt` / `system_prompt_append` | **拿不到系统提示词**，只能改写用户提示词或追加系统提示词 |
| `pi.sendMessage(..., {deliverAs:"steer"})` | `on_turn_stopping` 返回 `Continue + message` | 只在「无工具调用即将结束」时给一次转向机会 |
| `pi.on("turn_end")` 注入提醒 | `subscribe(TurnEnd)` 只能观察 | 观察类事件拿不到 `Context`，无法发消息/改状态 |
| `tool_call` 拦截 / 改写入参 | `on_tool_call` → `Block` / `input` | 等价 |
| `tool_result` 改写 | `on_tool_result` → `content` | 等价（phi 的 content 是字符串，pi 是内容块数组） |
| `ctx.ui.setStatus` / `notify` | `Context::set_status` / `notify` | **仅命令处理器可调用** |
| `ctx.ui.setFooter` / `setWidget` / 审批弹窗 | 无 | 只有单行 footer 状态与 `confirm` |
| `before_provider_request` / `before_provider_headers` | 无 | 无法改请求体/请求头（temperature、top_p、prompt_cache_key…） |
| `pi.exec`（带超时子进程） | `std::process::Command` + `try_wait` 轮询 | 自行实现超时 |
| `sessionManager` 读用量 / `getContextUsage` | 无 | 无会话用量读取，也无用量事件 |
| 定时器 / `ctx.abort()` | 无 | 无定时器回调、无 abort RPC |
| 宿主 LLM 调用（分类器） | 无 | 扩展无法发起模型调用 |
| 持久化 | 自行写 `~/.phi/extensions/<name>/` | 由 `phi-ext-common` 提供原子写 |

## 各扩展的取舍

### asymptotic-thinking

- `before_agent_start` 的引导消息改为追加到系统提示词。
- `turn_end` 的 steer 提醒改由 `turn_stopping` 发送；连续转向超过
  `MAX_CONSECUTIVE_STEERS`（5 次）后放行停止，避免死循环。
- `before_provider_request` 的推理参数无对应钩子，仅作为**建议参数**
  在 `/asymptotic-status` 与 `asymptotic-think_status` 中展示。
- 会话状态从 pi 的 SQLite 改为单文件 JSON 原子覆写（phi 拦截回调拿不到
  session id，状态按扩展实例存储）。

### sleep-continue

原有行为（提问拦截 / 自动继续 / 失败重试）之外的取舍：

- 看门狗 + `ctx.abort()`：无对应能力，移除；`/sleep-stall` 一并移除。
- 指数退避 `await sleep(delay)`：`turn_stopping` 是同步回调，改为即时重试
  + 连续失败计数。
- Esc 中断检测：phi 的 `AgentEnd` 事件不带 `aborted` 标志，移除。
- toast：改为在命令里刷新 footer 与提示。

**参照 [pi-auto-approval](https://github.com/Europa2061/pi-auto-approval)
新增的规则化自动审批**（仅在 `/sleep-on` 开启时生效）：

- 移植 `tool-routing` / `safe-command` / `decision` 的路由顺序：
  `deny 名单 → 只读工具 → 工作区内写入 → 必须人工交互的工具 →
  安全只读命令 → allow 名单 → 会话已批准 → 模式兜底`。
- 安全只读命令判定与 pi 版一致：`pwd`、只读 git 子命令
  （`status`/`log`/`diff`/`show`/`rev-parse`、`branch` 的安全旗标），
  并支持 `safeCommandAllowlist`（`*` 结尾前缀匹配）。
- 去掉 LLM 分类器与人工回退（phi 无法发起模型调用，`tool_call` 期也无 UI）：
  `Safe` 模式下「规则无法证明安全」= 阻止，等价于 pi 的 `auto` 模式；
  `Permissive` 模式则放行未命中 `deny` 的动作。
- 动作指纹用 FNV-1a 64（仅作会话内去重键），pi 用 SHA-256。
- 连续拒绝达到 `maxConsecutiveDenials` 时，阻止理由会要求模型停下来求助。

### 性能优化（跨扩展）

审查后落地的优化，详见 README 的「性能」一节：

- **rtk-optimizer**：build 过滤的 19 条逐行正则改为首字节派发（实测该步 3.1x、
  全流程 1.48x）；竞技场接进压缩管线；命令只归一化一次（6 → 1）；去掉每次
  `tool_result` 的 `config.clone()`；linter / search 加零成本预筛。
- **sleep-continue**：审批判定用竞技场；动作摘要惰性构造，直接放行路径不再
  遍历入参或算哈希。
- **phi-ext-common**：新增 `time::now_ms`（收敛 4 份实现）与 `arena::split_lines`；
  `ansi` 增加 `strip_ansi_fast` 并补 CSI 私有参数的回归测试。
- **去重**：rtk 自带的 ANSI 正则实现已删除（既慢又漏剥 `ESC[?25l` 这类序列），
  统一走 `phi-ext-common::ansi` 的单遍字节扫描。

### rtk-optimizer

- 输出压缩技术（ansi / build / test / git / linter / search / source /
  truncate / smart-truncate）与命令重写（`rtk rewrite` 子进程 + `RTK_DB_PATH`
  环境前缀 + Windows 管道安全改写 + Windows bash 兼容修正）逐条移植。
- phi 无 `tool_execution_update` / `tool_execution_end` 事件，流式清洗移除；
  压缩只在 `tool_result` 做一次。
- phi 的 content 是字符串而非内容块数组，压缩直接作用于整串；
  锚点安全 read 压缩保留锚点识别与重映射逻辑。
- 改写提示 / 缺 rtk 告警无法从回调弹 toast，改为缓存后由 `/rtk` 展示。
- 修正 pi 的一处解析 bug：`test result: ok. N passed; M failed;` 曾被解析成
  passed=0 / failed=N，本实现按模式声明分组下标取正确数字。

### cache-optimizer

phi 不暴露系统提示词、无请求体钩子、无用量事件，因此 pi 版的优化**均无法落地**：

- 前缀稳定化（stable candidates）、skills 列表压缩、session-overview 去抖
  —— 都依赖系统提示词内容，不可实现。
- `prompt_cache_key` / cache retention 注入、确定性工具排序
  —— 依赖请求体/请求头钩子，不可实现。
- 缓存命中率与 token 速率统计 —— 依赖用量事件，不可实现
  （计算函数已在 `phi-ext-common::stats` 就绪）。

因此本 crate 只保留**确实可落地**的部分：配置持久化 + `/cache-optimizer`
命令，并提供 `doctor` 明确报告每项能力的可用性与原因，避免静默失效。