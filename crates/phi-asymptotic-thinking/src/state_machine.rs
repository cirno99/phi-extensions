// state_machine.rs — 六态状态机 + 转移校验 + 轮次警告
//
// 由 pi 版 asymptotic-thinking 扩展的 src/state-machine.ts 移植。
// 与 pi 版的差异：
// - 去掉内存 Map + SQLite 双层结构，直接操作传入的 `SessionState`（由 store 负责持久化）。
// - 时间戳由调用方注入（`now_ms`），便于单元测试确定性断言。

use crate::types::{
    Difficulty, MasterTaskType, SessionState, State, SubTaskType,
};

/// 难度 × 状态 的显式轮次上限表（START/END 最低 1 轮）。
///
/// 调整说明（2026-08-07 v3）：全阶段均衡上调——每个阶段模型都可能调用工具。
/// 旧值参考（2026-08-05）：TRIVIAL 3 / SIMPLE 6 / MODERATE 10 / COMPLEX 15 / HARD 18 / EXTREME 22。
pub fn max_state_turns(state: State, difficulty: Option<Difficulty>) -> u32 {
    let Some(diff) = difficulty else {
        // 难度未设定（START 阶段）：START 最低 1 轮，其余状态防御性 0。
        return if state == State::Start { 1 } else { 0 };
    };
    match diff {
        Difficulty::Trivial => match state {
            State::Start => 1,
            State::DeepUnderstand => 200,
            State::Design => 100,
            State::Execute => 500,
            State::Verify => 100,
            State::End => 1,
        },
        Difficulty::Simple => match state {
            State::Start => 1,
            State::DeepUnderstand => 500,
            State::Design => 300,
            State::Execute => 1500,
            State::Verify => 300,
            State::End => 1,
        },
        Difficulty::Moderate => match state {
            State::Start => 1,
            State::DeepUnderstand => 1500,
            State::Design => 1000,
            State::Execute => 4000,
            State::Verify => 1000,
            State::End => 1,
        },
        Difficulty::Complex => match state {
            State::Start => 1,
            State::DeepUnderstand => 3000,
            State::Design => 2000,
            State::Execute => 7000,
            State::Verify => 2000,
            State::End => 1,
        },
        Difficulty::Hard => match state {
            State::Start => 1,
            State::DeepUnderstand => 5000,
            State::Design => 3500,
            State::Execute => 9000,
            State::Verify => 3000,
            State::End => 1,
        },
        Difficulty::Extreme => match state {
            State::Start => 1,
            State::DeepUnderstand => 7000,
            State::Design => 5000,
            State::Execute => 10000,
            State::Verify => 4000,
            State::End => 1,
        },
    }
}

/// turn_end 提醒间隔：`state_turn_count % interval == 0` 时才发送，避免稀释正常提示词。
pub fn reminder_interval(state: State, difficulty: Option<Difficulty>) -> u32 {
    let Some(diff) = difficulty else {
        return 1;
    };
    let value = match diff {
        Difficulty::Trivial => match state {
            State::DeepUnderstand => 5,
            State::Design => 8,
            State::Execute => 12,
            State::Verify => 8,
            _ => 0,
        },
        Difficulty::Simple => match state {
            State::DeepUnderstand => 6,
            State::Design => 10,
            State::Execute => 15,
            State::Verify => 10,
            _ => 0,
        },
        Difficulty::Moderate => match state {
            State::DeepUnderstand => 8,
            State::Design => 12,
            State::Execute => 18,
            State::Verify => 12,
            _ => 0,
        },
        Difficulty::Complex => match state {
            State::DeepUnderstand => 10,
            State::Design => 15,
            State::Execute => 20,
            State::Verify => 15,
            _ => 0,
        },
        Difficulty::Hard => match state {
            State::DeepUnderstand => 12,
            State::Design => 18,
            State::Execute => 25,
            State::Verify => 18,
            _ => 0,
        },
        Difficulty::Extreme => match state {
            State::DeepUnderstand => 15,
            State::Design => 20,
            State::Execute => 30,
            State::Verify => 20,
            _ => 0,
        },
    };
    if value == 0 {
        1
    } else {
        value
    }
}

