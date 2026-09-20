// templates.rs — 六态提示词组装（v3：START/END 重构 + 全链路路径感知 + 输出精简）
//
// 由 pi 版 asymptotic-thinking 扩展的 src/templates.ts 移植。
// 与 pi 版的差异：原实现用正则做两处清洗，这里改为等价的按行处理，避免引入 regex 依赖。

use crate::prompts;
use crate::state_machine::{format_next_state_hint, max_state_turns};
use crate::types::{Difficulty, MasterTaskType, State, SubTaskType, TOOL_TASK_INFO, TOOL_TRANSITION};

/// 扩展 SYSTEM.md 框架规则（编译期内联，来自 assets/framework-rules.md）。
pub const FRAMEWORK_RULES: &str = include_str!("../assets/framework-rules.md");

/// 任务描述：`编程类-Rust开发 · 复杂`。
pub fn task_desc(
    master: Option<MasterTaskType>,
    sub: Option<SubTaskType>,
    diff: Option<Difficulty>,
) -> String {
    let (Some(master), Some(diff)) = (master, diff) else {
        return "尚未设定".to_string();
    };
    let sub_label = match sub {
        Some(s) => format!("-{}", s.label()),
        None => String::new(),
    };
    format!("{}{} · {}", master.label(), sub_label, diff.label())
}

/// 大类领域要点提示（未设定大类时为空串）。
pub fn task_hint(master: Option<MasterTaskType>) -> &'static str {
    match master {
        Some(m) => m.hint(),
        None => "",
    }
}

/// 清洗提示词模块返回值，与 pi 版两处正则等价。
///
/// 1. 去掉各模块自带的「当前为xxx（xxx难度）。」开头行（避免与 taskBlock 重复）。
/// 2. 去掉各模块自带的 transition 流转指令行（统一由 actions 管理）。
fn clean_prompt_content(raw: &str) -> String {
    let mut lines: Vec<&str> = raw.lines().collect();

    let should_strip_head = lines
        .first()
        .is_some_and(|line| line.starts_with("当前为") && line.contains("难度）。"));
    if should_strip_head {
        lines.remove(0);
        if lines.first().is_some_and(|line| line.trim().is_empty()) {
            lines.remove(0);
        }
    }

    lines
        .into_iter()
        .filter(|line| !line.contains(TOOL_TRANSITION))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// 获取领域提示词内容（画像未设定时返回提示语）。
pub fn prompt_content(
    master: Option<MasterTaskType>,
    sub: Option<SubTaskType>,
    diff: Option<Difficulty>,
    state: State,
) -> String {
    let (Some(master), Some(sub), Some(diff)) = (master, sub, diff) else {
        return format!("等待模型调用 {TOOL_TASK_INFO} 工具 设定任务信息...");
    };
    clean_prompt_content(prompts::build_prompt(master, sub, diff, state))
}

/// 全链路路径感知：按 visited 动态生成适配段。
pub fn path_adapter(state: State, visited: &[State]) -> &'static str {
    // 直达 EXECUTE：无前置理解/设计（TRIVIAL 最短路径）。
    if state == State::Execute && !visited.contains(&State::Design) {
        return "> ⚡ 本任务直达执行模式：未经过深度理解与方案设计。请先自行明确需求要点与执行步骤，再直接实施。";
    }
    // 跳理解到 DESIGN（SIMPLE 跳 DEEP_UNDERSTAND）。
    if state == State::Design && !visited.contains(&State::DeepUnderstand) {
        return "> ⚡ 本任务未经过深度理解。请先简要明确需求边界，再列出方案。";
    }
    // 回退到 DESIGN（曾进入 EXECUTE 后回退重做）。
    if state == State::Design && visited.contains(&State::Execute) {
        return "> ⚡ 曾进入 EXECUTE 后回退。方案需覆盖已执行部分的调整与修正。";
    }
    ""
}

/// 反引号字符（模板里用于包裹工具名）。
const BT: char = '\u{60}';

