// main.rs — phi-rtk-optimizer 扩展入口。
//
// 由 pi 版 pi-rtk-optimizer 的 src/index.ts 移植。
//
// pi → phi 的钩子映射：
// - `tool_call`（bash 命令重写）→ `on_tool_call` 回写 `input`，等价。
// - `tool_result`（输出压缩）→ `on_tool_result` 回写 `content`，等价。
// - `before_agent_start`（注入源码过滤排障注记）→ `on_before_agent_start`
//   的 `system_prompt_append`。
// - `session_start` / `agent_end` → `subscribe`，用于重载配置与清理跟踪表。
//
// 受 phi 宿主能力限制而无法移植的部分（详见 PLAN.md）：
// - `tool_execution_update`（流式输出清洗）与 `tool_execution_end`（结果清洗）
//   在 phi 无对应事件；压缩只在 `tool_result` 做一次。
// - 改写提示 / 缺 rtk 告警无法从拦截回调弹 toast，改为缓存后由 `/rtk` 展示。

mod commands;
mod compactor;
mod config;
mod metrics;
mod rewriter;
mod runtime;
mod techniques;

use phi_ext::{phi, pxb};
use phi_ext_common::json::{Value, ValueAsMutObject, ValueAsScalar, ValueObjectAccess};

use config::RtkMode;
use rewriter::{
    apply_rewritten_command_shell_safety_fixups, apply_rtk_command_environment,
    apply_windows_bash_compatibility_fixes, compute_rewrite_decision,
};
use runtime::Shared;

/// 改写提示里命令的截断长度。
const NOTICE_COMMAND_CHARS: usize = 100;
/// 改写提示里结果命令的截断长度。
const NOTICE_RESULT_CHARS: usize = 120;
/// 告警细节的截断长度。
const NOTICE_DETAIL_CHARS: usize = 120;

/// 源码过滤可能影响后续编辑时注入的排障注记。
const SOURCE_FILTER_TROUBLESHOOTING_NOTE: &str = "RTK note: If file edits repeatedly fail because old text does not match, ask the user to manually run '/rtk' in the Phi TUI, disable 'Read compaction enabled', re-read the file, apply the edit, then ask the user to manually re-enable it in the Phi TUI.";

fn main() -> Result<(), phi::Error> {
    let mut ext = phi::Extension::new(config::EXTENSION_NAME, env!("CARGO_PKG_VERSION"));
    let shared = runtime::shared();

    commands::register(&mut ext, shared.clone());
    register_tool_call(&mut ext, shared.clone());
    register_tool_result(&mut ext, shared.clone());
    register_before_agent_start(&mut ext, shared.clone());
    register_events(&mut ext, shared);

    ext.run()
}