/// 根据当前状态 + 难度，返回允许转移的目标状态集合。
///
/// START 时根据难度开放不同路径：TRIVIAL 可直达 EXECUTE，SIMPLE 可到 DESIGN/EXECUTE，
/// 其余必须先 DEEP_UNDERSTAND。END 无合法出口（只能靠 before_agent_start 代码转换回 START）。
pub fn allowed_targets(from: Option<State>, difficulty: Option<Difficulty>) -> Vec<State> {
    match from {
        None | Some(State::Start) => match difficulty {
            Some(Difficulty::Trivial) => vec![State::DeepUnderstand, State::Execute],
            Some(Difficulty::Simple) => {
                vec![State::DeepUnderstand, State::Design, State::Execute]
            }
            _ => vec![State::DeepUnderstand],
        },
        Some(State::DeepUnderstand) => vec![State::Design, State::Execute, State::Verify],
        Some(State::Design) => vec![State::Execute, State::DeepUnderstand, State::Verify],
        Some(State::Execute) => vec![State::Verify, State::Design, State::DeepUnderstand],
        Some(State::Verify) => vec![
            State::End,
            State::Execute,
            State::DeepUnderstand,
            State::Design,
        ],
        // 无合法出口，只能 before_agent_start 代码转换。
        Some(State::End) => Vec::new(),
    }
}

/// 按自然流程方向拆分出的目标集合。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TargetGroups {
    /// 自然流程中目标顺序大于当前状态的目标（含完成任务进入 END）。
    pub forward: Vec<State>,
    /// 自然流程中目标顺序小于当前状态的目标。
    pub backward: Vec<State>,
}

/// 将指定状态下允许的目标区分为「向前」和「回退」两组。
pub fn classify_targets(from: Option<State>, difficulty: Option<Difficulty>) -> TargetGroups {
    let current_order = from.unwrap_or(State::Start).order();
    let mut groups = TargetGroups::default();
    for target in allowed_targets(from, difficulty) {
        let is_forward = (target == State::End && from != Some(State::End))
            || target.order() > current_order;
        if is_forward {
            groups.forward.push(target);
        } else {
            groups.backward.push(target);
        }
    }
    groups
}

/// 格式化状态流转提示文本。
///
/// 格式：`本阶段完成可前移至[方案设计(DESIGN)、执行(EXECUTE)]状态；本阶段不足可回退至[...]状态。`
pub fn format_next_state_hint(from: Option<State>, difficulty: Option<Difficulty>) -> String {
    let groups = classify_targets(from, difficulty);
    let fmt = |states: &[State]| {
        states
            .iter()
            .map(|s| format!("{}({})", s.label(), s.name()))
            .collect::<Vec<_>>()
            .join("、")
    };

    let mut parts: Vec<String> = Vec::new();
    if !groups.forward.is_empty() {
        parts.push(format!("本阶段完成可前移至[{}]状态；", fmt(&groups.forward)));
    }
    if !groups.backward.is_empty() {
        parts.push(format!("本阶段不足可回退至[{}]状态。", fmt(&groups.backward)));
    }
    parts.concat()
}

/// 状态机操作错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StateError {
    /// 目标状态与当前状态相同。
    #[error("不能转移到自身（{0}→{0}）")]
    SameState(String),
    /// START 出发前未设定任务画像。
    #[error("START 流转前必须调用 asymptotic-think_set-task-info 工具设定任务画像（难度/大类型/小类型），当前画像未设定")]
    ProfileMissing,
    /// 目标状态不在允许集合内。
    #[error("{from} 状态下不能转移到 {to}。可转移: {allowed}")]
    IllegalTarget {
        /// 当前状态名。
        from: String,
        /// 目标状态名。
        to: String,
        /// 允许的目标列表。
        allowed: String,
    },
    /// 非 START 且未超限时调用 set-task-info。
    #[error("仅可在 START 阶段或状态超限时设定任务信息")]
    TaskInfoNotAllowed,
}

/// 状态转移成功后的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionOutcome {
    /// 转移前状态。
    pub from: State,
    /// 转移后状态。
    pub to: State,
}

/// 设定任务画像（对应 `asymptotic-think_set-task-info`）。
///
/// 允许条件：START 状态（正常设定）或当前状态超限（防御难度虚高，允许重估）。
/// 设定后 `state_turn_count` 归零（新难度新起点），`task_turn_count` 保持不变。
pub fn set_task_info(
    state: &mut SessionState,
    difficulty: Difficulty,
    master: MasterTaskType,
    sub: SubTaskType,
    now_ms: u64,
) -> Result<(), StateError> {
    let current = state.state;
    let over_limit = match current {
        Some(s) if s != State::End => {
            state.state_turn_count > max_state_turns(s, state.difficulty)
        }
        _ => false,
    };
    if current != Some(State::Start) && !over_limit {
        return Err(StateError::TaskInfoNotAllowed);
    }

    state.difficulty = Some(difficulty);
    state.master_task_type = Some(master);
    state.sub_task_type = Some(sub);
    state.last_transition_time = now_ms;
    state.state_turn_count = 0;
    Ok(())
}

