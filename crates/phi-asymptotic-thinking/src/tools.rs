// tools.rs — 三个 LLM 可调用工具：set-task-info / transition / status。

use phi_ext::phi;
use serde_json::Value;

use crate::runtime::Shared;
use crate::state_machine::{self, StateError};
use crate::store::now_ms;
use crate::templates;
use crate::types::{self, Difficulty, MasterTaskType, State, SubTaskType};

/// 统一的工具结果构造：只填 `content` 与 `detail`。
fn result(content: impl Into<String>, detail: impl Into<String>) -> phi::ToolResult {
    phi::ToolResult {
        content: content.into(),
        detail: detail.into(),
        ..phi::ToolResult::default()
    }
}

/// 从 JSON 参数中取出必填字符串字段。
fn str_field<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("参数 `{key}` 缺失、为空或不是字符串。"))
}

/// 把状态机错误转成给模型看的中文提示（含可选项清单，便于自纠）。
fn describe_state_error(err: &StateError) -> String {
    match err {
        StateError::SameState(_) => {
            "目标状态与当前状态相同，无需流转。若本阶段职责已完成，请选择其它目标；可用目标见状态守卫提示。"
                .to_string()
        }
        StateError::ProfileMissing => format!(
            "尚未设定任务画像。请先调用 `{}` 设定 difficulty / master_task_type / sub_task_type。{} {}",
            types::TOOL_TASK_INFO,
            types::describe_difficulties(),
            types::describe_masters()
        ),
        StateError::IllegalTarget { .. } => format!(
            "非法流转：{err}。请从允许集合中选择目标，或先补齐上一阶段职责。"
        ),
        StateError::TaskInfoNotAllowed => {
            "当前状态不允许重设任务画像。仅当处于 START 阶段，或本状态轮次已超过上限时才能重设。"
                .to_string()
        }
    }
}

/// 生成「设定画像 / 流转」成功后返回给模型的引导文本。
pub(crate) fn guide_after(state: &types::SessionState, at: State) -> String {
    format!(
        "{}\n\n{}",
        templates::build_template(
            at,
            state.task_turn_count,
            state.master_task_type,
            state.sub_task_type,
            state.difficulty,
            &state.visited,
        ),
        state_machine::format_next_state_hint(Some(at), state.difficulty)
    )
}

/// `set-task-info` 的参数 schema。
fn task_info_schema() -> phi::Schema {
    phi::Schema::object()
        .property(
            "difficulty",
            phi::Schema::string().description("任务难度，决定各状态的轮次上限。"),
        )
        .property(
            "master_task_type",
            phi::Schema::string().description("大任务类型。"),
        )
        .property(
            "sub_task_type",
            phi::Schema::string().description("小任务类型，必须属于所选大类型。"),
        )
        .required(["difficulty", "master_task_type", "sub_task_type"])
}

/// `transition` 的参数 schema。
fn transition_schema() -> phi::Schema {
    phi::Schema::object()
        .property("to", phi::Schema::string().description("目标状态名。"))
        .required(["to"])
}

/// 构建「设定任务画像」工具。
pub fn task_info_tool(rt: Shared) -> phi::Tool {
    let desc = format!(
        "设定任务画像（难度 / 大类型 / 小类型），必须在 START 阶段调用。{} {} {}",
        types::describe_difficulties(),
        types::describe_masters(),
        types::describe_subs()
    );
    phi::Tool::new(
        types::TOOL_TASK_INFO,
        desc,
        task_info_schema(),
        move |args: &[u8]| {
            let parsed: Value = serde_json::from_slice(args)
                .map_err(|e| format!("参数不是合法 JSON：{e}"))?;
            let difficulty = Difficulty::from_name(str_field(&parsed, "difficulty")?)
                .ok_or_else(|| format!("未知难度。{}", types::describe_difficulties()))?;
            let master = MasterTaskType::from_name(str_field(&parsed, "master_task_type")?)
                .ok_or_else(|| format!("未知大任务类型。{}", types::describe_masters()))?;
            let sub = SubTaskType::from_name(str_field(&parsed, "sub_task_type")?)
                .ok_or_else(|| format!("未知小任务类型。{}", types::describe_subs()))?;
            if sub.master() != master {
                return Err(format!(
                    "小任务类型 `{}` 属于大类型 `{}`，与传入的 `{}` 不匹配。{}",
                    sub.name(),
                    sub.master().name(),
                    master.name(),
                    types::describe_subs()
                ));
            }

            let guard = rt.borrow_mut();
            let mut state = guard.store.load();
            state_machine::set_task_info(&mut state, difficulty, master, sub, now_ms())
                .map_err(|e| describe_state_error(&e))?;
            guard
                .store
                .save(&state)
                .map_err(|e| format!("状态持久化失败：{e}"))?;
            drop(guard);

            Ok(result(
                guide_after(&state, State::Start),
                format!(
                    "画像：{} / {} / {}",
                    difficulty.label(),
                    master.label(),
                    sub.label()
                ),
            ))
        },
    )
}

