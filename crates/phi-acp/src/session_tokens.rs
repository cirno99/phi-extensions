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

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use phi_ext_common::paths;

/// 尾部读取窗口：最新的 usage 一定在文件末尾附近。
const TAIL_BYTES: u64 = 512 * 1024;

/// 复刻 Go 侧 `project.ProjectDirName`（`internal/project/session_dir.go`）：
/// 去掉前导路径分隔符，把 `/ \ :` 换成 `-`，两边包 `--`。
///
/// 例：`/home/cirno99/Code/Rust/phi-extensions` →
/// `--home-cirno99-Code-Rust-phi-extensions--`。
pub fn project_dir_name(cwd: &str) -> String {
    let cleaned = cwd.trim_end_matches(['/', '\\']);
    let stripped = cleaned.strip_prefix(['/', '\\']).unwrap_or(cleaned);
    let mut out = String::with_capacity(stripped.len() + 4);
    for ch in stripped.chars() {
        match ch {
            '/' | '\\' | ':' => out.push('-'),
            _ => out.push(ch),
        }
    }
    if out.is_empty() {
        out.push_str("unknown");
    }
    format!("--{out}--")
}

/// 会话根目录：`<phi_home>/session`。
fn session_base() -> PathBuf {
    paths::phi_home().join("session")
}

/// 读取当前活动会话的真实上下文 token 数（无可用数据时返回 `None`）。
pub fn read_context_tokens() -> Option<u64> {
    let file = active_session_file()?;
    read_tail_context_tokens(&file)
}

/// 定位当前活动会话文件：优先 `PWD` 对应的项目目录，回退全局最新 `.jsonl`。
fn active_session_file() -> Option<PathBuf> {
    if let Ok(pwd) = std::env::var("PWD") {
        if !pwd.trim().is_empty() {
            let dir = session_base().join(project_dir_name(&pwd));
            if let Some(file) = newest_jsonl_in(&dir) {
                return Some(file);
            }
        }
    }
    newest_jsonl_any()
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
fn newest_jsonl_any() -> Option<PathBuf> {
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
    fn project_dir_name_matches_go_encoding() {
        assert_eq!(
            project_dir_name("/home/cirno99/Code/Rust/phi-extensions"),
            "--home-cirno99-Code-Rust-phi-extensions--"
        );
        assert_eq!(project_dir_name("/"), "--unknown--");
        assert_eq!(project_dir_name(""), "--unknown--");
    }

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
}