/// 组装六态提示词模板。
pub fn build_template(
    state: State,
    turn: u32,
    master: Option<MasterTaskType>,
    sub: Option<SubTaskType>,
    diff: Option<Difficulty>,
    visited: &[State],
) -> String {
    let max_turns = max_state_turns(state, diff);
    let state_label = state.label();
    let status_line = format!("第{turn}/{max_turns}轮 {state_label}阶段");
    let status_line_with_task = format!(
        "{status_line} · {}",
        task_desc(master, sub, diff)
    );

    // START（启动：评估 + 设定画像）。
    if state == State::Start {
        let task_turn_line = format!("第task{turn}轮任务启动");
        return format!(
            "<task{turn}>\n\n<instruction spec=\"markdown\">\n{status_line}\n{task_turn_line}\n\n遵守《渐近式思考状态机操作规范》\n\n本次任务目标是深度评估调用所有tool工具与skill技能，获取外部信息与本地记忆，挖掘用户指令隐藏信息。\n深度评估是否需要使用 web_search 网络搜索、web_fetch 网页获取，必须给出简要评估结论（节省输出token）。\n深入分析用户指令的任务类型和难度，为后续阶段做准备。\n\n工作纪律：\n- 每轮开始先判断当前进度对应状态机的哪个阶段；状态不符时先调用 {BT}{TOOL_TRANSITION}{BT} 工具流转到对应状态，再继续执行\n- 轮次超限且需等待用户决策时，停止当前操作与轮次，待用户下次输入再继续\n</instruction>\n\n<actions spec=\"markdown\">\n- 调用 {BT}{TOOL_TASK_INFO}{BT} 工具设定任务画像\n</actions>\n\n</task{turn}>"
        );
    }

    // END（任务终态：极简）。
    if state == State::End {
        return format!(
            "<task{turn}>\n\n<instruction spec=\"markdown\">\n{status_line}\n\n遵守《渐近式思考状态机操作规范》\n\n上一任务已完成，保持空闲等待新指令，不执行任何修改操作。\n</instruction>\n\n</task{turn}>"
        );
    }

    let hint = task_hint(master);
    let content = prompt_content(master, sub, diff, state);
    let path_segment = path_adapter(state, visited);
    let flow_hint = format_next_state_hint(Some(state), diff);

    let mut task_block = String::from("<instruction spec=\"markdown\">\n");
    task_block.push_str(&status_line_with_task);
    task_block.push_str("\n遵守《渐近式思考状态机操作规范》");
    if !hint.is_empty() {
        task_block.push_str("\n> ");
        task_block.push_str(hint);
    }
    if !path_segment.is_empty() {
        task_block.push('\n');
        task_block.push_str(path_segment);
    }
    if !content.is_empty() {
        task_block.push_str("\n\n");
        task_block.push_str(&content);
    }
    task_block.push_str("\n\n> 🔄 可用流转：");
    task_block.push_str(&flow_hint);
    task_block.push_str("\n</instruction>");
    let transition_call = format!("调用 {BT}{TOOL_TRANSITION}{BT} 工具流转状态");
    let middle = match state {
        State::DeepUnderstand => format!(
            "\n<actions spec=\"markdown\">\n- 逐条列出需求的功能边界、约束条件和验收标准\n- 缺失信息用工具检索补齐，不猜测\n- 完成本阶段职责后，{transition_call}\n</actions>\n\n<constraints spec=\"markdown\">\n- 只读文件辅助理解，不执行写操作\n</constraints>"
        ),
        State::Design => format!(
            "\n<actions spec=\"markdown\">\n- 方案按顺序列出：涉及文件 → 前置步骤 → 实施路径 → 验收标准\n- 每个步骤写明具体工具名和文件路径\n- 完成本阶段职责后，{transition_call}\n</actions>\n\n<constraints spec=\"markdown\">\n- 设计完成前不执行任何代码修改\n</constraints>"
        ),
        State::Execute => format!(
            "\n<actions spec=\"markdown\">\n- 严格按方案步骤顺序执行\n- 每步执行后自检结果，确认通过再推进下一步\n- 方案不可行时回退 DESIGN 重新设计\n- 完成本阶段职责后，{transition_call}\n</actions>"
        ),
        State::Verify => format!(
            "\n<output spec=\"markdown\">\n| 维度 | 状态 | 说明 |\n|:--|:--:|:--|\n| 需求理解正确完整 | ✅/❌ | ... |\n| 实施步骤全部完成 | ✅/❌ | ... |\n| 输出格式符合规范 | ✅/❌ | ... |\n</output>\n\n<actions spec=\"markdown\">\n- 逐项如实标注 ✅ 或 ❌\n- 有 ❌ 项修正后全部重新验证\n- 全部 ✅ 后{transition_call}\n</actions>"
        ),
        State::Start | State::End => String::new(),
    };
    format!("<task{turn}>\n\n{task_block}\n{middle}\n\n</task{turn}>")
}

/// 生成 `<stateGuard>` 命令语（turn_end 常规提醒的头部）。
pub fn state_guard(state: State, state_turn_count: u32, max_turns: u32) -> String {
    format!(
        "<stateGuard>\n渐近式思考·强制执行：当前处于【{}】状态（第{state_turn_count}/{max_turns}轮）。\n本阶段必须完成该状态职责后，调用 {BT}{TOOL_TRANSITION}{BT} 工具流转状态。\n未调用前请勿结束回复；若阶段未完成请说明原因后继续推进。\n</stateGuard>",
        state.label()
    )
}

/// 生成 `<violationWarning>` 违规提示（本轮未调用 transition 时使用）。
pub fn violation_warning() -> String {
    [
        "\n<violationWarning>⚠️ 本轮未调用 ",
        TOOL_TRANSITION,
        " 工具。若状态职责已完成，必须立即流转；若未完成，说明原因后继续。</violationWarning>",
    ]
    .concat()
}

/// 非 START 状态下的续作提醒，防止模型被新消息打断。
pub fn continuation_notice(state: State) -> String {
    format!(
        "\n\n> ⚠️ 上一次对话未完成任务，不可中断——请回到当前【{}】状态，继续把未完成的流程推进到底。\n严格遵守人格角色定义，严格遵守用户指令。",
        state.label()
    )
}

