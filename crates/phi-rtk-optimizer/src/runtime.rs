// runtime.rs — RTK 扩展运行期共享状态。
//
// phi 的拦截/订阅回调不带 `Context`（无法 notify / set_status），因此所有
// 跨回调状态收敛到 `Rc<RefCell<Runtime>>`，并且「本该弹 toast 的告警」改为
// 记入 `pending_notices`，等下一次 `/rtk` 命令时统一刷给用户。

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use phi_ext_common::arena::Scratch;
use phi_ext_common::time::now_ms;

use crate::compactor::{self, CompactionOutcome};
use crate::config::{self, RtkIntegrationConfig};
use crate::metrics::OutputMetrics;
use crate::rewriter::{resolve_rtk_executable, run_with_timeout, RtkExecutableResolution};

/// rtk 可用性探测的新鲜度窗口。
const STATUS_STALE_AFTER: Duration = Duration::from_secs(30);

/// 有界「只提醒一次」集合：超过上限时按插入顺序淘汰最旧的键。
#[derive(Debug)]
pub struct BoundedNoticeTracker {
    limit: usize,
    seen: HashSet<String>,
    order: VecDeque<String>,
}

impl BoundedNoticeTracker {
    /// 创建容量为 `max_entries` 的追踪器（至少 1）。
    pub fn new(max_entries: usize) -> Self {
        Self {
            limit: max_entries.max(1),
            seen: HashSet::new(),
            order: VecDeque::new(),
        }
    }

    /// 首次见到该键返回 `true`（应当提醒），重复返回 `false`。
    pub fn remember(&mut self, key: &str) -> bool {
        if self.seen.contains(key) {
            return false;
        }
        self.seen.insert(key.to_string());
        self.order.push_back(key.to_string());
        while self.order.len() > self.limit {
            if let Some(evicted) = self.order.pop_front() {
                self.seen.remove(&evicted);
            }
        }
        true
    }

    /// 清空。
    pub fn reset(&mut self) {
        self.seen.clear();
        self.order.clear();
    }
}

/// rtk 运行期状态。
#[derive(Debug, Clone, Default)]
pub struct RuntimeStatus {
    /// rtk 是否可用。
    pub rtk_available: bool,
    /// 上次探测时间（Unix 毫秒）。
    pub last_checked_at: Option<u64>,
    /// 上次探测的失败原因。
    pub last_error: Option<String>,
    /// 可执行文件解析结果。
    pub executable: Option<RtkExecutableResolution>,
}

/// 扩展运行期共享状态。
pub struct Runtime {
    /// 当前配置。
    pub config: RtkIntegrationConfig,
    /// 配置文件路径。
    pub config_path: PathBuf,
    /// 配置解析告警（待 `/rtk` 展示）。
    pub config_warning: Option<String>,
    /// rtk 可用性状态。
    pub status: RuntimeStatus,
    /// 输出压缩收益统计。
    pub metrics: OutputMetrics,
    /// 已提醒过的消息（去重）。
    warned: BoundedNoticeTracker,
    /// 已提示过的改写建议（去重）。
    suggestions: BoundedNoticeTracker,
    /// 待展示给用户的告警（回调无法 notify，只能缓存）。
    pending_notices: Vec<String>,
    /// 进行中的 bash 命令（toolCallId → command），用于 tool_result 阶段判定命令类型。
    active_bash_commands: HashMap<String, String>,
    /// 复用的竞技场：单次输出压缩的临时内存，调用结束整体释放。
    scratch: Scratch,
    /// 缺 rtk 告警是否已发过。
    missing_rtk_warning_shown: bool,
}

impl Runtime {
    /// 创建运行时并加载配置。
    pub fn new() -> Self {
        let path = config::config_path();
        let loaded = config::load(&path);
        Self {
            config: loaded.config,
            config_path: path,
            config_warning: loaded.warning,
            status: RuntimeStatus::default(),
            metrics: OutputMetrics::default(),
            warned: BoundedNoticeTracker::new(100),
            suggestions: BoundedNoticeTracker::new(200),
            pending_notices: Vec::new(),
            active_bash_commands: HashMap::new(),
            scratch: Scratch::with_capacity(16 * 1024),
            missing_rtk_warning_shown: false,
        }
    }

