// store.rs — 会话状态与开关的 JSON 持久化。
//
// 由 pi 版 asymptotic-thinking 扩展的 src/session-store.ts 移植。
// 与 pi 版的差异：
// - 原实现用 append-only SQLite（取最新 id），这里改为单文件 JSON 原子覆写。
// - phi 的拦截钩子拿不到 session_id（`Extension.host` 为私有字段且 `run` 会消费自身），
//   因此状态按扩展实例单文件存储；session_id 由 main.rs 通过 subscribe(SessionStart) 记录后
//   仅用于展示与校验，不参与文件分片。

use std::path::{Path, PathBuf};

use phi_ext_common::config::{load_or_default, load_strict, save_atomic, ConfigError};
use phi_ext_common::paths;

use crate::types::SessionState;

/// 扩展名（同时作为 `~/.phi/extensions/<name>/` 的目录名）。
pub const EXTENSION_NAME: &str = "phi-asymptotic-thinking";

/// 返回当前 Unix 毫秒时间戳（复用共享实现）。
pub use phi_ext_common::time::now_ms;

/// 状态与开关的持久化句柄。
pub struct Store {
    state_path: PathBuf,
    toggle_path: PathBuf,
}

impl Store {
    /// 在指定目录下创建句柄（目录不存在时由写入侧创建）。
    pub fn new(dir: &Path) -> Self {
        Self {
            state_path: dir.join("state.json"),
            toggle_path: dir.join("enabled.json"),
        }
    }

    /// 使用扩展的标准状态目录 `~/.phi/extensions/<name>/state/`。
    pub fn default_location() -> Self {
        Self::new(&paths::extension_state_dir(EXTENSION_NAME))
    }

    /// 读取会话状态；文件缺失或损坏时返回默认状态。
    pub fn load(&self) -> SessionState {
        load_or_default::<SessionState>(&self.state_path)
    }

    /// 原子写入会话状态。
    pub fn save(&self, state: &SessionState) -> Result<(), ConfigError> {
        save_atomic(&self.state_path, state)
    }
    /// 读取开关状态（默认启用，与 pi 版一致）。
    pub fn is_enabled(&self) -> bool {
        #[derive(serde::Deserialize)]
        struct Toggle {
            enabled: bool,
        }
        match load_strict::<Toggle>(&self.toggle_path) {
            Ok(Some(toggle)) => toggle.enabled,
            _ => true,
        }
    }

    /// 写入开关状态。
    pub fn set_enabled(&self, enabled: bool) -> Result<(), ConfigError> {
        #[derive(serde::Serialize)]
        struct Toggle {
            enabled: bool,
        }
        save_atomic(&self.toggle_path, &Toggle { enabled })
    }
}

impl Default for Store {
    fn default() -> Self {
        Self::default_location()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Difficulty, MasterTaskType, State, SubTaskType};

    /// 创建进程唯一的临时目录（测试用，不做自动清理以外的额外处理）。
    fn temp_store(name: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("phi-asym-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Store::new(&dir)
    }

    #[test]
    fn load_should_return_default_state_when_file_missing() {
        let store = temp_store("missing");
        let state = store.load();
        assert_eq!(state.state, Some(State::Start));
        assert_eq!(state.task_turn_count, 0);
    }

    #[test]
    fn save_then_load_should_round_trip_state() {
        let store = temp_store("roundtrip");
        let mut state = store.load();
        state.state = Some(State::Execute);
        state.difficulty = Some(Difficulty::Hard);
        state.master_task_type = Some(MasterTaskType::Coding);
        state.sub_task_type = Some(SubTaskType::Testing);
        state.state_turn_count = 12;
        store.save(&state).expect("写入应成功");
        assert_eq!(store.load(), state);
    }

    #[test]
    fn enabled_should_default_to_true() {
        let store = temp_store("toggle-default");
        assert!(store.is_enabled());
    }

    #[test]
    fn set_enabled_should_persist_flag() {
        let store = temp_store("toggle-set");
        store.set_enabled(false).expect("写入应成功");
        assert!(!store.is_enabled());
        store.set_enabled(true).expect("写入应成功");
        assert!(store.is_enabled());
    }

    #[test]
    fn now_ms_should_return_non_zero_timestamp() {
        assert!(now_ms() > 1_600_000_000_000);
    }
}
