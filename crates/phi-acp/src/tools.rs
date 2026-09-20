//! LLM 可调用工具 —— 对应 acp-kernel `src/compress-tools.ts` 与 billion-context
//! 的 `compress` / `decompress` / `search_context` / `status` / `acp_rule` 工具。

use phi_ext::phi;
use serde::de::DeserializeOwned;
use serde::Deserialize;

use crate::config::EXTENSION_NAME;
use crate::runtime::Shared;
use crate::types::{CompressRangeSpec, CompressionState, RuleRecord};

/// 一个压缩范围参数。
#[derive(Debug, Deserialize)]
struct RangeArg {
    #[serde(rename = "startId", alias = "startRef")]
    start_id: String,
    #[serde(rename = "endId", alias = "endRef")]
    end_id: String,
    summary: String,
    #[serde(default)]
    topic: Option<String>,
    #[serde(default, rename = "summaryMaxChars")]
    summary_max_chars: Option<usize>,
}

/// `compress` 工具入参。
#[derive(Debug, Deserialize)]
struct CompressArgs {
    #[serde(default)]
    topic: Option<String>,
    #[serde(default)]
    content: Vec<RangeArg>,
    #[serde(default)]
    ranges: Vec<RangeArg>,
}

/// `decompress` 工具入参。
#[derive(Debug, Deserialize)]
struct DecompressArgs {
    #[serde(rename = "blockId", alias = "block_id", alias = "id")]
    block_id: String,
}

/// `search_context` 工具入参。
#[derive(Debug, Deserialize)]
struct SearchArgs {
    query: String,
}

/// `acp_rule` 工具入参。
#[derive(Debug, Deserialize)]
struct RuleArgs {
    action: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    id: Option<String>,
}

/// 解析工具入参。走 `phi_ext_common::json`（simd-json）：工具入参是每轮最热的
/// JSON 解析路径，simd-json 用 SIMD 做结构定位与转义扫描。
fn parse_args<T: DeserializeOwned>(args: &[u8]) -> Result<T, String> {
    phi_ext_common::json::parse::<T>(args).map_err(|e| format!("invalid arguments: {e}"))
}

/// 注册全部工具。
pub fn register(ext: &mut phi::Extension, shared: Shared) {
    register_compress(ext, shared.clone());
    register_decompress(ext, shared.clone());
    register_search(ext, shared.clone());
    register_status(ext, shared.clone());
    register_rule(ext, shared);
}

fn register_compress(ext: &mut phi::Extension, shared: Shared) {
    let schema = phi_ext_common::json::json!({
        "type": "object",
        "properties": {
            "topic": { "type": "string", "description": "Short topic label for the compressed range." },
            "content": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "startId": { "type": "string", "description": "Start ref, e.g. m00123" },
                        "endId": { "type": "string", "description": "End ref, e.g. m00140" },
                        "summary": { "type": "string" },
                        "topic": { "type": "string" },
                        "summaryMaxChars": { "type": "integer" }
                    },
                    "required": ["startId", "endId", "summary"]
                }
            }
        },
        "required": ["content"]
    });
    let tool = phi::Tool::new(
        "compress",
        "Compress a range of the conversation into a self-contained summary. \
         The summary becomes the only record of the replaced messages — make it complete.",
        phi::Schema::raw(phi_ext_common::json::to_vec(&schema).expect("schema")),
        move |args| {
            let parsed: CompressArgs = parse_args(args)?;
            let mut all = parsed.content;
            all.extend(parsed.ranges);
            if all.is_empty() {
                return Err("compress requires at least one content entry".to_string());
            }
            let ranges: Vec<CompressRangeSpec> = all
                .into_iter()
                .map(|r| CompressRangeSpec {
                    start_ref: r.start_id,
                    end_ref: r.end_id,
                    summary: r.summary,
                    topic: r.topic.or_else(|| parsed.topic.clone()),
                    compress_call_id: None,
                    summary_max_chars: r.summary_max_chars,
                })
                .collect();

            let mut guard = shared.borrow_mut();
            if !guard.config.enabled {
                return Err(format!("{EXTENSION_NAME} is disabled"));
            }
            let before = guard.state.blocks.len();
            let outcome = guard.apply(&ranges);
            let summary = crate::compress::created_blocks_summary(&guard.state, before);
            let mut lines = Vec::new();
            if outcome.result.blocks_created > 0 {
                lines.push(format!(
                    "compressed {} range(s), reclaimed ~{} tokens",
                    outcome.result.blocks_created, outcome.result.tokens_compressed
                ));
                if !summary.is_empty() {
                    lines.push(summary);
                }
            }
            for warning in &outcome.result.warnings {
                lines.push(format!("warning: {warning}"));
            }
            for error in &outcome.result.errors {
                lines.push(format!("error: {error}"));
            }
            if lines.is_empty() {
                lines.push("nothing compressed".to_string());
            }
            Ok(phi::ToolResult {
                content: lines.join("\n"),
                ..Default::default()
            })
        },
    );
    ext.register_tool(tool);
}

