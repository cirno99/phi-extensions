// runtime.rs — 扩展运行期共享状态。

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use crate::config::{self, CacheOptimizerConfig};

/// 运行期状态。
pub struct Runtime {
    /// 当前配置。
    pub config: CacheOptimizerConfig,
    /// 配置文件路径。
    pub config_path: PathBuf,
    /// 配置解析告警。
    pub config_warning: Option<String>,
}

impl Runtime {
    /// 创建运行时并加载配置。
    pub fn new() -> Self {
        let path = config::config_path();
        let (config, config_warning) = config::load(&path);
        Self {
            config,
            config_path: path,
            config_warning,
        }
    }

    /// 重新读取配置。
    pub fn reload(&mut self) {
        let (config, warning) = config::load(&self.config_path);
        self.config = config;
        self.config_warning = warning;
    }

    /// 保存配置。
    pub fn persist(&self) -> Result<(), String> {
        config::save(&self.config, &self.config_path).map_err(|err| format!("配置保存失败：{err}"))
    }

    /// 一行配置摘要。
    pub fn summary(&self) -> String {
        format!(
            "启用：{} · footer 口径：{:?} · prompt_cache_key：{:?}",
            if self.config.enabled { "是" } else { "否" },
            self.config.footer_mode,
            self.config.prompt_cache_key
        )
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

/// 扩展内部共享状态别名。
pub type Shared = Rc<RefCell<Runtime>>;

/// 创建共享运行时。
pub fn shared() -> Shared {
    Rc::new(RefCell::new(Runtime::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_should_include_all_fields() {
        let runtime = Runtime::new();
        let summary = runtime.summary();
        assert!(summary.contains("启用"));
        assert!(summary.contains("footer 口径"));
        assert!(summary.contains("prompt_cache_key"));
    }

    #[test]
    fn reload_should_keep_defaults_when_file_missing() {
        let mut runtime = Runtime::new();
        runtime.config.enabled = false;
        runtime.reload();
        // 测试环境通常没有配置文件，应回落默认（enabled = true）。
        assert!(runtime.config.enabled);
    }
}