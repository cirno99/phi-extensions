//! 可逆吸收的原文仓库。
//!
//! # 为什么需要它
//!
//! 原版（billion-context / acp-kernel）的 absorb 是**可逆**的：被吸收的工具输出
//! 仍留在宿主历史里，`decompress` 随时能取回，因此「吸收」只花上下文、不丢信息。
//! phi 上扩展拿不到历史，只能在 `tool_result` 拦截时**替换**模型看到的那条消息，
//! 原文一旦不另存就永久丢失——模型只能重跑工具（`read` / `build` 的代价往往比
//! 省下的 token 还大）。
//!
//! 于是这里把被吸收的原文落到扩展自己的数据目录 `state/absorbed/<handle>.txt`，
//! stub 里带上句柄，模型可用 `acp_decompress <handle>` 逐字取回。
//!
//! 正文**不写进 `state.json`**：状态文件每 turn 都可能原子重写，把几百 KB 原文
//! 塞进去会让每次落盘都变慢；单独成文件后，写入只发生一次。
//!
//! 仓库有上限（[`MAX_ENTRIES`]），超出时按插入顺序淘汰最旧的文件——句柄一旦
//! 被淘汰，`acp_decompress` 会明确告知「已过期」，而不是返回错误内容。

use std::cell::{Cell, RefCell};
use std::fs;
use std::path::PathBuf;

use phi_ext_common::paths;