/// 用 `<task{N}>` 包裹提醒文本，隔离上下文。
pub fn wrap_task(turn: u32, reminder: &str) -> String {
    format!("<task{turn}>\n{reminder}\n</task{turn}>")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_profile() -> (Option<MasterTaskType>, Option<SubTaskType>, Option<Difficulty>) {
        (
            Some(MasterTaskType::Coding),
            Some(SubTaskType::RustDev),
            Some(Difficulty::Complex),
        )
    }

    #[test]
    fn task_desc_should_report_unset_profile() {
        assert_eq!(task_desc(None, None, None), "尚未设定");
    }

    #[test]
    fn task_desc_should_join_labels() {
        let (m, s, d) = full_profile();
        assert_eq!(task_desc(m, s, d), "编程类-Rust开发 · 复杂");
    }

    #[test]
    fn task_hint_should_be_empty_without_master() {
        assert!(task_hint(None).is_empty());
        assert!(task_hint(Some(MasterTaskType::Coding)).contains("编程类任务"));
    }

    #[test]
    fn clean_prompt_content_should_strip_leading_status_and_transition_lines() {
        let raw = "当前为编程类Rust开发（复杂难度）。\n\n明确Rust版本与edition → 评估unsafe必要性\n\n领域要点：所有权与借用检查\n\n严格遵守《编程与架构准则》";
        let cleaned = clean_prompt_content(raw);
        assert!(!cleaned.starts_with("当前为"));
        assert!(cleaned.starts_with("明确Rust版本"));
        assert!(cleaned.contains("领域要点"));
    }

    #[test]
    fn clean_prompt_content_should_drop_transition_lines() {
        let raw = "错误分析根因后修正 → 完成目标后 asymptotic-think_transition 进入 VERIFY。\n所有权转移明确 · 生命周期标注完整";
        let cleaned = clean_prompt_content(raw);
        assert!(!cleaned.contains("asymptotic-think_transition"));
        assert!(cleaned.contains("所有权转移明确"));
    }

    #[test]
    fn prompt_content_should_prompt_for_profile_when_missing() {
        let content = prompt_content(None, None, None, State::Start);
        assert!(content.contains("设定任务信息"));
    }

    #[test]
    fn build_template_start_should_request_task_info() {
        let text = build_template(State::Start, 3, None, None, None, &[]);
        assert!(text.starts_with("<task3>"));
        assert!(text.ends_with("</task3>"));
        assert!(text.contains("第task3轮任务启动"));
        assert!(text.contains("设定任务画像"));
    }

    #[test]
    fn build_template_end_should_be_minimal() {
        let text = build_template(State::End, 1, None, None, None, &[]);
        assert!(text.contains("保持空闲等待新指令"));
        assert!(!text.contains("<actions"));
    }

    #[test]
    fn build_template_design_should_include_actions_and_flow_hint() {
        let (m, s, d) = full_profile();
        let visited = [State::DeepUnderstand];
        let text = build_template(State::Design, 2, m, s, d, &visited);
        assert!(text.contains("编程类-Rust开发 · 复杂"));
        assert!(text.contains("<actions spec=\"markdown\">"));
        assert!(text.contains("设计完成前不执行任何代码修改"));
        assert!(text.contains("可用流转："));
        assert!(text.contains("工具流转状态"));
    }

    #[test]
    fn build_template_verify_should_include_output_table() {
        let (m, s, d) = full_profile();
        let visited = [State::DeepUnderstand, State::Design, State::Execute];
        let text = build_template(State::Verify, 5, m, s, d, &visited);
        assert!(text.contains("<output spec=\"markdown\">"));
        assert!(text.contains("| 需求理解正确完整 | ✅/❌ | ... |"));
    }

    #[test]
    fn build_template_should_apply_path_adapter_for_direct_execute() {
        let (m, s, d) = full_profile();
        let text = build_template(State::Execute, 1, m, s, d, &[]);
        assert!(text.contains("本任务直达执行模式"));
    }

    #[test]
    fn state_guard_should_render_state_and_turn_counters() {
        let guard = state_guard(State::Execute, 40, 7000);
        assert!(guard.starts_with("<stateGuard>"));
        assert!(guard.ends_with("</stateGuard>"));
        assert!(guard.contains("【执行】状态（第40/7000轮）"));
    }

    #[test]
    fn continuation_notice_should_mention_current_state_label() {
        let notice = continuation_notice(State::Verify);
        assert!(notice.contains("【自检验证】状态"));
    }

    #[test]
    fn wrap_task_should_isolate_reminder_with_task_tags() {
        assert_eq!(wrap_task(7, "hello"), "<task7>\nhello\n</task7>");
    }

    #[test]
    fn framework_rules_should_be_inlined() {
        assert!(FRAMEWORK_RULES.contains("渐近式思考框架"));
        assert!(FRAMEWORK_RULES.contains("START → DEEP_UNDERSTAND"));
    }
}