fn register_decompress(ext: &mut phi::Extension, shared: Shared) {
    let schema = phi_ext_common::json::json!({
        "type": "object",
        "properties": {
            "blockId": { "type": "string", "description": "Block id, e.g. b3" }
        },
        "required": ["blockId"]
    });
    let tool = phi::Tool::new(
        "acp_decompress",
        "Restore the original content of a compressed block and mark it expanded \
         (it will not be re-folded).",
        phi::Schema::raw(phi_ext_common::json::to_vec(&schema).expect("schema")),
        move |args| {
            let parsed: DecompressArgs = parse_args(args)?;
            let mut guard = shared.borrow_mut();
            let Some(block_id) = crate::decompress::parse_block_id_arg(&parsed.block_id) else {
                return Err(format!("invalid block id: {}", parsed.block_id));
            };
            let preview = {
                let Some(block) = crate::decompress::decompress(&block_id, &guard.state) else {
                    return Err(format!("unknown block: {block_id}"));
                };
                crate::decompress::build_restored_content_preview(block, &guard.messages, 8000)
            };
            crate::decompress::deactivate_block(&mut guard.state, &block_id);
            guard.persist();
            Ok(phi::ToolResult {
                content: format!("expanded {block_id}\n\n{preview}"),
                ..Default::default()
            })
        },
    );
    ext.register_tool(tool);
}

fn register_search(ext: &mut phi::Extension, shared: Shared) {
    let schema = phi_ext_common::json::json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": "Keywords to search compressed blocks" }
        },
        "required": ["query"]
    });
    let tool = phi::Tool::new(
        "acp_search",
        "Search compressed blocks by relevance to recover a fact from earlier work.",
        phi::Schema::raw(phi_ext_common::json::to_vec(&schema).expect("schema")),
        move |args| {
            let parsed: SearchArgs = parse_args(args)?;
            let guard = shared.borrow();
            let hits = crate::decompress::search_blocks(&parsed.query, &guard.state);
            if hits.is_empty() {
                return Ok(phi::ToolResult {
                    content: "no matching blocks".to_string(),
                    ..Default::default()
                });
            }
            let mut lines = Vec::new();
            for block in hits.iter().take(10) {
                lines.push(format!(
                    "{} [tier {}] {}: {}",
                    block.block_id,
                    block.tier,
                    block
                        .topic
                        .clone()
                        .unwrap_or_else(|| "(no topic)".to_string()),
                    block.summary.lines().next().unwrap_or("").trim()
                ));
            }
            Ok(phi::ToolResult {
                content: lines.join("\n"),
                ..Default::default()
            })
        },
    );
    ext.register_tool(tool);
}

