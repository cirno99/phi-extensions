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

/// 原文仓库目录：`<phi_home>/extensions/phi-acp/state/absorbed`。
pub fn dir() -> PathBuf {
    if let Some(overridden) = DIR_OVERRIDE.with(|slot| slot.borrow().clone()) {
        return overridden;
    }
    paths::extension_state_dir(EXTENSION_NAME).join("absorbed")
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
}