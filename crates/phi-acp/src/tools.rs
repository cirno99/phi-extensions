//! LLM 可调用工具 —— 对应 acp-kernel `src/compress-tools.ts` 与 billion-context
//! 的 `compress` / `decompress` / `search_context` / `status` / `acp_rule` 工具。

use phi_ext::phi;
use serde::de::DeserializeOwned;
use serde::Deserialize;

use crate::config::EXTENSION_NAME;
use crate::runtime::Shared;
use crate::types::CompressRangeSpec;

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
    /// 块解压时是否递归到全部原始消息（默认 false：只上溯一层）。
    #[serde(default)]
    full: Option<bool>,
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
            // 先同步一次内核状态：提醒关闭时 `turn_stopping` 不再每轮 `process()`，
            // 而 ref 分配 / 块同步正是在 `process()` 里做的——compress 必须自己保证。
            guard.process();
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
            "blockId": {
                "type": "string",
                "description": "Compressed block id (b3) or absorb handle (a5)"
            },
            "full": {
                "type": "boolean",
                "description": "For a block id: recurse to the original messages instead of one tier up (default false)."
            }
        },
        "required": ["blockId"]
    });
    let tool = phi::Tool::new(
        "acp_decompress",
        "Restore compressed content for exact details. Accepts a compressed block id (`b3` — \
         returns the block's content; by default one tier up, set full:true to recurse all the \
         way to the original messages) or an absorb handle (`a5` — restores a tool output that \
         was elided with an `[acp absorb]` marker, verbatim). Stateless: the block stays \
         compressed, so repeat calls are free.",
        phi::Schema::raw(phi_ext_common::json::to_vec(&schema).expect("schema")),
        move |args| {
            let parsed: DecompressArgs = parse_args(args)?;
            let guard = shared.borrow();
            // 可逆吸收句柄：直接从磁盘仓库取回逐字原文（不必重跑工具）。
            if let Some(handle) = crate::absorb::parse_absorb_handle(&parsed.block_id) {
                if crate::state::absorbed_output_by_handle(&guard.state, &handle).is_none() {
                    return Err(format!(
                        "unknown absorb handle: {handle} (it may have been evicted)"
                    ));
                }
                let Some(content) = crate::absorb_store::load(&handle) else {
                    return Err(format!(
                        "absorbed output {handle} is no longer stored (evicted from disk)"
                    ));
                };
                return Ok(phi::ToolResult {
                    content: format!("restored {handle}\n\n{content}"),
                    ..Default::default()
                });
            }
            let Some(block_id) = crate::decompress::parse_block_id_arg(&parsed.block_id) else {
                return Err(format!("invalid block id: {}", parsed.block_id));
            };
            let Some(block) = crate::decompress::decompress(&block_id, &guard.state) else {
                return Ok(phi::ToolResult {
                    content: format!("[Block {block_id} not found]"),
                    ..Default::default()
                });
            };
            // 无状态解压：块保持压缩，摘要留在原位，只把内容以文本返回。不修改
            // 状态（对应上游「STATELESS RETRIEVAL」），因此不会产生 expand/re-fold
            // 循环，重复解压也是免费的。
            let full = parsed.full.unwrap_or(false);
            let (collected, count) = crate::decompress::collect_block_content(
                &guard.state,
                block,
                &guard.messages,
                full,
            );
            let body = if collected.is_empty() {
                block.summary.clone()
            } else {
                collected
            };
            let header = format!(
                "[Block {block_id} content — {count} item(s){}]",
                if full { ", full" } else { "" }
            );
            Ok(phi::ToolResult {
                content: crate::decompress::render_decompress_output(&header, &block_id, &body),
                ..Default::default()
            })
        },
    );
    ext.register_tool(tool);
}

/// `acp_search` 一次返回的条数（对应上游 `executeSearchContext` 的默认 limit 5）。
const SEARCH_LIMIT: usize = 5;