fn register_status(ext: &mut phi::Extension, shared: Shared) {
    let schema = phi_ext_common::json::json!({ "type": "object", "properties": {} });
    let tool = phi::Tool::new(
        "acp_status",
        "Report context usage, active/total blocks, and the current compressible ranges.",
        phi::Schema::raw(phi_ext_common::json::to_vec(&schema).expect("schema")),
        move |_args| {
            let mut guard = shared.borrow_mut();
            let outcome = guard.process();
            let tokens = guard.effective_token_count();
            // 这个数字要么来自宿主会话文件（真值），要么是本地估算（只数
            // 用户输入 + 工具结果 + 工具入参，看不到助手正文与推理）。
            // 两者可差 40%，不标出来就会把「估算误差」当成「压缩失败」排查。
            let source =
                if guard.config.use_host_tokens && crate::session_tokens::last_read_used_host() {
                    "host"
                } else {
                    "local estimate"
                };
            let report =
                crate::compress::status(&guard.state, tokens, &guard.config.to_kernel_config());
            let mut lines = vec![format!(
                "context: {:.1}% ({}/{} tokens, {source}) · active blocks {} · total {} · reclaimed {} tokens · absorbed {} tokens",
                report.context_usage * 100.0,
                report.token_count,
                report.model_context_limit,
                report.active_blocks,
                report.total_blocks,
                report.tokens_compressed,
                guard.state.stats.absorbed_tokens
            )];
            if let Some(nudge) = &outcome.nudge {
                let ranges = crate::nudge::format_ranges(
                    &nudge.compressible_ranges,
                    &nudge.protected_ranges,
                );
                lines.push(ranges);
            }
            Ok(phi::ToolResult {
                content: lines.join("\n"),
                ..Default::default()
            })
        },
    );
    ext.register_tool(tool);
}

fn register_rule(ext: &mut phi::Extension, shared: Shared) {
    let schema = phi_ext_common::json::json!({
        "type": "object",
        "properties": {
            "action": { "type": "string", "enum": ["add", "list", "remove", "clear"] },
            "text": { "type": "string" },
            "id": { "type": "string" }
        },
        "required": ["action"]
    });
    let tool = phi::Tool::new(
        "acp_rule",
        "Record a persistent reminder that is re-injected every turn and never compressed.",
        phi::Schema::raw(phi_ext_common::json::to_vec(&schema).expect("schema")),
        move |args| {
            let parsed: RuleArgs = parse_args(args)?;
            let mut guard = shared.borrow_mut();
            match parsed.action.as_str() {
                "add" => {
                    let Some(text) = parsed.text.filter(|t| !t.trim().is_empty()) else {
                        return Err("acp_rule add requires non-empty text".to_string());
                    };
                    let id = {
                        let next = guard.state.next_rule_id.unwrap_or(1);
                        guard.state.next_rule_id = Some(next + 1);
                        format!("rule{next}")
                    };
                    guard.state.rules.push(RuleRecord {
                        id: id.clone(),
                        text,
                    });
                    guard.persist();
                    Ok(phi::ToolResult {
                        content: format!("recorded {id}"),
                        ..Default::default()
                    })
                }
                "remove" => {
                    let Some(id) = parsed.id else {
                        return Err("acp_rule remove requires id".to_string());
                    };
                    let before = guard.state.rules.len();
                    guard.state.rules.retain(|r| r.id != id);
                    guard.persist();
                    Ok(phi::ToolResult {
                        content: format!("removed {} rule(s)", before - guard.state.rules.len()),
                        ..Default::default()
                    })
                }
                "clear" => {
                    guard.state.rules.clear();
                    guard.persist();
                    Ok(phi::ToolResult {
                        content: "cleared all rules".to_string(),
                        ..Default::default()
                    })
                }
                _ => {
                    let mut lines = vec![format!("{} rule(s):", guard.state.rules.len())];
                    for rule in &guard.state.rules {
                        lines.push(format!("  {}: {}", rule.id, rule.text));
                    }
                    Ok(phi::ToolResult {
                        content: lines.join("\n"),
                        ..Default::default()
                    })
                }
            }
        },
    );
    ext.register_tool(tool);
}

/// 供系统提示词注入的持久规则文本。
pub fn format_rules_for_prompt(state: &CompressionState) -> String {
    if state.rules.is_empty() {
        return String::new();
    }
    let mut lines = vec!["Persistent ACP rules (always apply):".to_string()];
    for rule in &state.rules {
        lines.push(format!("- {}", rule.text));
    }
    lines.join("\n")
}
