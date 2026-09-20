// types.rs — 渐近式思考状态机类型定义（双层任务类型 v4）
//
// 由 pi 版 asymptotic-thinking 扩展的 src/types.ts 移植。
// 与 pi 版的差异：
// - 去掉 `INFERENCE_*` 的运行时应用（phi 无 before_provider_request 钩子），
//   仅保留为「建议参数」供 /asymptotic-status 展示。
// - `SessionState` 增加 serde 派生以便 JSON 持久化。

use serde::{Deserialize, Serialize};

/// 状态流转工具名。
pub const TOOL_TRANSITION: &str = "asymptotic-think_transition";
/// 任务画像设定工具名。
pub const TOOL_TASK_INFO: &str = "asymptotic-think_set-task-info";
/// 状态查询工具名。
pub const TOOL_STATUS: &str = "asymptotic-think_status";

/// 六态状态机（START 起点 + 四业务态 + END 终态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum State {
    /// 新任务起点：评估任务复杂度，设定任务画像。
    Start,
    /// 深度理解需求：功能边界、约束、验收标准。
    DeepUnderstand,
    /// 方案设计：涉及文件、前置步骤、实施路径、验收标准。
    Design,
    /// 执行方案：严格按步骤、每步自检、不可行回退。
    Execute,
    /// 自检验证：逐项核对，全部通过流转 END。
    Verify,
    /// 任务终态：保持空闲，等待新指令。
    End,
}

/// 六档任务难度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Difficulty {
    Trivial,
    Simple,
    Moderate,
    Complex,
    Hard,
    Extreme,
}

/// 大任务类型（6 类）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MasterTaskType {
    Coding,
    Retrieval,
    Analytics,
    Devops,
    Entertainment,
    General,
}

/// 小任务类型（28 类）。
///
/// 在 pi 版 27 类的基础上补充 `ZigDev`：Zig 是本项目的一等目标语言，
/// 语言相关的判定（提示词、输出压缩、命令识别）都需要覆盖。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SubTaskType {
    JavaDev,
    RustDev,
    PythonDev,
    JsDev,
    GoDev,
    ZigDev,
    CrudDev,
    BugFix,
    CodeRefactor,
    Testing,
    Architect,
    CodeReview,
    PerfOptimize,
    PaperRetrieval,
    DailyRetrieval,
    DocRetrieval,
    CodeRetrieval,
    DataAnalysis,
    CodeAnalysis,
    LogAnalysis,
    RequirementAnalysis,
    Deploy,
    Monitor,
    Cicd,
    Config,
    FunChat,
    CreativeWriting,
    General,
}

impl State {
    /// 全部状态，按状态机顺序排列。
    pub const ALL: [State; 6] = [
        State::Start,
        State::DeepUnderstand,
        State::Design,
        State::Execute,
        State::Verify,
        State::End,
    ];

    /// 状态机顺序索引，用于区分「前移」与「回退」。
    pub const fn order(self) -> u8 {
        match self {
            State::Start => 0,
            State::DeepUnderstand => 1,
            State::Design => 2,
            State::Execute => 3,
            State::Verify => 4,
            State::End => 5,
        }
    }

    /// 中文标签。
    pub const fn label(self) -> &'static str {
        match self {
            State::Start => "启动",
            State::DeepUnderstand => "深度理解",
            State::Design => "方案设计",
            State::Execute => "执行",
            State::Verify => "自检验证",
            State::End => "结束",
        }
    }

    /// 线上协议名（大写蛇形）。
    pub const fn name(self) -> &'static str {
        match self {
            State::Start => "START",
            State::DeepUnderstand => "DEEP_UNDERSTAND",
            State::Design => "DESIGN",
            State::Execute => "EXECUTE",
            State::Verify => "VERIFY",
            State::End => "END",
        }
    }

    /// 由线上协议名解析，未知值返回 `None`。
    pub fn from_name(name: &str) -> Option<Self> {
        let upper = name.trim().to_ascii_uppercase();
        State::ALL.into_iter().find(|s| s.name() == upper)
    }
}

impl Difficulty {
    /// 全部难度，由易到难。
    pub const ALL: [Difficulty; 6] = [
        Difficulty::Trivial,
        Difficulty::Simple,
        Difficulty::Moderate,
        Difficulty::Complex,
        Difficulty::Hard,
        Difficulty::Extreme,
    ];

