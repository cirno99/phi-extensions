//! 会话身份 —— 压缩状态的归属键。
//!
//! # 为什么需要它
//!
//! 上游 `billion-context` 的压缩状态是**按会话**存的：`src/paths.ts` 的
//! `sessionsDir()` 注释就是 *"Sessions dir: one JSON file per session"*，
//! `src/persist.ts` 落盘的结构是 `PersistedSession { id, state }`。
//!
//! 本扩展早先只有一个**全局** `state.json`，于是新会话会继承上一个会话的 ref
//! 索引与块账本：
//!
//! - `acp_status` 报出宿主机历史里**根本不存在**的可压缩范围；
//! - 模型照着这些 ref 调 `compress`，必然被 ref 门拒绝（unknown ref）——
//!   表面症状就是「压缩失败 / 无作用」；
//! - `acp_search` 返回上一个会话的块，`acp_decompress` 取回的是别的会话的原文。
//!
//! ref（`m00042` / `b3`）在注入契约里明确是**每会话**的，状态文件也必须每会话。
//!
//! # 会话 id 从哪来
//!
//! ⚠️ phi 的 `SessionStart` 事件**不带当前会话 id**：`internal/extension/proc.go:755`
//! 只转发 `Reason` 与 `PreviousSessionID`，`ext.SessionStartEvent.SessionID` 在
//! 转发时被丢掉了（宿主侧的一个缺口）。`SessionShutdown` 倒是带
//! `TargetSessionID`，但它对 `/new` 是空串（`controller.go` 的
//! `beginSessionSwitch("new", "", false)`）。
//!
//! 能拿到 id 的地方是**会话文件名**：
//! `<phi_home>/session/<ProjectDirName>/<timestamp>_<32位十六进制>.jsonl`
//! （`internal/session/manager.go:113` / `:218` 的 `fmt.Sprintf("%s_%s.jsonl", …)`，
//! id 来自 `generateSessionID()` = 16 字节 hex）。因此这里从文件名解析，
//! 复用 [`crate::session_tokens`] 已经用来定位当前会话文件的那套逻辑。
//!
//! ⚠️ 该文件是**惰性创建**的（`manager.go` 的 `flushAllEntries` 在第一条消息时
//! 才 `os.Create`）。因此 `/new` 之后、第一条消息落盘之前，解析会得到**上一个**
//! 会话的 id。调用方必须把 `SessionStart` 的 `reason == "new"` 当作权威信号
//! （见 [`crate::runtime::Runtime::on_session_start`]），并在键未知时**不落盘**。

/// 从会话文件名解析会话 id。
///
/// 形如 `2026-01-02T15-04-05_0123456789abcdef0123456789abcdef.jsonl`：时间戳
/// （`2006-01-02T15-04-05`）不含 `_`，因此按**第一个** `_` 切分即可。
pub fn key_from_file_name(name: &str) -> Option<String> {
    let stem = name.strip_suffix(".jsonl")?;
    let (_, id) = stem.split_once('_')?;
    if id.is_empty() {
        return None;
    }
    Some(sanitize_key(id))
}

/// 把会话 id 净化成安全的文件名片段。
///
/// phi 的 id 是纯十六进制、本身安全；这里仍然过滤，因为键也可能来自调用方
/// （单测、将来宿主换格式），不能让它拼出路径分隔符或 `..`。
pub fn sanitize_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for ch in key.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    // 全部字符都是 ASCII，`truncate` 一定落在字符边界上。
    out.truncate(64);
    if out.is_empty() || out.chars().all(|ch| ch == '.') {
        return "unknown".to_string();
    }
    out
}

/// 当前活动会话的 id（无法确定时返回 `None`）。
pub fn current_session_key() -> Option<String> {
    #[cfg(test)]
    if let Some(value) = KEY_OVERRIDE.with(|slot| slot.borrow().clone()) {
        return value;
    }
    let file = crate::session_tokens::active_session_file()?;
    key_from_file_name(file.file_name()?.to_str()?)
}

#[cfg(test)]
thread_local! {
    /// 单测覆盖：外层 `Some` = 「已设置覆盖」，内层即返回值。
    ///
    /// 必须存在：默认实现会去读**用户真实**的会话目录，单测会因此变成
    /// 非确定性（依赖跑测试时机器上恰好有哪些会话）。
    static KEY_OVERRIDE: std::cell::RefCell<Option<Option<String>>> =
        const { std::cell::RefCell::new(None) };
}

/// 单测：固定 [`current_session_key`] 的返回值（`None` 取消覆盖）。
#[cfg(test)]
pub fn set_key_override(value: Option<Option<String>>) {
    KEY_OVERRIDE.with(|slot| *slot.borrow_mut() = value);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_should_be_parsed_from_a_session_file_name() {
        assert_eq!(
            key_from_file_name("2026-01-02T15-04-05_0123456789abcdef0123456789abcdef.jsonl"),
            Some("0123456789abcdef0123456789abcdef".to_string())
        );
    }

    #[test]
    fn key_should_be_none_without_a_session_separator() {
        assert_eq!(key_from_file_name("2026-01-02T15-04-05.jsonl"), None);
        assert_eq!(key_from_file_name("2026-01-02T15-04-05_.jsonl"), None);
        assert_eq!(key_from_file_name("not-a-session.txt"), None);
    }

    /// 时间戳本身含 `-` 但不含 `_`，所以必须按**第一个** `_` 切分；反过来
    /// （按最后一个）会把时间戳当 id。
    #[test]
    fn key_should_split_on_the_first_underscore() {
        assert_eq!(
            key_from_file_name("2026-01-02T15-04-05_ab_cd.jsonl"),
            Some("ab_cd".to_string())
        );
    }

    #[test]
    fn sanitize_should_block_path_separators_and_dotdot() {
        assert_eq!(sanitize_key("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(sanitize_key("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_key(".."), "unknown");
        assert_eq!(sanitize_key(""), "unknown");
    }

    #[test]
    fn sanitize_should_cap_the_length() {
        assert_eq!(sanitize_key(&"a".repeat(200)).len(), 64);
    }
}