    /// 压缩一次工具结果。
    ///
    /// 竞技场与配置/统计同属 `Runtime`：用字段级解构避免每轮
    /// `config.clone()`（配置里有多个 `Vec<String>`），也不与 `RefCell` 打架。
    pub fn compact(
        &mut self,
        tool_name: &str,
        input: &serde_json::Value,
        content: &str,
    ) -> CompactionOutcome {
        let Runtime {
            config,
            metrics,
            scratch,
            ..
        } = self;
        let outcome = compactor::compact_tool_result(
            scratch.arena(),
            tool_name,
            input,
            content,
            config,
            Some(metrics),
        );
        // 临时内存整体释放，底层 chunk 保留给下一次调用复用。
        scratch.finish();
        outcome
    }

    /// 重新读取配置（配置不存在时先写出默认值）。
    pub fn reload_config(&mut self) {
        if let Err(err) = config::ensure_exists(&self.config_path) {
            self.push_notice(format!(
                "{}: failed to create {}: {err}",
                config::EXTENSION_NAME,
                self.config_path.display()
            ));
        }
        let loaded = config::load(&self.config_path);
        self.config = loaded.config;
        self.config_warning = loaded.warning;
        if let Some(warning) = self.config_warning.clone() {
            self.push_notice(warning);
        }
    }

    /// 探测 rtk 是否可用。
    pub fn refresh_status(&mut self) {
        let resolution = resolve_rtk_executable(std::env::consts::OS, 1_000);
        let checked_at = now_ms();
        match run_with_timeout(&resolution.command, &["--version"], 5_000) {
            Ok(output) if output.code == 0 => {
                self.status = RuntimeStatus {
                    rtk_available: true,
                    last_checked_at: Some(checked_at),
                    last_error: None,
                    executable: Some(resolution),
                };
                self.missing_rtk_warning_shown = false;
            }
            Ok(output) => {
                let detail = format!(
                    "{} {} (exit {})",
                    output.stderr.trim(),
                    output.stdout.trim(),
                    output.code
                );
                self.status = RuntimeStatus {
                    rtk_available: false,
                    last_checked_at: Some(checked_at),
                    last_error: Some(detail.split_whitespace().collect::<Vec<_>>().join(" ")),
                    executable: Some(resolution),
                };
            }
            Err(err) => {
                self.status = RuntimeStatus {
                    rtk_available: false,
                    last_checked_at: Some(checked_at),
                    last_error: Some(err),
                    executable: Some(resolution),
                };
            }
        }
    }

    /// 若 `guardWhenRtkMissing` 开启且状态过期，则重新探测。
    pub fn ensure_status_fresh(&mut self) {
        if !self.config.guard_when_rtk_missing {
            return;
        }
        let now = now_ms();
        let stale = match self.status.last_checked_at {
            Some(checked) => now.saturating_sub(checked) > STATUS_STALE_AFTER.as_millis() as u64,
            None => true,
        };
        if stale {
            self.refresh_status();
        }
    }

    /// 缺 rtk 时是否需要跳过命令处理。
    pub fn should_skip_when_rtk_missing(&self) -> bool {
        self.config.guard_when_rtk_missing && !self.status.rtk_available
    }

    /// 缓存一条待展示告警（去重）。
    pub fn push_notice(&mut self, message: String) {
        if self.warned.remember(&message) {
            self.pending_notices.push(message);
        }
    }