    /// 中文标签。
    pub const fn label(self) -> &'static str {
        match self {
            Difficulty::Trivial => "微不足道",
            Difficulty::Simple => "简单",
            Difficulty::Moderate => "中等",
            Difficulty::Complex => "复杂",
            Difficulty::Hard => "困难",
            Difficulty::Extreme => "极难",
        }
    }

    /// 线上协议名。
    pub const fn name(self) -> &'static str {
        match self {
            Difficulty::Trivial => "TRIVIAL",
            Difficulty::Simple => "SIMPLE",
            Difficulty::Moderate => "MODERATE",
            Difficulty::Complex => "COMPLEX",
            Difficulty::Hard => "HARD",
            Difficulty::Extreme => "EXTREME",
        }
    }

    /// 由线上协议名解析（兼容 `TRIVIAL(微不足道)` 形式）。
    pub fn from_name(name: &str) -> Option<Self> {
        let upper = normalize_token(name);
        Difficulty::ALL.into_iter().find(|d| d.name() == upper)
    }
}

impl MasterTaskType {
    /// 全部大类型。
    pub const ALL: [MasterTaskType; 6] = [
        MasterTaskType::Coding,
        MasterTaskType::Retrieval,
        MasterTaskType::Analytics,
        MasterTaskType::Devops,
        MasterTaskType::Entertainment,
        MasterTaskType::General,
    ];

    /// 中文标签。
    pub const fn label(self) -> &'static str {
        match self {
            MasterTaskType::Coding => "编程类",
            MasterTaskType::Retrieval => "检索类",
            MasterTaskType::Analytics => "分析类",
            MasterTaskType::Devops => "运维类",
            MasterTaskType::Entertainment => "娱乐类",
            MasterTaskType::General => "通用类",
        }
    }

    /// 线上协议名。
    pub const fn name(self) -> &'static str {
        match self {
            MasterTaskType::Coding => "CODING",
            MasterTaskType::Retrieval => "RETRIEVAL",
            MasterTaskType::Analytics => "ANALYTICS",
            MasterTaskType::Devops => "DEVOPS",
            MasterTaskType::Entertainment => "ENTERTAINMENT",
            MasterTaskType::General => "GENERAL",
        }
    }

    /// 大类领域要点提示。
    pub const fn hint(self) -> &'static str {
        match self {
            MasterTaskType::Coding => "编程类任务——注意代码结构、测试覆盖和错误处理",
            MasterTaskType::Retrieval => "检索类任务——多渠道并行搜索、信息去重和来源标注",
            MasterTaskType::Analytics => "分析类任务——数据来源可靠、分析逻辑严谨、结论有据",
            MasterTaskType::Devops => "运维类任务——环境配置验证、操作影响评估、回滚方案",
            MasterTaskType::Entertainment => "娱乐类任务——风格一致、内容有趣、安全合规",
            MasterTaskType::General => "通用任务——根据上下文判定具体侧重点",
        }
    }

    /// 该大类允许的子类型。
    pub const fn sub_types(self) -> &'static [SubTaskType] {
        match self {
            MasterTaskType::Coding => &[
                SubTaskType::JavaDev,
                SubTaskType::RustDev,
                SubTaskType::PythonDev,
                SubTaskType::JsDev,
                SubTaskType::GoDev,
                SubTaskType::ZigDev,
                SubTaskType::CrudDev,
                SubTaskType::BugFix,
                SubTaskType::CodeRefactor,
                SubTaskType::Testing,
                SubTaskType::Architect,
                SubTaskType::CodeReview,
                SubTaskType::PerfOptimize,
            ],
            MasterTaskType::Retrieval => &[
                SubTaskType::PaperRetrieval,
                SubTaskType::DailyRetrieval,
                SubTaskType::DocRetrieval,
                SubTaskType::CodeRetrieval,
            ],
            MasterTaskType::Analytics => &[
                SubTaskType::DataAnalysis,
                SubTaskType::CodeAnalysis,
                SubTaskType::LogAnalysis,
                SubTaskType::RequirementAnalysis,
            ],
            MasterTaskType::Devops => &[
                SubTaskType::Deploy,
                SubTaskType::Monitor,
                SubTaskType::Cicd,
                SubTaskType::Config,
            ],
            MasterTaskType::Entertainment => {
                &[SubTaskType::FunChat, SubTaskType::CreativeWriting]
            }
            MasterTaskType::General => &[SubTaskType::General],
        }
    }

    /// 由线上协议名解析。
    pub fn from_name(name: &str) -> Option<Self> {
        let upper = normalize_token(name);
        MasterTaskType::ALL.into_iter().find(|m| m.name() == upper)
    }

    /// 基础推理参数建议（temperature, top_p）。
    pub const fn inference_base(self) -> (f32, f32) {
        match self {
            MasterTaskType::Coding => (0.2, 0.85),
            MasterTaskType::Retrieval => (0.1, 0.7),
            MasterTaskType::Analytics => (0.3, 0.9),
            MasterTaskType::Devops => (0.1, 0.7),
            MasterTaskType::Entertainment => (0.9, 0.95),
            MasterTaskType::General => (0.5, 0.9),
        }
    }
}