/// 空白折叠 + 按字符截断（对应 pi 的 `trimMessage`）。
fn trim_message(raw: &str, max_chars: usize) -> String {
    let clean = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if clean.chars().count() <= max_chars {
        return clean;
    }
    let kept: String = clean.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// 是否需要注入源码过滤排障注记。
fn should_inject_source_filter_note(config: &config::RtkIntegrationConfig) -> bool {
    let compaction = &config.output_compaction;
    config.enabled
        && compaction.enabled
        && compaction.read_compaction.enabled
        && compaction.source_code_filtering_enabled
        && compaction.source_code_filtering != config::SourceFilterLevel::None
        && (compaction.smart_truncate.enabled || compaction.truncate.enabled)
}

/// 把改写后的命令写回工具入参 JSON。
fn with_command(input: &Value, command: &str) -> Option<Vec<u8>> {
    let mut input = input.clone();
    let object = input.as_object_mut()?;
    let _ = object.insert("command".to_string(), Value::String(command.to_string()));
    phi_ext_common::json::to_vec(&input).ok()
}

/// `tool_call`：bash 命令重写。
fn register_tool_call(ext: &mut phi::Extension, shared: Shared) {
    ext.on_tool_call(move |ev| {
        if ev.tool_name != "bash" {
            return None;
        }
        let mut guard = shared.borrow_mut();
        if !guard.config.enabled {
            return None;
        }

        let input = phi_ext_common::json::value(&ev.input).ok()?;
        let command = input.get("command").and_then(|value| value.as_str())?.to_string();
        if command.trim().is_empty() {
            return None;
        }
        // 供 tool_result 阶段在入参缺 command 时回填。
        guard.track_bash_command(&ev.tool_call_id, Some(&command));

        let platform = std::env::consts::OS;
        let mut next_command = command.clone();

        // Windows bash 兼容修正（仅 rewrite 模式，与 pi 版一致）。
        if guard.config.mode == RtkMode::Rewrite {
            next_command = apply_windows_bash_compatibility_fixes(&command, platform).command;
        }

        guard.ensure_status_fresh();
        if guard.should_skip_when_rtk_missing() {
            return (next_command != command)
                .then(|| with_command(&input, &next_command))
                .flatten()
                .map(|input| phi::ToolCallResult {
                    input: Some(input),
                    ..Default::default()
                });
        }

        let executable = guard.status.executable.clone();
        let decision = compute_rewrite_decision(&next_command, platform, 3_000, executable.as_ref());

        if !decision.changed {
            if let Some(warning) = decision.warning.clone() {
                guard.push_notice(format!(
                    "{}: rtk rewrite skipped for '{}' ({}).",
                    config::EXTENSION_NAME,
                    trim_message(&decision.original_command, NOTICE_COMMAND_CHARS),
                    trim_message(&warning, NOTICE_DETAIL_CHARS)
                ));
            }
            return (next_command != command)
                .then(|| with_command(&input, &next_command))
                .flatten()
                .map(|input| phi::ToolCallResult {
                    input: Some(input),
                    ..Default::default()
                });
        }

        if guard.config.mode == RtkMode::Rewrite {
            let env_scoped = apply_rtk_command_environment(&decision.rewritten_command, platform);
            let final_command = apply_rewritten_command_shell_safety_fixups(&env_scoped, platform);
            if guard.config.show_rewrite_notifications {
                guard.push_notice(format!(
                    "RTK rewrite: {} -> {}",
                    trim_message(&decision.original_command, NOTICE_COMMAND_CHARS),
                    trim_message(&final_command, NOTICE_RESULT_CHARS)
                ));
            }
            let rewritten = with_command(&input, &final_command)?;
            return Some(phi::ToolCallResult {
                input: Some(rewritten),
                ..Default::default()
            });
        }

        // suggest 模式：只提示，不改写。
        let key = format!(
            "{}:{}",
            decision.original_command, decision.rewritten_command
        );
        if guard.remember_suggestion(&key) {
            guard.push_notice(format!("RTK suggestion: {}", decision.rewritten_command));
        }
        (next_command != command)
            .then(|| with_command(&input, &next_command))
            .flatten()
            .map(|input| phi::ToolCallResult {
                input: Some(input),
                ..Default::default()
            })
    });
}

/// `tool_result`：工具输出压缩。
fn register_tool_result(ext: &mut phi::Extension, shared: Shared) {
    ext.on_tool_result(move |ev| {
        let mut guard = shared.borrow_mut();

        // bash 命令跟踪：入参里通常已有 command，缺失时用跟踪值补齐。
        let tracked = guard.tracked_bash_command(&ev.tool_call_id).map(str::to_string);
        if ev.tool_name == "bash" {
            guard.forget_bash_command(&ev.tool_call_id);
        }

        if !guard.config.enabled || !guard.config.output_compaction.enabled {
            return None;
        }

        let mut input = phi_ext_common::json::value(&ev.input).unwrap_or_default();
        if ev.tool_name == "bash" && input.get("command").is_none() {
            if let (Some(command), Some(map)) = (tracked, input.as_object_mut()) {
                map.insert("command".to_string(), Value::String(command));
            }
        }

        let outcome = guard.compact(&ev.tool_name, &input, &ev.content);

        if !outcome.changed {
            return None;
        }
        Some(phi::ToolResultResult {
            content: Some(outcome.text),
            context: String::new(),
            stop: false,
            reason: String::new(),
        })
    });
}

/// `before_agent_start`：必要时追加源码过滤排障注记。
fn register_before_agent_start(ext: &mut phi::Extension, shared: Shared) {
    ext.on_before_agent_start(move |_ev| {
        let mut guard = shared.borrow_mut();
        guard.ensure_status_fresh();
        guard.maybe_warn_rtk_missing();
        if !should_inject_source_filter_note(&guard.config) {
            return None;
        }
        Some(phi::BeforeAgentStartResult {
            prompt: None,
            system_prompt_append: SOURCE_FILTER_TROUBLESHOOTING_NOTE.to_string(),
        })
    });
}

/// 生命周期事件：重载配置、清理跟踪表。
fn register_events(ext: &mut phi::Extension, shared: Shared) {
    {
        let shared = shared.clone();
        ext.subscribe(pxb::Event::SessionStart, move |_ev| {
            shared.borrow_mut().on_session_start();
        });
    }
    {
        let shared = shared.clone();
        ext.subscribe(pxb::Event::AgentEnd, move |_ev| {
            shared.borrow_mut().clear_bash_commands();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phi_ext_common::json::json;

    #[test]
    fn trim_message_should_collapse_whitespace_and_truncate() {
        assert_eq!(trim_message("  a \n b  ", 100), "a b");
        let long = "x".repeat(50);
        let trimmed = trim_message(&long, 10);
        assert_eq!(trimmed.chars().count(), 10);
        assert!(trimmed.ends_with('…'));
    }

    #[test]
    fn with_command_should_replace_only_command_field() {
        let input = json!({ "command": "ls", "timeout": 5 });
        let bytes = with_command(&input, "rtk ls").expect("应序列化成功");
        let value = phi_ext_common::json::value(&bytes).expect("应反序列化成功");
        assert_eq!(value["command"], json!("rtk ls"));
        assert_eq!(value["timeout"], json!(5));
    }

    #[test]
    fn with_command_should_reject_non_object_input() {
        assert!(with_command(&json!([1, 2]), "x").is_none());
    }

    #[test]
    fn source_filter_note_should_require_all_conditions() {
        let mut config = config::RtkIntegrationConfig::default();
        assert!(!should_inject_source_filter_note(&config));

        config.output_compaction.read_compaction.enabled = true;
        config.output_compaction.source_code_filtering_enabled = true;
        config.output_compaction.source_code_filtering = config::SourceFilterLevel::Minimal;
        config.output_compaction.truncate.enabled = true;
        assert!(should_inject_source_filter_note(&config));

        config.enabled = false;
        assert!(!should_inject_source_filter_note(&config));
    }

    #[test]
    fn source_filter_note_should_be_english_for_prompt_stability() {
        // 注记会进入系统提示词，保持稳定文本避免缓存抖动。
        assert!(SOURCE_FILTER_TROUBLESHOOTING_NOTE.starts_with("RTK note:"));
    }
}