    /// 取出并清空待展示告警。
    pub fn take_notices(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_notices)
    }

    /// 记录一次改写建议（同一建议只提示一次）。
    pub fn remember_suggestion(&mut self, key: &str) -> bool {
        self.suggestions.remember(key)
    }

    /// 缺 rtk 告警（每次会话只发一次）。
    pub fn maybe_warn_rtk_missing(&mut self) {
        if !self.config.enabled || !self.config.guard_when_rtk_missing {
            return;
        }
        if self.status.rtk_available {
            self.missing_rtk_warning_shown = false;
            return;
        }
        if self.missing_rtk_warning_shown {
            return;
        }
        self.missing_rtk_warning_shown = true;
        let reason = self
            .status
            .last_error
            .as_deref()
            .map(|detail| format!(" ({detail})"))
            .unwrap_or_default();
        let handling = if self.config.mode == crate::config::RtkMode::Suggest {
            "rewrite suggestions"
        } else {
            "command rewrite"
        };
        self.push_notice(format!(
            "{}: rtk binary unavailable, {handling} bypassed{reason}.",
            config::EXTENSION_NAME
        ));
    }

    /// 记录进行中的 bash 命令。
    pub fn track_bash_command(&mut self, tool_call_id: &str, command: Option<&str>) {
        if tool_call_id.is_empty() {
            return;
        }
        match command {
            Some(command) if !command.trim().is_empty() => {
                self.active_bash_commands
                    .insert(tool_call_id.to_string(), command.to_string());
            }
            _ => {
                self.active_bash_commands.remove(tool_call_id);
            }
        }
    }

    /// 查询进行中的 bash 命令。
    pub fn tracked_bash_command(&self, tool_call_id: &str) -> Option<&str> {
        self.active_bash_commands.get(tool_call_id).map(String::as_str)
    }

    /// 忘记进行中的 bash 命令。
    pub fn forget_bash_command(&mut self, tool_call_id: &str) {
        self.active_bash_commands.remove(tool_call_id);
    }

    /// 清空进行中的 bash 命令（会话开始 / agent 结束时）。
    pub fn clear_bash_commands(&mut self) {
        self.active_bash_commands.clear();
    }

    /// 会话开始时的状态重置。
    pub fn on_session_start(&mut self) {
        self.warned.reset();
        self.suggestions.reset();
        self.clear_bash_commands();
        self.missing_rtk_warning_shown = false;
        self.reload_config();
        self.refresh_status();
        self.maybe_warn_rtk_missing();
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
    fn bounded_tracker_should_evict_oldest() {
        let mut tracker = BoundedNoticeTracker::new(2);
        assert!(tracker.remember("a"));
        assert!(tracker.remember("b"));
        assert!(!tracker.remember("a"));
        // 插入 c 会淘汰 a
        assert!(tracker.remember("c"));
        assert!(tracker.remember("a"));
        assert!(!tracker.remember("c"));
    }

    #[test]
    fn bounded_tracker_should_reset() {
        let mut tracker = BoundedNoticeTracker::new(4);
        assert!(tracker.remember("a"));
        tracker.reset();
        assert!(tracker.remember("a"));
    }

    #[test]
    fn bounded_tracker_should_force_minimum_capacity() {
        let mut tracker = BoundedNoticeTracker::new(0);
        assert!(tracker.remember("a"));
        assert!(!tracker.remember("a"));
    }

    #[test]
    fn push_notice_should_deduplicate() {
        let mut runtime = Runtime::new();
        runtime.push_notice("boom".to_string());
        runtime.push_notice("boom".to_string());
        assert_eq!(runtime.take_notices(), vec!["boom".to_string()]);
        assert!(runtime.take_notices().is_empty());
    }

    #[test]
    fn bash_command_tracking_should_round_trip() {
        let mut runtime = Runtime::new();
        runtime.track_bash_command("c1", Some("cargo build"));
        assert_eq!(runtime.tracked_bash_command("c1"), Some("cargo build"));
        runtime.track_bash_command("c1", None);
        assert_eq!(runtime.tracked_bash_command("c1"), None);
    }

    #[test]
    fn should_skip_when_rtk_missing_should_respect_guard_flag() {
        let mut runtime = Runtime::new();
        runtime.config.guard_when_rtk_missing = false;
        runtime.status.rtk_available = false;
        assert!(!runtime.should_skip_when_rtk_missing());

        runtime.config.guard_when_rtk_missing = true;
        runtime.status.rtk_available = false;
        assert!(runtime.should_skip_when_rtk_missing());

        runtime.status.rtk_available = true;
        assert!(!runtime.should_skip_when_rtk_missing());
    }
}