impl SubTaskType {
    /// 全部子类型（按大类型分组，顺序与 pi 版一致）。
    pub const ALL: [SubTaskType; 28] = [
        SubTaskType::JavaDev,
        SubTaskType::RustDev,
        SubTaskType::PythonDev,
        SubTaskType::JsDev,
        SubTaskType::GoDev,
        SubTaskType::ZigDev,
        SubTaskType::CrudDev,
        SubTaskType::BugFix,
        SubTaskType::CodeRefactor,
        SubTaskType::Testing,
        SubTaskType::Architect,
        SubTaskType::CodeReview,
        SubTaskType::PerfOptimize,
        SubTaskType::PaperRetrieval,
        SubTaskType::DailyRetrieval,
        SubTaskType::DocRetrieval,
        SubTaskType::CodeRetrieval,
        SubTaskType::DataAnalysis,
        SubTaskType::CodeAnalysis,
        SubTaskType::LogAnalysis,
        SubTaskType::RequirementAnalysis,
        SubTaskType::Deploy,
        SubTaskType::Monitor,
        SubTaskType::Cicd,
        SubTaskType::Config,
        SubTaskType::FunChat,
        SubTaskType::CreativeWriting,
        SubTaskType::General,
    ];

    /// 中文标签。
    pub const fn label(self) -> &'static str {
        match self {
            SubTaskType::JavaDev => "Java开发",
            SubTaskType::RustDev => "Rust开发",
            SubTaskType::PythonDev => "Python开发",
            SubTaskType::JsDev => "JavaScript开发",
            SubTaskType::GoDev => "Go开发",
            SubTaskType::ZigDev => "Zig开发",
            SubTaskType::CrudDev => "增删改查",
            SubTaskType::BugFix => "缺陷修复",
            SubTaskType::CodeRefactor => "代码重构",
            SubTaskType::Testing => "程序测试",
            SubTaskType::Architect => "架构设计",
            SubTaskType::CodeReview => "代码审查",
            SubTaskType::PerfOptimize => "性能优化",
            SubTaskType::PaperRetrieval => "论文检索",
            SubTaskType::DailyRetrieval => "日常检索",
            SubTaskType::DocRetrieval => "文档检索",
            SubTaskType::CodeRetrieval => "代码检索",
            SubTaskType::DataAnalysis => "数据分析",
            SubTaskType::CodeAnalysis => "代码分析",
            SubTaskType::LogAnalysis => "日志分析",
            SubTaskType::RequirementAnalysis => "需求分析",
            SubTaskType::Deploy => "部署上线",
            SubTaskType::Monitor => "监控告警",
            SubTaskType::Cicd => "CI/CD",
            SubTaskType::Config => "环境配置",
            SubTaskType::FunChat => "休闲聊天",
            SubTaskType::CreativeWriting => "创意写作",
            SubTaskType::General => "通用",
        }
    }

    /// 线上协议名。
    pub const fn name(self) -> &'static str {
        match self {
            SubTaskType::JavaDev => "JAVA_DEV",
            SubTaskType::RustDev => "RUST_DEV",
            SubTaskType::PythonDev => "PYTHON_DEV",
            SubTaskType::JsDev => "JS_DEV",
            SubTaskType::GoDev => "GO_DEV",
            SubTaskType::ZigDev => "ZIG_DEV",
            SubTaskType::CrudDev => "CRUD_DEV",
            SubTaskType::BugFix => "BUG_FIX",
            SubTaskType::CodeRefactor => "CODE_REFACTOR",
            SubTaskType::Testing => "TESTING",
            SubTaskType::Architect => "ARCHITECT",
            SubTaskType::CodeReview => "CODE_REVIEW",
            SubTaskType::PerfOptimize => "PERF_OPTIMIZE",
            SubTaskType::PaperRetrieval => "PAPER_RETRIEVAL",
            SubTaskType::DailyRetrieval => "DAILY_RETRIEVAL",
            SubTaskType::DocRetrieval => "DOC_RETRIEVAL",
            SubTaskType::CodeRetrieval => "CODE_RETRIEVAL",
            SubTaskType::DataAnalysis => "DATA_ANALYSIS",
            SubTaskType::CodeAnalysis => "CODE_ANALYSIS",
            SubTaskType::LogAnalysis => "LOG_ANALYSIS",
            SubTaskType::RequirementAnalysis => "REQUIREMENT_ANALYSIS",
            SubTaskType::Deploy => "DEPLOY",
            SubTaskType::Monitor => "MONITOR",
            SubTaskType::Cicd => "CICD",
            SubTaskType::Config => "CONFIG",
            SubTaskType::FunChat => "FUN_CHAT",
            SubTaskType::CreativeWriting => "CREATIVE_WRITING",
            SubTaskType::General => "GENERAL",
        }
    }

    /// 由线上协议名解析。
    pub fn from_name(name: &str) -> Option<Self> {
        let upper = normalize_token(name);
        SubTaskType::ALL.into_iter().find(|s| s.name() == upper)
    }

    /// 所属大类型。
    pub fn master(self) -> MasterTaskType {
        MasterTaskType::ALL
            .into_iter()
            .find(|m| m.sub_types().contains(&self))
            .unwrap_or(MasterTaskType::General)
    }

    /// 小类型推理微调（temperature 增量, top_p 增量）。
    pub const fn inference_tuning(self) -> (f32, f32) {
        match self {
            SubTaskType::Architect
            | SubTaskType::CodeReview
            | SubTaskType::BugFix
            | SubTaskType::PerfOptimize => (-0.05, 0.0),
            SubTaskType::CodeRefactor => (-0.03, 0.0),
            SubTaskType::CreativeWriting => (0.05, 0.03),
            SubTaskType::FunChat => (0.0, 0.02),
            _ => (0.0, 0.0),
        }
    }
}

