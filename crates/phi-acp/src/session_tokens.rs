//! 从 phi 宿主会话文件读取真实上下文 token 数。
//!
//! 背景：phi 扩展协议（PXB）不向扩展暴露宿主的 token 用量——事件
//! [`pxb::EventNotify`] 只有 turn_index 等字段，`session_id`/`cwd` 也仅在
//! 斜杠命令的 `Context` 里可见，`turn_stopping` 等钩子拿不到。因此本扩展
//! 过去只能统计自己观测到的消息视图（用户输入 + 工具调用/结果），会严重
//! 低估真实上下文。
//!
//! 但宿主会把每次 completion 的 token 用量**持久化到会话 JSONL 文件**：
//! `<phi_home>/session/<ProjectDirName(cwd)>/<timestamp>_<sessionID>.jsonl`，
//! 每条 assistant 消息 entry 顶层带 `usage`（见 phi 源码
//! `internal/session/entry.go` 的 `SessionMessageEntry.Usage`，注释明确说是为
//! "session lifecycle extensions" 保留的）。
//!
//! 于是本模块直接读盘取真实值：定位当前活动会话文件（最近被追加写入的那
//! 个），解析最后一条非零 `usage`，按 `ContextTokens()` 口径换算上下文大小。

use std::cell::RefCell;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use phi_ext_common::paths;

/// 尾部读取窗口：最新的 usage 一定在文件末尾附近。
const TAIL_BYTES: u64 = 512 * 1024;

/// 会话根目录：`<phi_home>/session`。
fn session_base() -> PathBuf {
    paths::phi_home().join("session")
}

/// 复用的读取缓存：记住上次读到的文件偏移与结果，避免每 turn 重复读取整段
/// 512KB 尾部。扩展回调跑在单线程 runtime 上，`thread_local` 足够。
struct SessionCache {
    path: PathBuf,
    offset: u64,
    tokens: Option<u64>,
}

thread_local! {
    static CACHE: RefCell<Option<SessionCache>> = const { RefCell::new(None) };
}

