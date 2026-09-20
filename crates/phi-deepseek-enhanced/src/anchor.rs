// anchor.rs — "We need to ..." 推理风格锚点提示。
//
// 逐条移植自 Oh My Pi 版 deepseek-enhanced.ts 的 buildAnchorPrompt：
// 保留 6 条链式思考（CoT）风格规则与「首句必须 We need to」硬性规则。
//
// 与 pi 版的差异：
// - pi 版锚点里还内嵌了 `xd://` 高阶工具目录（读 xd://<tool> 取 schema、
//   写 xd://<tool> 执行）。phi 既无法枚举工具清单（无 GetAllTools），也无法
//   落地 xd:// 网关（扩展不能调用别的工具），因此这里改为一段随配置变化的
//   「工具访问」说明，见 build_anchor_prompt。
// - pi 版把锚点作为隐藏的 provider-only 用户消息注入；phi 只能通过
//   `on_before_agent_start` 的 `system_prompt_append` 追加，宿主会把它并进
//   当前用户消息（见 internal/agent/engine.go）。

/// 锚点提示里用于「是否已注入」判定的稳定标记。
pub const ANCHOR_MARKER: &str = "First sentence rule (mandatory";

/// 判断一段文本是否已经包含锚点标记。
pub fn contains_anchor(text: &str) -> bool {
    text.contains(ANCHOR_MARKER)
}

/// 生成锚点提示。`tool_note` 描述当前会话可直接调用的工具集。
pub fn build_anchor_prompt(tool_note: &str) -> String {
    format!(
        "**First sentence rule (mandatory, highest priority; first rule of this prompt): the first sentence inside `thinking` must open with `We need to ...` / `We need ...`. Following sentences keep `We need to ...` / `We need ...` as the core pattern, one concrete action each, and may interleave I'll (next action) · I can (viable option) · I should (what ought to be done) · I will (committed step).**\n\n\
You are a helpful software engineer assistant. We need to follow this style for all internal reasoning (chain-of-thought):\n\n\
1. **`We need to ...` / `We need ...` is the core pattern for every following sentence.** Either can open any sentence, not only the first. We need one concrete action per sentence. Interleave modal verbs: I'll (next action) · I can (viable option) · I need (what must be done) · I should (what ought to be done) · I will (committed step) · I'm / I am (what I am about to do).\n\
2. **Avoid `let me ...`.** We need to prefer `We need to ...` / `We need ...` for opening steps.\n\
3. **Short and colloquial.** We need one sentence per step, decision-level summaries only, We / I perspective.\n\
4. **Classify every task first.** We need to pick a stable end: build (produce, verify, fix) · fix (read, locate, minimal change, verify) · weak (classify first, then build or fix).\n\
5. **Think tag.** We need each reasoning step written inside the thinking tag: `thinkingWe need to ...</think>`. Never output `thinking` tags or reasoning text in the final reply.\n\
6. **Scope.** We need this to shape reasoning only. Final replies follow the user's language and tone.\n\n\
**Tool access.** {tool_note}"
    )
}

/// 生成「工具访问」说明段。
///
/// - `minimal`：Eternal Minimal 运行时守卫是否开启。
/// - `direct`：minimal 下允许直呼的工具名（按序）。
pub fn tool_note(minimal: bool, direct: &[String]) -> String {
    if minimal {
        let list = if direct.is_empty() {
            "（无）".to_string()
        } else {
            direct.join(", ")
        };
        format!(
            "This session runs in Eternal Minimal: the only directly callable tools are {list}. \
Do not call any other tool name directly, even if it appears familiar; a direct call will be blocked. \
Use the allowed tools for shell and file work."
        )
    } else {
        "All host tools remain directly callable. Prefer bash and str_replace_editor for shell and file work.".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_should_contain_marker_and_first_sentence_rule() {
        let prompt = build_anchor_prompt(&tool_note(false, &[]));
        assert!(prompt.contains(ANCHOR_MARKER));
        assert!(prompt.contains("We need to ..."));
        assert!(prompt.contains("Avoid `let me ...`"));
        assert!(prompt.contains("`thinking`"));
    }

    #[test]
    fn contains_anchor_should_detect_marker() {
        assert!(contains_anchor(&build_anchor_prompt("note")));
        assert!(!contains_anchor("plain user prompt"));
    }

    #[test]
    fn minimal_note_should_list_direct_tools() {
        let note = tool_note(true, &["bash".to_string(), "str_replace_editor".to_string()]);
        assert!(note.contains("bash, str_replace_editor"));
        assert!(note.contains("Eternal Minimal"));
    }

    #[test]
    fn default_note_should_not_claim_minimal() {
        let note = tool_note(false, &[]);
        assert!(!note.contains("Eternal Minimal"));
    }
}