/// 状态流转（对应 `asymptotic-think_transition`）。
///
/// - 校验转移合法性（[`allowed_targets`]）
/// - END 状态拒绝任何手动流转（只能由 before_agent_start 代码转换）
/// - 转入 END 时清空任务画像与 visited
/// - 从 START 出发时 `task_turn_count` +1
/// - `state_turn_count` 每次转移归零
/// - visited 记录每次流转的目标状态（全链路路径感知）
pub fn transition(
    state: &mut SessionState,
    to: State,
    now_ms: u64,
) -> Result<TransitionOutcome, StateError> {
    let from = state.state.unwrap_or(State::Start);
    if from == to {
        return Err(StateError::SameState(format!("{}→{}", from.name(), to.name())));
    }

    // START 出发：必须先设定任务画像（set-task-info），否则拒绝流转。
    if from == State::Start && !state.has_profile() {
        return Err(StateError::ProfileMissing);
    }

    let allowed = allowed_targets(Some(from), state.difficulty);
    if !allowed.contains(&to) {
        let allowed_text = if allowed.is_empty() {
            "无（END 需用户发消息自动转换）".to_string()
        } else {
            allowed
                .iter()
                .map(|s| s.name())
                .collect::<Vec<_>>()
                .join(", ")
        };
        return Err(StateError::IllegalTarget {
            from: from.name().to_string(),
            to: to.name().to_string(),
            allowed: allowed_text,
        });
    }

    let ending = to == State::End;
    if ending {
        state.clear_profile();
    }
    if from == State::Start {
        state.task_turn_count += 1;
    }
    state.state = Some(to);
    state.state_turn_count = 0;
    state.last_transition_time = now_ms;
    if !ending {
        state.visited.push(to);
    }
    Ok(TransitionOutcome { from, to })
}

/// 轮次警告级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarnLevel {
    /// 接近上限的软提醒。
    Soft,
    /// 已超过上限。
    Over,
    /// 严重超时，强制停止。
    HardStop,
}

/// 一轮结束后的轮次建议。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnAdvice {
    /// 警告级别。
    pub level: WarnLevel,
    /// 带 XML 标签包裹的完整提醒文本。
    pub text: String,
}

/// 难度档位（决定提醒措辞的严厉程度）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WarnTier {
    /// TRIVIAL / SIMPLE：温和，引导重估难度。
    Simple,
    /// MODERATE：标准。
    Standard,
    /// COMPLEX / HARD / EXTREME：严厉，强调流程纪律。
    Hard,
}

/// 由难度推导提醒档位。
fn warn_tier(difficulty: Option<Difficulty>) -> WarnTier {
    match difficulty {
        Some(Difficulty::Trivial) | Some(Difficulty::Simple) => WarnTier::Simple,
        Some(Difficulty::Complex) | Some(Difficulty::Hard) | Some(Difficulty::Extreme) => {
            WarnTier::Hard
        }
        _ => WarnTier::Standard,
    }
}

/// 文案表：级别 × 难度档位 → 建议文案。
fn detail_text(level: WarnLevel, tier: WarnTier) -> &'static str {
    match level {
        WarnLevel::Soft => match tier {
            WarnTier::Simple => "若任务比预期简单，可调用 asymptotic-think_set-task-info 工具 重估难度；否则请尽快完成并流转状态。",
            WarnTier::Standard => "可调用 asymptotic-think_set-task-info 工具 重新评估难度，或尽快完成当前阶段并流转状态，以顺利推进任务。",
            WarnTier::Hard => "请遵循流程纪律，专注推进，完成后立即流转状态；若难度评估有误，可调用 asymptotic-think_set-task-info 工具 重新评估难度。",
        },
        WarnLevel::Over => match tier {
            WarnTier::Simple => "任务可能被误评高难度——调用 asymptotic-think_set-task-info 工具重估为更低难度，或立即调用 asymptotic-think_transition 工具流转状态。",
            WarnTier::Standard => "请调用 asymptotic-think_transition 工具流转状态，或调用 asymptotic-think_set-task-info 工具 重新评估难度，及时推进任务。",
            WarnTier::Hard => "请遵循流程纪律，立即调用 asymptotic-think_transition 工具流转状态；若难度评估有误，可调用 asymptotic-think_set-task-info 工具 重新评估难度。",
        },
        WarnLevel::HardStop => match tier {
            WarnTier::Simple => "请立即调用 asymptotic-think_transition 工具流转状态；若任务实际简单，调用 asymptotic-think_set-task-info 工具 重估难度，以高效完成。",
            WarnTier::Standard => "请停止当前操作，立即调用 asymptotic-think_transition 工具流转状态，或调用 asymptotic-think_set-task-info 工具 重新评估难度，或等待用户新指令。",
            WarnTier::Hard => "请遵循流程纪律，停止当前操作并立即调用 asymptotic-think_transition 工具流转状态；若难度评估有误，可调用 asymptotic-think_set-task-info 工具 重新评估难度。",
        },
    }
}