// 解析出的会话**目录**缓存：命中后不再读 `/proc`、不再重算项目目录名，
// 只在该目录内重扫最新 `.jsonl`。
//
// 为什么缓存目录而不是文件：`/new` 之后新会话文件是**惰性创建**的，新文件
// 出现前旧文件仍是最新；若把文件本身缓存下来，新文件出现后会一直读旧文件、
// 无法自愈（`refresh_session_key` 正是靠反复重扫来痊愈的）。目录在整个进程
// 生命周期内稳定（cwd 不变），缓存它安全。会话切换由 [`reset_cache`] 清空。
thread_local! {
    static ACTIVE_DIR: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// 清空复用的读取缓存。
///
/// 会话切换（`SessionShutdown` / `SessionStart`）时必须调用：旧会话记住的文件
/// 偏移拿去读新会话的文件会直接跳过头几行（甚至把别人的 usage 当成本会话的）。
pub fn reset_cache() {
    CACHE.with(|cache| *cache.borrow_mut() = None);
    ACTIVE_DIR.with(|slot| *slot.borrow_mut() = None);
}

// 最近一次读取是否真的拿到了宿主 usage。
//
// 用于 `/acp status` 如实告知「这个百分比是宿主真值还是本地估算」——两者
// 相差可达 40%（本扩展只观测用户输入 + 工具结果 + 工具入参，看不到助手正文
// 与推理），排障时必须能区分。
thread_local! {
    static HOST_TOKENS_OK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// 最近一次 [`read_context_tokens`] 是否命中宿主 usage。
pub fn last_read_used_host() -> bool {
    HOST_TOKENS_OK.with(|c| c.get())
}

/// 读取当前活动会话的真实上下文 token 数（无可用数据时返回 `None`）。
///
/// 会话文件只追加，因此只读取自上次调用以来新增的字节，把每 turn 的读盘量从
/// 「512KB 尾部」降到「本 turn 追加量」。
pub fn read_context_tokens() -> Option<u64> {
    let file = active_session_file()?;
    let len = fs::metadata(&file).ok()?.len();

    let cached = CACHE.with(|cache| {
        cache
            .borrow()
            .as_ref()
            .filter(|entry| entry.path == file && entry.offset <= len)
            .map(|entry| (entry.offset, entry.tokens))
    });

    let result = match cached {
        Some((offset, previous)) => {
            if offset == len {
                // 文件没有新增内容，直接复用上次结果。
                return finish(previous);
            }
            let (appended, new_offset) = read_appended(&file, offset);
            let tokens = appended.or(previous);
            CACHE.with(|cache| {
                *cache.borrow_mut() = Some(SessionCache {
                    path: file,
                    offset: new_offset,
                    tokens,
                });
            });
            tokens
        }
        None => {
            let tokens = read_tail_context_tokens(&file);
            CACHE.with(|cache| {
                *cache.borrow_mut() = Some(SessionCache {
                    path: file,
                    offset: len,
                    tokens,
                });
            });
            tokens
        }
    };
    finish(result)
}

/// 记录「本次是否命中宿主 usage」并原样返回结果。
fn finish(tokens: Option<u64>) -> Option<u64> {
    HOST_TOKENS_OK.with(|c| c.set(tokens.is_some()));
    tokens
}

/// 定位当前活动会话文件。
///
/// 候选 cwd 依可信度排序：
/// 1. 宿主经命令上下文回填的 cwd（`/acp` 执行过才有）；
/// 2. **父进程 cwd**——宿主用 `cmd.Dir = <扩展目录>` 启动扩展，但父进程自身
///    的 cwd 就是用户的项目目录，直接读 `/proc/<ppid>/cwd` 即可，无需等
///    `/acp` 跑过一次。这是让 `useHostTokens` 从第一轮就生效的关键：扩展
///    进程的 `PWD` 是自己的目录，仅靠它定位会一直找不到正确的会话目录，
///    于是全程退回本地估算（实测高估/低估可达 40%）。
/// 3. 进程 `PWD`（最后的兜底）。
///
/// 除读 token 外，[`crate::session`] 也用它解析**当前会话 id**（文件名里的
/// `<timestamp>_<id>.jsonl`）——`SessionStart` 事件不带会话 id。
///
/// **不做**「全局最新 `.jsonl`」回退：多项目并行时会读到别的项目的会话，
/// 把别人的 usage 当成本会话的上下文，使用率判断随之错位。宁可不给数。
pub(crate) fn active_session_file() -> Option<PathBuf> {
    // 会话目录已知时只重扫该目录取最新文件（见 `ACTIVE_DIR` 的说明）。
    if let Some(dir) = ACTIVE_DIR.with(|slot| slot.borrow().clone()) {
        return newest_jsonl_in(&dir);
    }

    let mut candidates: Vec<String> = vec![host_cwd()];
    if let Some(cwd) = parent_process_cwd() {
        candidates.push(cwd);
    }
    candidates.push(std::env::var("PWD").ok().unwrap_or_default());

    for cwd in candidates {
        if cwd.trim().is_empty() {
            continue;
        }
        let dir = session_base().join(paths::project_dir_name(&cwd));
        if let Some(file) = newest_jsonl_in(&dir) {
            ACTIVE_DIR.with(|slot| *slot.borrow_mut() = Some(dir));
            return Some(file);
        }
    }
    None
}

/// 宿主（父进程）的工作目录。
///
/// Linux 上 `/proc/<ppid>/cwd` 指向真实项目目录；宿主刻意没给子进程设置
/// `PWD`（其源码中没有 `PWD` 字面量），所以这是扩展在 `/acp` 之前拿到项目
/// 目录的唯一途径。非 Linux 或读取失败时返回 `None`（回退到其它候选）。
fn parent_process_cwd() -> Option<String> {
    let stat = fs::read_to_string("/proc/self/stat").ok()?;
    // 格式：`pid (comm) state ppid ...`；`comm` 可能含空格 / 括号，因此必须从
    // **最后一个** `)` 之后开始切分才可靠。
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    let _state = fields.next()?;
    let ppid = fields.next()?;
    let cwd = fs::read_link(format!("/proc/{ppid}/cwd")).ok()?;
    Some(cwd.to_string_lossy().into_owned())
}

// 宿主报告的工作目录。
//
// ⚠️ SDK 只在命令处理器的 `Context` 里暴露 cwd/session_id（拦截回调拿不到），
// 所以这里只能由命令侧（`/acp …` 的 `ctx.cwd()`）回填。拿到之前退回进程 `PWD`。
// 宿主切会话时虽会推 `SessionMeta`，但 SDK 未把它转发给拦截回调——这是已知降级点。
thread_local! {
    static HOST_CWD: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// 记录宿主报告的 cwd（由命令处理器调用）。
pub fn set_host_cwd(cwd: &str) {
    if cwd.trim().is_empty() {
        return;
    }
    HOST_CWD.with(|slot| *slot.borrow_mut() = Some(cwd.to_string()));
}

fn host_cwd() -> String {
    HOST_CWD
        .with(|slot| slot.borrow().clone())
        .unwrap_or_default()
}

/// 目录内最近修改的 `.jsonl` 文件。
fn newest_jsonl_in(dir: &Path) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if best.as_ref().map_or(true, |(t, _)| modified > *t) {
            best = Some((modified, path));
        }
    }
    best.map(|(_, path)| path)
}

/// 扫描全部项目子目录，取最近修改的 `.jsonl`。
///
/// 仅当调用方显式要求「全局最新」时才有意义（诊断 / 回退），默认定位路径不再
/// 使用它——见 [`active_session_file`] 的说明。
pub fn newest_jsonl_anywhere() -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in fs::read_dir(session_base()).ok()?.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Some(file) = newest_jsonl_in(&dir) else {
            continue;
        };
        let Ok(modified) = fs::metadata(&file).and_then(|m| m.modified()) else {
            continue;
        };
        if best.as_ref().map_or(true, |(t, _)| modified > *t) {
            best = Some((modified, file));
        }
    }
    best.map(|(_, path)| path)
}