thread_local! {
    /// 是否允许落盘。单测（[`crate::runtime::Runtime::new_isolated`]）会关掉，
    /// 避免把测试数据写进用户真实的 `state/absorbed` 目录。
    static ENABLED: Cell<bool> = const { Cell::new(true) };
    /// 目录覆盖（仅单测使用：把仓库指向临时目录）。
    static DIR_OVERRIDE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// 开关原文落盘（单测用）。
pub fn set_enabled(on: bool) {
    ENABLED.with(|flag| flag.set(on));
}

/// 当前是否允许落盘。
pub fn is_enabled() -> bool {
    ENABLED.with(|flag| flag.get())
}

/// 覆盖仓库目录（仅单测使用；传 `None` 恢复默认）。
pub fn set_dir_override(dir: Option<PathBuf>) {
    DIR_OVERRIDE.with(|slot| *slot.borrow_mut() = dir);
}

/// 保留的可逆吸收条目上限（超出淘汰最旧的）。
///
/// 取 256：一次长会话里真正会被回头查阅的巨型输出通常只有几十条，
/// 留 256 条既覆盖实际回查需求，又给磁盘用量封了顶。
pub const MAX_ENTRIES: usize = 256;

/// 扩展名（也是数据目录名），与 `config::EXTENSION_NAME` 保持一致。
const EXTENSION_NAME: &str = "phi-acp";

// 会话键：可逆吸收的原文**按会话**分目录存放。
//
// # 为什么必须按会话分
//
// 句柄编号（`next_absorb_id`）是 [`crate::state::CompressionState`] 的一部分，
// 而状态自本版本起按会话分文件（见 [`crate::session`]）。若原文仍共用一个目录，
// 两个会话的 `a1` 会写到**同一个文件**上：后写入的静默覆盖先写入的，
// `acp_decompress a1` 于是返回**别的会话的内容**——比找不到更糟。
//
// 键未知（`SessionStart` 前、或解析不出会话文件）时退回旧的共享目录，
// 保证功能不因此失效；键一旦确定（首个 turn 结束前必然确定）就切到会话目录。
thread_local! {
    static SESSION_KEY: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// 设置当前会话键（由 [`crate::runtime::Runtime`] 维护）。
pub fn set_session_key(key: Option<String>) {
    SESSION_KEY.with(|slot| *slot.borrow_mut() = key);
}

/// 当前会话键。
pub fn session_key() -> Option<String> {
    SESSION_KEY.with(|slot| slot.borrow().clone())
}

/// 原文仓库目录：`<phi_home>/extensions/phi-acp/state/absorbed[ /<会话键>]`。
pub fn dir() -> PathBuf {
    // 单测的目录覆盖充当**基目录**（而不是直接返回），这样按会话分目录的逻辑在
    // 单测里同样生效，同时仍然不会碰到用户真实目录。
    let base = DIR_OVERRIDE
        .with(|slot| slot.borrow().clone())
        .unwrap_or_else(|| paths::extension_state_dir(EXTENSION_NAME).join("absorbed"));
    match session_key() {
        Some(key) => base.join(key),
        None => base,
    }
}

fn path_for(handle: &str) -> PathBuf {
    dir().join(format!("{handle}.txt"))
}

/// 写入一条原文。失败（或单测禁用）时返回 `None`（调用方应回退为「不保留」语义）。
pub fn store(handle: &str, content: &str) -> Option<PathBuf> {
    if !is_enabled() {
        return None;
    }
    let dir = dir();
    if fs::create_dir_all(&dir).is_err() {
        return None;
    }
    let path = path_for(handle);
    fs::write(&path, content).ok()?;
    Some(path)
}

/// 读取一条原文；不存在（含被淘汰）时返回 `None`。
pub fn load(handle: &str) -> Option<String> {
    fs::read_to_string(path_for(handle)).ok()
}

/// 删除一条原文（句柄被淘汰时调用）。单测禁用时不动手，避免误删真实文件。
pub fn remove(handle: &str) {
    if !is_enabled() {
        return;
    }
    let _ = fs::remove_file(path_for(handle));
}

/// 清空整个仓库（`/acp reset` 时调用）。单测禁用时不动手。
pub fn clear() {
    if !is_enabled() {
        return;
    }
    let _ = fs::remove_dir_all(dir());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把仓库指向一个进程唯一的临时目录，返回清理闭包。
    fn with_temp_dir<T>(f: impl FnOnce() -> T) -> T {
        let dir = std::env::temp_dir().join(format!(
            "phi-acp-absorb-store-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        set_dir_override(Some(dir.clone()));
        set_enabled(true);
        let out = f();
        set_enabled(false);
        set_dir_override(None);
        let _ = fs::remove_dir_all(&dir);
        out
    }

    #[test]
    fn handle_should_round_trip_via_store() {
        with_temp_dir(|| {
            let content = "原始工具输出\n第二行";
            assert!(store("a1", content).is_some());
            assert_eq!(load("a1").as_deref(), Some(content));
            remove("a1");
            assert!(load("a1").is_none());
        });
    }

    #[test]
    fn missing_handle_should_return_none() {
        with_temp_dir(|| {
            assert!(load("a_does_not_exist_123").is_none());
        });
    }

    #[test]
    fn disabled_store_should_not_write() {
        set_enabled(false);
        assert!(store("a1", "x").is_none());
        assert!(load("a1").is_none());
    }

    /// 句柄编号是**每会话**的，因此原文也必须按会话分目录：否则两个会话的 `a1`
    /// 会写到同一个文件上，`acp_decompress a1` 返回**别的会话的内容**。
    #[test]
    fn dir_should_be_scoped_by_session() {
        set_dir_override(None);
        set_session_key(None);
        let shared = dir();

        set_session_key(Some("sess-a".to_string()));
        assert_eq!(dir(), shared.join("sess-a"));

        set_session_key(Some("sess-b".to_string()));
        assert_ne!(dir(), shared.join("sess-a"));

        // 键未知时退回共享目录（功能不因此失效）。
        set_session_key(None);
        assert_eq!(dir(), shared);
    }

    /// 同一句柄在两个会话里互不覆盖。
    #[test]
    fn handles_should_not_collide_across_sessions() {
        with_temp_dir(|| {
            set_session_key(Some("sess-a".to_string()));
            store("a1", "content from A");
            set_session_key(Some("sess-b".to_string()));
            store("a1", "content from B");

            assert_eq!(load("a1").as_deref(), Some("content from B"));
            set_session_key(Some("sess-a".to_string()));
            assert_eq!(load("a1").as_deref(), Some("content from A"));
            set_session_key(None);
        });
    }
}