impl Difficulty {
    /// 难度带来的温度偏移。
    pub const fn temperature_shift(self) -> f32 {
        match self {
            Difficulty::Trivial => 0.05,
            Difficulty::Simple => 0.02,
            Difficulty::Moderate => 0.0,
            Difficulty::Complex => -0.02,
            Difficulty::Hard => -0.05,
            Difficulty::Extreme => -0.08,
        }
    }
}

/// 每条 session 的状态记录（对应 pi 版 `SessionState`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionState {
    /// 当前状态；`None` = 新建会话未初始化。
    pub state: Option<State>,
    /// 任务难度，START 时由 set-task-info 设定，转入 END 时清空。
    pub difficulty: Option<Difficulty>,
    /// 大任务类型，与 difficulty 同生命周期。
    pub master_task_type: Option<MasterTaskType>,
    /// 小任务类型，与 difficulty 同生命周期。
    pub sub_task_type: Option<SubTaskType>,
    /// 任务轮次计数：每次从 START 转移时 +1，即已启动的任务数。
    pub task_turn_count: u32,
    /// 状态内轮次计数：当前状态内已消耗的 LLM 调用次数，转移时归零。
    pub state_turn_count: u32,
    /// 最近一次状态转移的时间戳（毫秒，Unix epoch）。
    pub last_transition_time: u64,
    /// 本次任务经过的状态路径，转入 END 时清空。
    pub visited: Vec<State>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            state: Some(State::Start),
            difficulty: None,
            master_task_type: None,
            sub_task_type: None,
            task_turn_count: 0,
            state_turn_count: 0,
            last_transition_time: 0,
            visited: Vec::new(),
        }
    }
}

impl SessionState {
    /// 任务画像是否已设定齐全。
    pub fn has_profile(&self) -> bool {
        self.difficulty.is_some()
            && self.master_task_type.is_some()
            && self.sub_task_type.is_some()
    }

    /// 清空任务画像（转入 END 时调用）。
    pub fn clear_profile(&mut self) {
        self.difficulty = None;
        self.master_task_type = None;
        self.sub_task_type = None;
        self.visited.clear();
    }
}

/// 六态流程图（水平箭头）。
pub fn flow_diagram() -> String {
    State::ALL
        .iter()
        .map(|s| s.label())
        .collect::<Vec<_>>()
        .join(" → ")
}