/// 读文件尾部，返回最后一条非零 usage 的上下文 token 数。
fn read_tail_context_tokens(path: &Path) -> Option<u64> {
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    // 从文件中部开始读时，首行可能被截断，丢弃它。
    let body = if start > 0 {
        let i = text.find('\n')?;
        &text[i + 1..]
    } else {
        &text[..]
    };
    last_context_tokens(body)
}

/// 从 `start` 起读取新增内容，返回（新增内容里的最后一条非零 usage，新的安全偏移）。
///
/// 只消费到最后一个完整行；若末尾是半行，偏移停在该行开头，留待下次。
fn read_appended(path: &Path, start: u64) -> (Option<u64>, u64) {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return (None, start),
    };
    if file.seek(SeekFrom::Start(start)).is_err() {
        return (None, start);
    }
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return (None, start);
    }
    let text = String::from_utf8_lossy(&buf);
    match text.rfind('\n') {
        Some(index) => {
            let complete = &text[..=index];
            let new_offset = start + index as u64 + 1;
            (last_context_tokens(complete), new_offset)
        }
        // 没有完整行：不推进偏移，避免丢行。
        None => (None, start),
    }
}

/// 从 JSONL 文本里取最后一条非零 usage 的上下文 token 数。
fn last_context_tokens(text: &str) -> Option<u64> {
    for line in text.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(entry) = phi_ext_common::json::parse_str::<UsageEntry>(line) else {
            continue;
        };
        if let Some(usage) = entry.usage {
            let tokens = usage.context_tokens();
            if tokens > 0 {
                return Some(tokens);
            }
        }
    }
    None
}

/// 会话 entry 里我们关心的字段（其余字段由 serde 忽略）。
#[derive(serde::Deserialize)]
struct UsageEntry {
    #[serde(default)]
    usage: Option<RawUsage>,
}

/// 复刻 Go 侧 `llm.Usage` 的 JSON 形状。
#[derive(serde::Deserialize)]
struct RawUsage {
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<PromptDetails>,
}