/// 一轮结束：`state_turn_count` +1，返回轮次警告（如有）。
///
/// 三级阈值：软提醒（接近 maxTurns）/ 超限警告（> maxTurns）/ hardStop 强制停止（> maxTurns + 1/3）。
/// END 不计数；`None` 视为 START 参与计数。
pub fn bump_and_warn(state: &mut SessionState) -> Option<TurnAdvice> {
    if state.state == Some(State::End) {
        return None;
    }

    let current = state.state.unwrap_or(State::Start);
    let next_turn = state.state_turn_count.saturating_add(1);
    let max_turns = max_state_turns(current, state.difficulty);
    state.state_turn_count = next_turn;

    let label = current.label();
    let tier = warn_tier(state.difficulty);
    let excess_threshold = max_turns + std::cmp::max(1, max_turns.div_ceil(3));

    let (level, detail) = if next_turn > excess_threshold {
        (WarnLevel::HardStop, detail_text(WarnLevel::HardStop, tier))
    } else if next_turn > max_turns {
        (WarnLevel::Over, detail_text(WarnLevel::Over, tier))
    } else if max_turns > 2 && next_turn + 1 >= max_turns {
        (WarnLevel::Soft, detail_text(WarnLevel::Soft, tier))
    } else {
        return None;
    };

    let text = match level {
        WarnLevel::Soft => format!(
            "\n<turnWarning>你已处于【{label}】状态 {next_turn} 轮（上限 {max_turns}），接近上限。{detail}</turnWarning>"
        ),
        WarnLevel::Over => format!(
            "\n<turnWarning>你已处于【{label}】状态 {next_turn} 轮（上限 {max_turns}），已超过上限。{detail}</turnWarning>"
        ),
        WarnLevel::HardStop => format!(
            "\n<hardStop>⛔ 强制停止：你已在【{label}】状态停留 {next_turn} 轮（上限 {max_turns}），严重超时。{detail}</hardStop>"
        ),
    };

    Some(TurnAdvice { level, text })
}