/// 把形如 `TRIVIAL(微不足道)` 或 ` trivial ` 的输入归一化为 `TRIVIAL`。
fn normalize_token(input: &str) -> String {
    let trimmed = input.trim();
    let head = trimmed.split(['(', '（']).next().unwrap_or(trimmed);
    head.trim().to_ascii_uppercase()
}

/// 生成 `KEY(中文) | KEY(中文)` 形式的参数说明。
pub fn describe_options(options: &[(&str, &str)]) -> String {
    options
        .iter()
        .map(|(name, label)| format!("{name}({label})"))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// 难度参数说明（用于工具 schema）。
pub fn describe_difficulties() -> String {
    let options: Vec<(&str, &str)> = Difficulty::ALL
        .iter()
        .map(|d| (d.name(), d.label()))
        .collect();
    describe_options(&options)
}

/// 大任务类型参数说明（用于工具 schema）。
pub fn describe_masters() -> String {
    let options: Vec<(&str, &str)> = MasterTaskType::ALL
        .iter()
        .map(|m| (m.name(), m.label()))
        .collect();
    describe_options(&options)
}

/// 小任务类型参数说明（用于工具 schema）。
pub fn describe_subs() -> String {
    let options: Vec<(&str, &str)> = SubTaskType::ALL
        .iter()
        .map(|s| (s.name(), s.label()))
        .collect();
    describe_options(&options)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_from_name_should_accept_screaming_snake_case() {
        assert_eq!(State::from_name("DEEP_UNDERSTAND"), Some(State::DeepUnderstand));
        assert_eq!(State::from_name(" deep_understand "), Some(State::DeepUnderstand));
        assert_eq!(State::from_name("NOPE"), None);
    }

    #[test]
    fn difficulty_from_name_should_strip_parenthesised_label() {
        assert_eq!(Difficulty::from_name("TRIVIAL(微不足道)"), Some(Difficulty::Trivial));
        assert_eq!(Difficulty::from_name("extreme"), Some(Difficulty::Extreme));
    }

    #[test]
    fn master_from_name_should_parse_all_variants() {
        for master in MasterTaskType::ALL {
            assert_eq!(MasterTaskType::from_name(master.name()), Some(master));
        }
    }

    #[test]
    fn sub_from_name_should_parse_all_variants() {
        for sub in SubTaskType::ALL {
            assert_eq!(SubTaskType::from_name(sub.name()), Some(sub));
        }
    }

    #[test]
    fn sub_master_should_be_consistent_with_master_sub_types() {
        for master in MasterTaskType::ALL {
            for sub in master.sub_types() {
                assert_eq!(sub.master(), master);
            }
        }
    }

    #[test]
    fn sub_types_should_cover_all_28_variants() {
        let total: usize = MasterTaskType::ALL
            .iter()
            .map(|m| m.sub_types().len())
            .sum();
        assert_eq!(total, SubTaskType::ALL.len());
        assert_eq!(total, 28);
    }

    #[test]
    fn state_order_should_match_machine_sequence() {
        let orders: Vec<u8> = State::ALL.iter().map(|s| s.order()).collect();
        assert_eq!(orders, vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn session_state_should_report_profile_completeness() {
        let mut state = SessionState::default();
        assert!(!state.has_profile());
        state.difficulty = Some(Difficulty::Complex);
        state.master_task_type = Some(MasterTaskType::Coding);
        state.sub_task_type = Some(SubTaskType::RustDev);
        assert!(state.has_profile());
        state.clear_profile();
        assert!(!state.has_profile());
        assert!(state.visited.is_empty());
    }

    #[test]
    fn session_state_should_round_trip_through_json() {
        let state = SessionState {
            state: Some(State::Execute),
            difficulty: Some(Difficulty::Hard),
            master_task_type: Some(MasterTaskType::Coding),
            sub_task_type: Some(SubTaskType::PerfOptimize),
            task_turn_count: 2,
            state_turn_count: 41,
            last_transition_time: 1_700_000_000_000,
            visited: vec![State::Start, State::DeepUnderstand, State::Design],
        };
        let encoded = serde_json::to_string(&state).expect("序列化应成功");
        let decoded: SessionState = serde_json::from_str(&encoded).expect("反序列化应成功");
        assert_eq!(decoded, state);
    }

    #[test]
    fn flow_diagram_should_join_labels_with_arrows() {
        assert_eq!(flow_diagram(), "启动 → 深度理解 → 方案设计 → 执行 → 自检验证 → 结束");
    }
}