/// 构建「状态流转」工具。
///
/// 流转成功时会把 `Runtime::transition_called` 置位，供 `on_turn_stopping` 做违规检测。
pub fn transition_tool(rt: Shared) -> phi::Tool {
    phi::Tool::new(
        types::TOOL_TRANSITION,
        "把渐近式思考状态机从当前状态流转到目标状态。目标必须落在当前状态的允许集合内。",
        transition_schema(),
        move |args: &[u8]| {
            let parsed: Value = serde_json::from_slice(args)
                .map_err(|e| format!("参数不是合法 JSON：{e}"))?;
            let to = State::from_name(str_field(&parsed, "to")?)
                .ok_or_else(|| {
                    format!(
                        "未知状态 `{}`。可用值：{}。",
                        str_field(&parsed, "to").unwrap_or(""),
                        State::ALL
                            .iter()
                            .map(|s| s.name())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })?;

            let mut guard = rt.borrow_mut();
            let mut state = guard.store.load();
            let outcome = state_machine::transition(&mut state, to, now_ms())
                .map_err(|e| describe_state_error(&e))?;
            guard
                .store
                .save(&state)
                .map_err(|e| format!("状态持久化失败：{e}"))?;
            guard.on_transition();
            drop(guard);

            let detail = format!("{} → {}", outcome.from.label(), outcome.to.label());
            Ok(result(guide_after(&state, outcome.to), detail))
        },
    )
}

/// 生成状态机完整快照（工具与 `/asymptotic-status` 命令共用）。
pub(crate) fn status_report(
    state: &types::SessionState,
    session_id: &str,
    enabled: bool,
) -> String {
    let current = state.state.unwrap_or(State::Start);
    let max_turns = state_machine::max_state_turns(current, state.difficulty);
    let interval = state_machine::reminder_interval(current, state.difficulty);
    let allowed = state_machine::allowed_targets(Some(current), state.difficulty);

    let allowed_text = if allowed.is_empty() {
        "无（END 由 before_agent_start 自动重置为 START）".to_string()
    } else {
        allowed
            .iter()
            .map(|s| format!("`{}`({})", s.name(), s.label()))
            .collect::<Vec<_>>()
            .join("、")
    };
    let visited_text = if state.visited.is_empty() {
        "（空）".to_string()
    } else {
        state
            .visited
            .iter()
            .map(|s| s.label())
            .collect::<Vec<_>>()
            .join(" → ")
    };

    // phi 没有 provider 请求钩子，无法真正改写 temperature / top_p，
    // 因此只把三层叠加结果作为「建议参数」展示，供人工核对。
    let (base_temp, base_top_p) = state
        .master_task_type
        .map_or((0.5, 0.9), MasterTaskType::inference_base);
    let (sub_temp, sub_top_p) = state
        .sub_task_type
        .map_or((0.0, 0.0), SubTaskType::inference_tuning);
    let temp_shift = state.difficulty.map_or(0.0, Difficulty::temperature_shift);
    let suggested_temp = (base_temp + sub_temp + temp_shift).clamp(0.0, 1.0);
    let suggested_top_p = (base_top_p + sub_top_p).clamp(0.0, 1.0);

    format!(
        "## 渐近式思考状态机快照\n\n\
         | 项 | 值 |\n|---|---|\n\
         | 启用 | {} |\n\
         | 当前状态 | **{}**（`{}`，序号 {}） |\n\
         | 任务轮次 | task {} |\n\
         | 状态内轮次 | {}/{}（提醒间隔 {}） |\n\
         | 难度 | {} |\n\
         | 大任务类型 | {} |\n\
         | 小任务类型 | {} |\n\
         | 建议推理参数 | temperature {:.2} / top_p {:.2}（phi 无请求钩子，仅供参考） |\n\
         | 会话 ID | `{}` |\n\n\
         **可流转目标**：{}\n\n\
         **已访问路径**：{}\n\n\
         **五态流程**：{}\n\n\
         {}",
        if enabled { "是" } else { "否" },
        current.label(),
        current.name(),
        current.order(),
        state.task_turn_count,
        state.state_turn_count,
        max_turns,
        interval,
        state.difficulty.map_or("未设定", Difficulty::label),
        state.master_task_type.map_or("未设定", MasterTaskType::label),
        state.sub_task_type.map_or("未设定", SubTaskType::label),
        suggested_temp,
        suggested_top_p,
        if session_id.is_empty() { "未知" } else { session_id },
        allowed_text,
        visited_text,
        types::flow_diagram(),
        state_machine::format_next_state_hint(Some(current), state.difficulty),
    )
}

/// 构建「查询状态」工具（只读，声明为 readable 以便宿主并发批处理）。
pub fn status_tool(rt: Shared) -> phi::Tool {
    phi::Tool::new(
        types::TOOL_STATUS,
        "查询渐近式思考状态机的当前状态、任务画像、轮次计数、可流转目标与流程路径。",
        phi::Schema::object(),
        move |_args: &[u8]| {
            let guard = rt.borrow();
            let state = guard.store.load();
            let enabled = guard.store.is_enabled();
            Ok(result(
                status_report(&state, &guard.session_id, enabled),
                "状态机快照",
            ))
        },
    )
    .readable()
}