/// 判断本轮的提醒是否应当发送（按 `reminder_interval` 节流）。
pub fn should_emit_reminder(state: &SessionState) -> bool {
    let current = state.state.unwrap_or(State::Start);
    if current == State::End {
        return false;
    }
    let interval = reminder_interval(current, state.difficulty);
    interval > 0 && state.state_turn_count % interval == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> SessionState {
        SessionState {
            difficulty: Some(Difficulty::Moderate),
            master_task_type: Some(MasterTaskType::Coding),
            sub_task_type: Some(SubTaskType::RustDev),
            ..SessionState::default()
        }
    }

    #[test]
    fn max_state_turns_should_return_start_only_when_difficulty_missing() {
        assert_eq!(max_state_turns(State::Start, None), 1);
        assert_eq!(max_state_turns(State::Design, None), 0);
    }

    #[test]
    fn max_state_turns_should_match_complex_row() {
        let diff = Some(Difficulty::Complex);
        assert_eq!(max_state_turns(State::DeepUnderstand, diff), 3000);
        assert_eq!(max_state_turns(State::Design, diff), 2000);
        assert_eq!(max_state_turns(State::Execute, diff), 7000);
        assert_eq!(max_state_turns(State::Verify, diff), 2000);
    }

    #[test]
    fn reminder_interval_should_fall_back_to_one_for_terminal_states() {
        assert_eq!(reminder_interval(State::Start, Some(Difficulty::Extreme)), 1);
        assert_eq!(reminder_interval(State::End, Some(Difficulty::Extreme)), 1);
        assert_eq!(reminder_interval(State::Execute, None), 1);
        assert_eq!(reminder_interval(State::Execute, Some(Difficulty::Extreme)), 30);
    }

    #[test]
    fn allowed_targets_should_open_short_paths_for_low_difficulty() {
        assert_eq!(
            allowed_targets(Some(State::Start), Some(Difficulty::Trivial)),
            vec![State::DeepUnderstand, State::Execute]
        );
        assert_eq!(
            allowed_targets(Some(State::Start), Some(Difficulty::Simple)),
            vec![State::DeepUnderstand, State::Design, State::Execute]
        );
        assert_eq!(
            allowed_targets(Some(State::Start), Some(Difficulty::Moderate)),
            vec![State::DeepUnderstand]
        );
    }

    #[test]
    fn allowed_targets_should_be_empty_for_end() {
        assert!(allowed_targets(Some(State::End), Some(Difficulty::Hard)).is_empty());
    }

    #[test]
    fn classify_targets_should_split_forward_and_backward() {
        let groups = classify_targets(Some(State::Design), Some(Difficulty::Hard));
        assert_eq!(groups.forward, vec![State::Execute, State::Verify]);
        assert_eq!(groups.backward, vec![State::DeepUnderstand]);
    }

    #[test]
    fn classify_targets_should_treat_end_as_forward() {
        let groups = classify_targets(Some(State::Verify), Some(Difficulty::Hard));
        assert!(groups.forward.contains(&State::End));
        assert!(groups.backward.contains(&State::Execute));
    }

    #[test]
    fn format_next_state_hint_should_render_labels_and_names() {
        let hint = format_next_state_hint(Some(State::Execute), Some(Difficulty::Hard));
        assert_eq!(
            hint,
            "本阶段完成可前移至[自检验证(VERIFY)]状态；本阶段不足可回退至[方案设计(DESIGN)、深度理解(DEEP_UNDERSTAND)]状态。"
        );
    }

    #[test]
    fn set_task_info_should_apply_on_start_and_reset_state_turns() {
        let mut state = SessionState {
            state_turn_count: 7,
            task_turn_count: 3,
            ..SessionState::default()
        };
        let result = set_task_info(
            &mut state,
            Difficulty::Complex,
            MasterTaskType::Coding,
            SubTaskType::PerfOptimize,
            1000,
        );
        assert!(result.is_ok());
        assert_eq!(state.difficulty, Some(Difficulty::Complex));
        assert_eq!(state.state_turn_count, 0);
        assert_eq!(state.task_turn_count, 3);
        assert_eq!(state.last_transition_time, 1000);
    }

    #[test]
    fn set_task_info_should_reject_when_state_is_not_start_and_within_limit() {
        let mut state = SessionState {
            state: Some(State::Design),
            ..profile()
        };
        let result = set_task_info(
            &mut state,
            Difficulty::Simple,
            MasterTaskType::Coding,
            SubTaskType::Testing,
            0,
        );
        assert_eq!(result, Err(StateError::TaskInfoNotAllowed));
        assert_eq!(state.difficulty, Some(Difficulty::Moderate));
    }

    #[test]
    fn set_task_info_should_allow_reevaluation_after_state_exceeds_limit() {
        let mut state = SessionState {
            state: Some(State::Design),
            difficulty: Some(Difficulty::Trivial),
            master_task_type: Some(MasterTaskType::Coding),
            sub_task_type: Some(SubTaskType::RustDev),
            state_turn_count: 101,
            ..SessionState::default()
        };
        let result = set_task_info(
            &mut state,
            Difficulty::Simple,
            MasterTaskType::Coding,
            SubTaskType::Testing,
            0,
        );
        assert!(result.is_ok());
        assert_eq!(state.difficulty, Some(Difficulty::Simple));
    }

    #[test]
    fn transition_should_reject_self_target() {
        let mut state = profile();
        let result = transition(&mut state, State::Start, 0);
        assert!(matches!(result, Err(StateError::SameState(_))));
    }

    #[test]
    fn transition_should_reject_when_profile_missing_on_start() {
        let mut state = SessionState::default();
        let result = transition(&mut state, State::DeepUnderstand, 0);
        assert_eq!(result, Err(StateError::ProfileMissing));
    }

    #[test]
    fn transition_should_reject_illegal_target() {
        let mut state = profile();
        let result = transition(&mut state, State::Verify, 0);
        assert!(matches!(result, Err(StateError::IllegalTarget { .. })));
        assert_eq!(state.state, Some(State::Start));
    }

    #[test]
    fn transition_from_start_should_increment_task_turns_and_record_visited() {
        let mut state = profile();
        let outcome = transition(&mut state, State::DeepUnderstand, 500).expect("合法流转");
        assert_eq!(outcome.from, State::Start);
        assert_eq!(outcome.to, State::DeepUnderstand);
        assert_eq!(state.task_turn_count, 1);
        assert_eq!(state.state_turn_count, 0);
        assert_eq!(state.visited, vec![State::DeepUnderstand]);
        assert_eq!(state.last_transition_time, 500);
    }

    #[test]
    fn transition_to_end_should_clear_profile_and_visited() {
        let mut state = SessionState {
            state: Some(State::Verify),
            visited: vec![State::DeepUnderstand, State::Design, State::Execute],
            task_turn_count: 2,
            ..profile()
        };
        transition(&mut state, State::End, 900).expect("合法流转");
        assert_eq!(state.state, Some(State::End));
        assert!(!state.has_profile());
        assert!(state.visited.is_empty());
        assert_eq!(state.task_turn_count, 2);
    }

    #[test]
    fn bump_and_warn_should_not_count_in_end_state() {
        let mut state = SessionState {
            state: Some(State::End),
            state_turn_count: 4,
            ..SessionState::default()
        };
        assert!(bump_and_warn(&mut state).is_none());
        assert_eq!(state.state_turn_count, 4);
    }

    #[test]
    fn bump_and_warn_should_return_none_far_from_limit() {
        let mut state = profile();
        state.state = Some(State::Execute);
        assert!(bump_and_warn(&mut state).is_none());
        assert_eq!(state.state_turn_count, 1);
    }

    #[test]
    fn bump_and_warn_should_emit_soft_warning_near_limit() {
        // TRIVIAL 的 DESIGN 上限为 100，第 99 轮触发软提醒。
        let mut state = SessionState {
            state: Some(State::Design),
            difficulty: Some(Difficulty::Trivial),
            master_task_type: Some(MasterTaskType::Coding),
            sub_task_type: Some(SubTaskType::RustDev),
            state_turn_count: 98,
            ..SessionState::default()
        };
        let advice = bump_and_warn(&mut state).expect("应产生软提醒");
        assert_eq!(advice.level, WarnLevel::Soft);
        assert!(advice.text.contains("<turnWarning>"));
        assert!(advice.text.contains("上限 100"));
    }

    #[test]
    fn bump_and_warn_should_emit_over_warning_past_limit() {
        let mut state = SessionState {
            state: Some(State::Design),
            difficulty: Some(Difficulty::Trivial),
            master_task_type: Some(MasterTaskType::Coding),
            sub_task_type: Some(SubTaskType::RustDev),
            state_turn_count: 100,
            ..SessionState::default()
        };
        let advice = bump_and_warn(&mut state).expect("应产生超限警告");
        assert_eq!(advice.level, WarnLevel::Over);
        assert!(advice.text.contains("已超过上限"));
    }

    #[test]
    fn bump_and_warn_should_emit_hard_stop_after_buffer_exceeded() {
        // 100 + max(1, ceil(100/3)) = 134，第 135 轮触发强制停止。
        let mut state = SessionState {
            state: Some(State::Design),
            difficulty: Some(Difficulty::Trivial),
            master_task_type: Some(MasterTaskType::Coding),
            sub_task_type: Some(SubTaskType::RustDev),
            state_turn_count: 134,
            ..SessionState::default()
        };
        let advice = bump_and_warn(&mut state).expect("应产生强制停止");
        assert_eq!(advice.level, WarnLevel::HardStop);
        assert!(advice.text.contains("<hardStop>"));
        assert!(advice.text.contains("强制停止"));
    }

    #[test]
    fn should_emit_reminder_should_throttle_by_interval() {
        let mut state = SessionState {
            state: Some(State::Execute),
            difficulty: Some(Difficulty::Moderate),
            state_turn_count: 18,
            ..SessionState::default()
        };
        assert!(should_emit_reminder(&state));
        state.state_turn_count = 17;
        assert!(!should_emit_reminder(&state));
        state.state = Some(State::End);
        state.state_turn_count = 18;
        assert!(!should_emit_reminder(&state));
    }
}
