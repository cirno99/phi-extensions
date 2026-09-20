//! 保护规则 —— 对应 acp-kernel `src/protected.ts`。

use std::collections::BTreeSet;

use crate::types::{Config, ContentType, CoreMessage};

/// 永远受保护的工具（ACP 自身的元数据工具）。
pub const ALWAYS_PROTECTED_TOOLS: &[&str] = &["compress", "acp_rule"];

/// 永不进入「软保护最近区」的工具结果。
pub const NEVER_PRESERVE_RECENT_TOOLS: &[&str] = &["decompress", "search_context", "read", "bash"];

/// 工具名模式匹配：以 `*` 结尾时按前缀匹配，否则全等。
pub fn match_tool_pattern(tool_name: &str, pattern: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('*') {
        return tool_name.starts_with(prefix);
    }
    tool_name == pattern
}

fn is_tool_message(message: &CoreMessage) -> bool {
    matches!(
        message.content_type,
        ContentType::ToolCall | ContentType::ToolResult
    )
}

/// 该消息是否属于「永不进入最近保护区」的工具。
pub fn is_never_preserve_recent(message: &CoreMessage) -> bool {
    if !is_tool_message(message) {
        return false;
    }
    let Some(name) = message.tool_name.as_deref() else {
        return false;
    };
    NEVER_PRESERVE_RECENT_TOOLS.contains(&name)
}

/// 该工具消息是否受保护（硬保护 + 用户配置的模式）。
pub fn is_message_protected(message: &CoreMessage, config: &Config) -> bool {
    if !is_tool_message(message) {
        return false;
    }
    let Some(name) = message.tool_name.as_deref() else {
        return false;
    };
    if ALWAYS_PROTECTED_TOOLS.contains(&name) {
        return true;
    }
    config
        .protected_tools
        .iter()
        .any(|pattern| match_tool_pattern(name, pattern))
}

/// 收集受保护工具调用的 toolCallId（用于按配对保护其工具结果）。
pub fn collect_protected_tool_call_ids(
    messages: &[CoreMessage],
    config: &Config,
) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    for message in messages {
        if message.content_type == ContentType::ToolCall {
            if let Some(call_id) = message.tool_call_id.as_deref() {
                if !call_id.is_empty() && is_message_protected(message, config) {
                    ids.insert(call_id.to_string());
                }
            }
        }
    }
    ids
}

/// 带配对的保护判定。
pub fn is_message_protected_with_pairing(
    message: &CoreMessage,
    config: &Config,
    protected_call_ids: &BTreeSet<String>,
) -> bool {
    if is_message_protected(message, config) {
        return true;
    }
    if message.content_type == ContentType::ToolResult {
        if let Some(call_id) = message.tool_call_id.as_deref() {
            if protected_call_ids.contains(call_id) {
                return true;
            }
        }
    }
    false
}

/// 「仅最新实例受保护」的收集结果。
#[derive(Debug, Clone, Default)]
pub struct LatestProtected {
    /// 最新调用的 toolCallId。
    pub call_ids: BTreeSet<String>,
    /// 无 toolCallId 的最新调用消息 id。
    pub msg_ids: BTreeSet<String>,
}

/// 为每个 `protectedLatestTools` 模式收集最后一个匹配调用及其配对。
pub fn collect_latest_protected(messages: &[CoreMessage], config: &Config) -> LatestProtected {
    let mut latest = LatestProtected::default();
    if config.protected_latest_tools.is_empty() {
        return latest;
    }
    for pattern in &config.protected_latest_tools {
        let mut last: Option<&CoreMessage> = None;
        for message in messages {
            if message.content_type == ContentType::ToolCall {
                if let Some(name) = message.tool_name.as_deref() {
                    if match_tool_pattern(name, pattern) {
                        last = Some(message);
                    }
                }
            }
        }
        if let Some(message) = last {
            match message.tool_call_id.as_deref() {
                Some(call_id) if !call_id.is_empty() => {
                    latest.call_ids.insert(call_id.to_string());
                }
                _ => {
                    latest.msg_ids.insert(message.id.clone());
                }
            }
        }
    }
    latest
}

/// 是否为「仅最新受保护」的调用或与其配对的结果。
pub fn is_message_latest_protected(message: &CoreMessage, latest: &LatestProtected) -> bool {
    if message.content_type == ContentType::ToolCall && latest.msg_ids.contains(&message.id) {
        return true;
    }
    if is_tool_message(message) {
        if let Some(call_id) = message.tool_call_id.as_deref() {
            if latest.call_ids.contains(call_id) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Role;

    fn tool_call(id: &str, name: &str, call_id: &str) -> CoreMessage {
        CoreMessage {
            id: id.into(),
            role: Role::Assistant,
            content_type: ContentType::ToolCall,
            tool_name: Some(name.into()),
            tool_call_id: Some(call_id.into()),
            ..Default::default()
        }
    }

    fn tool_result(id: &str, call_id: &str) -> CoreMessage {
        CoreMessage {
            id: id.into(),
            role: Role::Tool,
            content_type: ContentType::ToolResult,
            tool_call_id: Some(call_id.into()),
            ..Default::default()
        }
    }

    #[test]
    fn match_tool_pattern_should_support_prefix_glob() {
        assert!(match_tool_pattern("read", "read"));
        assert!(!match_tool_pattern("read", "rea"));
        assert!(match_tool_pattern("skill_apply", "skill*"));
        assert!(!match_tool_pattern("skills", "read*"));
    }

    #[test]
    fn always_protected_tools_should_be_protected() {
        let config = Config::default_for(1000);
        assert!(is_message_protected(
            &tool_call("1", "compress", "c1"),
            &config
        ));
        assert!(is_message_protected(
            &tool_call("1", "acp_rule", "c1"),
            &config
        ));
    }

    #[test]
    fn pairing_should_protect_result_by_call_id() {
        let mut config = Config::default_for(1000);
        config.protected_tools = vec!["read".into()];
        let messages = vec![
            tool_call("c", "read", "call-1"),
            tool_result("r", "call-1"),
            tool_call("c2", "bash", "call-2"),
            tool_result("r2", "call-2"),
        ];
        let ids = collect_protected_tool_call_ids(&messages, &config);
        assert!(ids.contains("call-1"));
        assert!(is_message_protected_with_pairing(
            &messages[1],
            &config,
            &ids
        ));
        assert!(!is_message_protected_with_pairing(
            &messages[3],
            &config,
            &ids
        ));
    }

    #[test]
    fn latest_protected_should_keep_only_newest() {
        let mut config = Config::default_for(1000);
        config.protected_latest_tools = vec!["todo_list".into()];
        let messages = vec![
            tool_call("c1", "todo_list", "call-1"),
            tool_call("c2", "todo_list", "call-2"),
            tool_result("r2", "call-2"),
        ];
        let latest = collect_latest_protected(&messages, &config);
        assert!(!latest.call_ids.contains("call-1"));
        assert!(latest.call_ids.contains("call-2"));
        assert!(!is_message_latest_protected(&messages[0], &latest));
        assert!(is_message_latest_protected(&messages[2], &latest));
    }

    #[test]
    fn never_preserve_recent_should_match_listed_tools() {
        let config = Config::default_for(1000);
        let _ = config;
        assert!(is_never_preserve_recent(
            &tool_result("r", "call-1").clone_for("bash")
        ));
        assert!(!is_never_preserve_recent(&CoreMessage::text(
            "t",
            Role::User,
            "hi"
        )));
    }

    impl CoreMessage {
        fn clone_for(self, name: &str) -> Self {
            Self {
                tool_name: Some(name.into()),
                ..self
            }
        }
    }
}