/// 渲染一条检索结果。
///
/// 块沿用上游 `executeSearchContext` 的行格式（`bN (Tn) "topic"` + 缩进预览）；
/// 消息命中额外标出角色与「压缩掉它的块」，模型据此决定解压哪个块。
fn render_search_hit(hit: &crate::search::SearchResult) -> String {
    use crate::search::{MessageRole, SearchDocKind};
    match hit.kind {
        SearchDocKind::Block => format!(
            "{} (T{}) \"{}\"\n  {}",
            hit.reference, hit.tier, hit.title, hit.preview
        ),
        SearchDocKind::Message => {
            let role = hit.role.map(MessageRole::as_str).unwrap_or("message");
            let owner = hit
                .block_id
                .as_deref()
                .map(|block| format!(" · {block}"))
                .unwrap_or_default();
            format!("{} ({role}{owner})\n  {}", hit.reference, hit.preview)
        }
    }
}

fn register_search(ext: &mut phi::Extension, shared: Shared) {
    let schema = phi_ext_common::json::json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": "Keywords to search compressed blocks and folded history" }
        },
        "required": ["query"]
    });
    let tool = phi::Tool::new(
        "acp_search",
        "Search compressed blocks and folded history by relevance to recover a fact from earlier work.",
        phi::Schema::raw(phi_ext_common::json::to_vec(&schema).expect("schema")),
        move |args| {
            let parsed: SearchArgs = parse_args(args)?;
            let query = parsed.query.trim();
            if query.is_empty() {
                return Err("query is required".to_string());
            }
            let guard = shared.borrow();
            // 上游生产路径：hybrid（0.7·BM25 + 0.3·fuzzy）对「全部块（含失活）
            // + 历史消息」统一打分，再按角色加权。
            let options = crate::search::SearchOptions {
                limit: Some(SEARCH_LIMIT),
                ..Default::default()
            };
            let hits = crate::search::search_state(&guard.state, &guard.messages, query, &options);
            if hits.is_empty() {
                // 区分「根本没有块」（搜索无意义，显式提示以阻断过早重试循环）
                // 与「有块但没匹配」——对应上游 search_context。
                let any_active = guard.state.blocks.iter().any(|b| b.active);
                let content = if !any_active {
                    "[No compressed blocks exist yet — nothing to search.]".to_string()
                } else {
                    format!("[No blocks matched \"{query}\"]")
                };
                return Ok(phi::ToolResult {
                    content,
                    ..Default::default()
                });
            }
            let mut lines = vec![format!("Found {} result(s) for \"{query}\":", hits.len())];
            lines.extend(hits.iter().map(render_search_hit));
            Ok(phi::ToolResult {
                content: lines.join("\n\n"),
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
            let observed = guard.estimate_tokens();
            // 会话键：状态是**按会话**存的（见 `crate::session`），报出来才能解释
            // 「为什么块账本是空的」——新会话本来就应该是空的。
            let session = guard.session_key().unwrap_or("unknown");
            let mut lines = vec![format!(
                "context: {:.1}% ({}/{} tokens, {source}) · session {session} · observed view ~{observed} tokens · active blocks {} · total {} · reclaimed {} tokens · absorbed {} tokens ({} reversible handles)",
                report.context_usage * 100.0,
                report.token_count,
                report.model_context_limit,
                report.active_blocks,
                report.total_blocks,
                report.tokens_compressed,
                guard.state.stats.absorbed_tokens,
                guard.state.absorbed_outputs.len()
            )];
            let mut compressible_empty = false;
            if let Some(nudge) = &outcome.nudge {
                compressible_empty = nudge.compressible_ranges.is_empty();
                let ranges = crate::nudge::format_ranges(
                    &nudge.compressible_ranges,
                    &nudge.protected_ranges,
                );
                lines.push(ranges);
            }
            // 超过限额且无内容可压时，不要把「无可压缩范围」当成「去压更多」——
            // 在 phi 上 compress 改不了宿主请求体，这个观察视图又只覆盖用户输入 +
            // 工具结果（看不到助手正文 / 推理 / 被压缩的历史），因此可能远小于真实
            // 上下文。如实告知，并指向真正能减小的通道。
            if report.context_usage >= 1.0 && compressible_empty {
                lines.push(
                    "⚠️ Over the configured limit with nothing compressible in this extension's \
                     view. On phi `compress` cannot shrink the host request — the view only \
                     covers user input + tool results (assistant text/reasoning and compacted \
                     history are invisible), so it can be far smaller than the real context. \
                     The host only runs native compaction at a turn that ends WITHOUT tool \
                     calls (engine.go: `len(msg.ToolCalls)==0`), so a long tool-call loop grows \
                     the context monotonically no matter what context_window is set. Real \
                     levers: absorb (already maximally aggressive at this usage), and END THE \
                     TURN with a plain text reply so the host can compact."
                        .to_string(),
                );
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
        "Record a persistent reminder that survives compression (add/list/remove/clear).",
        phi::Schema::raw(phi_ext_common::json::to_vec(&schema).expect("schema")),
        move |args| {
            let parsed: RuleArgs = parse_args(args)?;
            let mut guard = shared.borrow_mut();
            // 校验失败是**正常结果**（对应上游 `executeRule`：把内核错误原文回给
            // 模型让它自己修），不是工具错误。
            let content = match parsed.action.as_str() {
                "add" => {
                    let Some(text) = parsed.text.as_deref() else {
                        return Err("acp_rule add requires non-empty text".to_string());
                    };
                    match crate::rules::add_rule(&mut guard.state, text) {
                        Ok(rule) => {
                            guard.persist();
                            format!("Recorded {}: {}", rule.id, rule.text)
                        }
                        Err(error) => error,
                    }
                }
                "remove" => {
                    let Some(id) = parsed.id.as_deref() else {
                        return Err("acp_rule remove requires id".to_string());
                    };
                    match crate::rules::remove_rule(&mut guard.state, id) {
                        Ok(rule) => {
                            guard.persist();
                            format!("Removed {}: {}", rule.id, rule.text)
                        }
                        Err(error) => error,
                    }
                }
                "clear" => {
                    let count = crate::rules::clear_rules(&mut guard.state);
                    guard.persist();
                    format!("cleared {count} rule(s)")
                }
                _ => {
                    if guard.state.rules.is_empty() {
                        "No rules recorded.".to_string()
                    } else {
                        crate::rules::format_rules_list(&guard.state)
                    }
                }
            };
            Ok(phi::ToolResult {
                content,
                ..Default::default()
            })
        },
    );
    ext.register_tool(tool);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::{MessageRole, SearchDocKind, SearchResult};

    fn hit(kind: SearchDocKind, reference: &str) -> SearchResult {
        SearchResult {
            kind,
            reference: reference.to_string(),
            block_id: None,
            tier: 1,
            score: 1.0,
            title: "title".to_string(),
            preview: "…needle…".to_string(),
            role: None,
            tokens: None,
        }
    }

    #[test]
    fn render_search_hit_should_format_blocks_like_upstream() {
        let mut block = hit(SearchDocKind::Block, "b3");
        block.tier = 2;
        block.title = "auth token".to_string();
        assert_eq!(
            render_search_hit(&block),
            "b3 (T2) \"auth token\"\n  …needle…"
        );
    }

    #[test]
    fn render_search_hit_should_link_message_hits_to_their_owning_block() {
        let mut message = hit(SearchDocKind::Message, "m00350");
        message.role = Some(MessageRole::User);
        message.block_id = Some("b3".to_string());
        assert_eq!(
            render_search_hit(&message),
            "m00350 (user · b3)\n  …needle…"
        );

        // 尚未被任何块折叠的消息没有拥有块。
        message.block_id = None;
        assert_eq!(render_search_hit(&message), "m00350 (user)\n  …needle…");
    }
}
