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
//   当前用户消息的**末尾**（见 internal/agent/engine.go 的 Loop）。
// - 完整锚点只在会话首轮（以及上下文压缩后）注入，其余轮次改用
//   build_anchor_reminder 的短提醒，见该函数注释。
// - 风格规则改为**正面白名单**（`We need to ...` / `I will ...` / `I am ...`），不写
//   「不要用某个开头词」这类负向禁令——负向提及本身会把模型往那个开头词上引。

/// 锚点提示里用于「是否已注入」判定的稳定标记。
pub const ANCHOR_MARKER: &str = "First sentence rule (mandatory";

/// 每轮风格提醒里用于「是否已注入」判定的稳定标记。
pub const ANCHOR_REMINDER_MARKER: &str = "Reasoning style reminder (still in effect";

/// `thinking` 开始标签的字面量。
///
/// 拆成 `concat!` 书写：本仓库此前的编辑链路会把字面量 `<thinking>` 整段吞掉，
/// 导致锚点退化成 `thinkingWe need to ...` 这种残缺、自相矛盾的指令
/// （模型看到的是「写进 thinking 标签」却拿到一个不成形的标签示例）。
const THINK_OPEN: &str = concat!("<", "thinking>");

/// `thinking` 结束标签的字面量（拆开书写的原因同 `THINK_OPEN`）。
const THINK_CLOSE: &str = concat!("</", "thinking>");

/// 判断一段文本是否已经包含锚点或风格提醒标记。
pub fn contains_anchor(text: &str) -> bool {
    text.contains(ANCHOR_MARKER) || text.contains(ANCHOR_REMINDER_MARKER)
}

/// 生成锚点提示。`tool_note` 描述当前会话可直接调用的工具集。
pub fn build_anchor_prompt(tool_note: &str) -> String {
    format!(
        "**First sentence rule (mandatory, highest priority; first rule of this prompt): the first sentence inside `thinking` must open with `We need to ...` / `We need ...`. Every following sentence must open with one of exactly three patterns: `We need to ...` / `We need ...`, `I will ...`, `I am ...` / `I'm ...`. Nothing else.**\n\n\
You are a helpful software engineer assistant. We need to follow this style for all internal reasoning (chain-of-thought):\n\n\
1. **Only three openers exist.** Every sentence opens with `We need to ...` / `We need ...`, `I will ...`, or `I am ...` / `I'm ...`. Any other opening word is a violation.\n\
2. **`We need to ...` leads.** It opens the first sentence and stays the default opener; switch to `I will ...` for a committed next step and `I am ...` / `I'm ...` for what is happening right now.\n\
3. **One concrete action per sentence.** Decision-level summaries only; short and colloquial, We / I perspective.\n\
4. **Classify every task first.** We need to pick a stable end: build (produce, verify, fix) · fix (read, locate, minimal change, verify) · weak (classify first, then build or fix).\n\
5. **Think tag.** We need each reasoning step written inside the reasoning block: {THINK_OPEN}We need to ...{THINK_CLOSE}. Never write that block — or any reasoning text — into the final reply.\n\
6. **Scope.** We need this to shape reasoning only. Final replies follow the user's language and tone.\n\n\
Required shape (illustrative; do not copy the wording): `We need to read the failing test first. I will open the test file. I am reusing the existing helper. We need to keep the diff minimal.`\n\n\
**Tool access.** {tool_note}"
    )
}

/// 生成每轮的极简风格提醒。
///
/// 完整锚点只在会话首轮（以及压缩后）注入一次，随后会被后续对话历史淹没。
/// 模型对「离上下文尾部远的指令」遵循度衰减很快，这正是本扩展「很少能让
/// DeepSeek 进入 We-need 思维链」的主因；因此在其余轮次里把规则压成一段
/// 贴在当前用户消息末尾的短提醒，持续把风格拉回来。
///
/// `minimal` 为真时额外附带当前的「工具访问」说明：完整锚点只在首轮注入，而
/// `/deepseek minimal on|off` 可能在会话中途改变守卫状态，把白名单贴到每轮提醒里
/// 才能保证「守卫实际拦什么」与「模型被告知什么」始终一致。
///
/// - `minimal`：Eternal Minimal 运行时守卫是否开启。
/// - `direct`：minimal 下允许直呼的工具名（按序）。
pub fn build_anchor_reminder(minimal: bool, direct: &[String]) -> String {
    let reminder = format!(
        "**{ANCHOR_REMINDER_MARKER}):** open every sentence of your reasoning with one of exactly \
three patterns — `We need to ...` / `We need ...`, `I will ...`, `I am ...` / `I'm ...`; the first \
sentence of `thinking` must be `We need to ...`. One concrete action per sentence. Classify the \
task first, then act. The final reply follows the user's language and tone and carries no reasoning text."
    );
    if minimal {
        format!("{reminder}\n\n**Tool access.** {}", tool_note(true, direct))
    } else {
        reminder
    }
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
        assert!(prompt.contains("`I will ...`"));
        assert!(prompt.contains("`I am ...`"));
        assert!(prompt.contains("`thinking`"));
    }

    /// 回归：锚点与每轮提醒里都不得再出现被替换掉的默认开头词——
    /// 负向提及（「不要写 X」）本身会把模型往 X 上引。
    #[test]
    fn prompt_and_reminder_should_not_name_the_replaced_opener() {
        let prompt = build_anchor_prompt(&tool_note(false, &[]));
        assert!(!prompt.to_lowercase().contains("let me"));
        assert!(!build_anchor_reminder(false, &[])
            .to_lowercase()
            .contains("let me"));
    }

    /// 回归：锚点里的 thinking 标签必须是成形的一对，不能被吞掉左尖括号。
    #[test]
    fn anchor_should_spell_out_well_formed_think_tag() {
        let prompt = build_anchor_prompt(&tool_note(false, &[]));
        assert!(prompt.contains("<thinking>We need to ...</thinking>"));
        assert!(!prompt.contains("`thinkingWe need"));
    }

    #[test]
    fn reminder_should_carry_its_own_marker() {
        let reminder = build_anchor_reminder(false, &[]);
        assert!(reminder.contains(ANCHOR_REMINDER_MARKER));
        assert!(reminder.contains("We need to ..."));
        assert!(reminder.contains("`I will ...`"));
        assert!(reminder.contains("`I am ...`"));
        assert!(!reminder.contains(ANCHOR_MARKER));
    }

    #[test]
    fn reminder_should_carry_tool_note_in_minimal_mode() {
        let direct = vec!["bash".to_string(), "str_replace_editor".to_string()];
        let reminder = build_anchor_reminder(true, &direct);
        assert!(reminder.contains("Eternal Minimal"));
        assert!(reminder.contains("bash, str_replace_editor"));
        // 非 minimal 下不追加工具说明，避免每轮白送一段冗余提示。
        assert!(!build_anchor_reminder(false, &direct).contains("Eternal Minimal"));
    }

    #[test]
    fn contains_anchor_should_detect_marker_and_reminder() {
        assert!(contains_anchor(&build_anchor_prompt("note")));
        assert!(contains_anchor(&build_anchor_reminder(false, &[])));
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