#[derive(serde::Deserialize)]
struct PromptDetails {
    #[serde(default)]
    cached_tokens: u64,
    #[serde(default)]
    cache_write_tokens: u64,
}

impl RawUsage {
    /// 对齐 Go 侧 `llm.Usage.ContextTokens()`：`total_tokens` 优先，否则各
    /// 互斥桶求和（prompt + completion + 缓存读 + 缓存写）。
    fn context_tokens(&self) -> u64 {
        if self.total_tokens > 0 {
            return self.total_tokens;
        }
        let (cached, write) = self
            .prompt_tokens_details
            .as_ref()
            .map(|d| (d.cached_tokens, d.cache_write_tokens))
            .unwrap_or((0, 0));
        self.prompt_tokens + self.completion_tokens + cached + write
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_context_tokens_prefers_total_and_skips_zero() {
        let text = concat!(
            r#"{"type":"message","usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}}"#,
            "\n",
            r#"{"type":"message","usage":{"prompt_tokens":0,"completion_tokens":0,"total_tokens":0}}"#,
            "\n",
        );
        // 末条为零 -> 向前找到 12。
        assert_eq!(last_context_tokens(text), Some(12));
    }

    #[test]
    fn last_context_tokens_sums_buckets_without_total() {
        let text = r#"{"usage":{"prompt_tokens":100,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":40,"cache_write_tokens":3}}}"#;
        assert_eq!(last_context_tokens(text), Some(148));
    }

    #[test]
    fn last_context_tokens_returns_none_without_usage() {
        let text = r#"{"type":"session","id":"x"}"#;
        assert_eq!(last_context_tokens(text), None);
    }

    #[test]
    fn last_context_tokens_ignores_malformed_lines() {
        let text = concat!(
            "{oops not json}\n",
            r#"{"usage":{"total_tokens":77}}"#,
            "\n",
        );
        assert_eq!(last_context_tokens(text), Some(77));
    }

    #[test]
    fn read_tail_context_tokens_reads_file_end() {
        let path = std::env::temp_dir().join(format!(
            "phi-acp-session-tokens-{}.jsonl",
            std::process::id()
        ));
        let content = concat!(
            r#"{"usage":{"total_tokens":111}}"#,
            "\n",
            r#"{"usage":{"total_tokens":222}}"#,
            "\n",
        );
        std::fs::write(&path, content).expect("写入临时会话文件失败");
        assert_eq!(read_tail_context_tokens(&path), Some(222));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_appended_reads_only_new_bytes() {
        let path = std::env::temp_dir().join(format!(
            "phi-acp-session-append-{}.jsonl",
            std::process::id()
        ));
        std::fs::write(&path, "{\"usage\":{\"total_tokens\":111}}\n").expect("写入失败");
        let (first, offset) = read_appended(&path, 0);
        assert_eq!(first, Some(111));
        // 追加一行后，只读新增部分。
        std::fs::write(
            &path,
            "{\"usage\":{\"total_tokens\":111}}\n{\"usage\":{\"total_tokens\":222}}\n",
        )
        .expect("写入失败");
        let (second, _) = read_appended(&path, offset);
        assert_eq!(second, Some(222));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_appended_defers_partial_line() {
        let path = std::env::temp_dir().join(format!(
            "phi-acp-session-partial-{}.jsonl",
            std::process::id()
        ));
        // 第二个 JSON 行没有换行结尾，视为半行。
        std::fs::write(
            &path,
            "{\"usage\":{\"total_tokens\":5}}\n{\"usage\":{\"total_tokens\":9}}",
        )
        .expect("写入失败");
        let (first, offset) = read_appended(&path, 0);
        assert_eq!(first, Some(5));
        // 补上换行后，半行才被消费。
        std::fs::write(
            &path,
            "{\"usage\":{\"total_tokens\":5}}\n{\"usage\":{\"total_tokens\":9}}\n",
        )
        .expect("写入失败");
        let (second, _) = read_appended(&path, offset);
        assert_eq!(second, Some(9));
        let _ = std::fs::remove_file(&path);
    }
}
