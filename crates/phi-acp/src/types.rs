//! ACP 压缩内核数据模型 —— 对应 acp-kernel `src/types.ts`。
//!
//! 所有类型都实现 `Serialize` / `Deserialize`，因为压缩状态需要跨会话持久化到
//! `<phi_home>/extensions/phi-acp/state/`。序列化字段名与 TS 版保持一致，
//! 便于与既有状态文件互读、也便于排障时人工比对。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 消息角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// 用户消息。
    #[default]
    User,
    /// 助手消息。
    Assistant,
    /// 系统消息。
    System,
    /// 工具消息。
    Tool,
}

/// 消息内容类型。对应 TS 的 `text` / `tool-call` / `tool-result` / `reasoning`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ContentType {
    /// 纯文本。
    #[default]
    Text,
    /// 工具调用。
    ToolCall,
    /// 工具结果。
    ToolResult,
    /// 推理 / 思考内容。
    Reasoning,
}

/// 内核视角的一条消息。
///
/// 宿主负责把自身的消息结构投影成本类型：文本、工具名、工具调用 id 与
/// 推理 token 计数。`thinking_tokens` 只参与计量，不参与渲染或压缩。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CoreMessage {
    /// 稳定 id（宿主提供，跨 turn 不变）。
    pub id: String,
    /// 角色。
    #[serde(default)]
    pub role: Role,
    /// 内容类型。
    #[serde(default, rename = "contentType")]
    pub content_type: ContentType,
    /// 可见文本（工具调用时为参数序列化文本）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// 工具名（工具调用 / 结果）。
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "toolName")]
    pub tool_name: Option<String>,
    /// 工具调用 id（工具调用 / 结果）。
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "toolCallId"
    )]
    pub tool_call_id: Option<String>,
    /// 宿主投影的推理 token 计数，仅用于计量。
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "thinkingTokens"
    )]
    pub thinking_tokens: Option<u32>,
}

impl CoreMessage {
    /// 便捷构造一条文本消息。
    pub fn text(id: impl Into<String>, role: Role, text: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            role,
            content_type: ContentType::Text,
            text: Some(text.into()),
            ..Default::default()
        }
    }

    /// 该消息的可见文本（缺失视为空串）。
    pub fn text_str(&self) -> &str {
        self.text.as_deref().unwrap_or("")
    }
}

/// 压缩层级：1=原文摘要，2=蒸馏，3=超浓缩。
pub type CompressionTier = u8;

/// 块代际。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BlockGeneration {
    /// 年轻块。
    #[default]
    Young,
    /// 老块（存活足够多次）。
    Old,
}

/// 一个压缩块。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompressionBlock {
    /// 块 id（`bN`）。
    #[serde(rename = "blockId")]
    pub block_id: String,
    /// 所属压缩批次 id（`rN`）。
    #[serde(rename = "runId")]
    pub run_id: String,
    /// 层级。
    #[serde(default = "default_tier")]
    pub tier: CompressionTier,
    /// 主题（可选）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    /// 模型写出的摘要。
    pub summary: String,
    /// 直接压缩的原始消息 id。
    #[serde(rename = "directMessageIds")]
    pub direct_message_ids: Vec<String>,
    /// 有效覆盖的原始消息 id（含被下级块覆盖的）。
    #[serde(rename = "effectiveMessageIds")]
    pub effective_message_ids: Vec<String>,
    /// 直接消费的下级块 id。
    #[serde(rename = "directBlockIds")]
    pub direct_block_ids: Vec<String>,
    /// 被压缩原文的 token 数。
    #[serde(rename = "compressedTokens")]
    pub compressed_tokens: u64,
    /// 创建时间（Unix 毫秒）。
    #[serde(rename = "createdAt")]
    pub created_at: u64,
    /// 存活次数（每 turn +1，达到阈值后转老）。
    #[serde(rename = "survivedCount")]
    pub survived_count: u32,
    /// 代际。
    #[serde(default)]
    pub generation: BlockGeneration,
    /// 是否激活。
    pub active: bool,
    /// 用户显式解压过：其失活状态是刻意保留的。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expanded: Option<bool>,
    /// 压缩耗时（毫秒）。
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "durationMs"
    )]
    pub duration_ms: Option<u64>,
    /// 触发压缩的 compress 调用 id。
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "compressCallId"
    )]
    pub compress_call_id: Option<String>,
    /// 起始 ref。
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "startRef")]
    pub start_ref: Option<String>,
    /// 结束 ref。
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "endRef")]
    pub end_ref: Option<String>,
}

fn default_tier() -> CompressionTier {
    1
}

/// 块的当前 ref 跨度（预解析，供展示）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockSpan {
    /// 块 id。
    pub block_id: String,
    /// 层级。
    pub tier: CompressionTier,
    /// 起始 ref。
    pub start_ref: String,
    /// 结束 ref。
    pub end_ref: String,
}

/// 原始 id ↔ ref 双向映射。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessageRefMap {
    /// 原始 id → ref。
    #[serde(rename = "byRaw")]
    pub by_raw: BTreeMap<String, String>,
    /// ref → 原始 id。
    #[serde(rename = "byRef")]
    pub by_ref: BTreeMap<String, String>,
}

/// 提醒（nudge）状态。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NudgeState {
    /// 上次逐消息提醒时的 token 数。
    #[serde(rename = "lastPerMessageNudgeTokens")]
    pub last_per_message_nudge_tokens: u64,
    /// 上次展示提醒时的 token 数。
    #[serde(rename = "lastNudgeShownTokens")]
    pub last_nudge_shown_tokens: u64,
    /// 基线 token 数。
    #[serde(rename = "baselineTokens")]
    pub baseline_tokens: u64,
    /// 锚点（保留字段）。
    #[serde(default)]
    pub anchors: BTreeMap<String, phi_ext_common::json::Value>,
    /// 各层级的节奏基线。
    #[serde(rename = "lastShownByTier")]
    pub last_shown_by_tier: BTreeMap<u8, u64>,
}

/// 压缩累计统计。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompressionStats {
    /// 累计压缩 token 数。
    #[serde(rename = "tokensCompressed")]
    pub tokens_compressed: u64,
    /// 压缩次数。
    #[serde(rename = "compressionCount")]
    pub compression_count: u64,
    /// absorb 回收的累计 token 数。
    #[serde(default, rename = "absorbedTokens")]
    pub absorbed_tokens: u64,
}

/// 一次即时工具结果吸收记录。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AbsorbRecord {
    /// 工具调用 id。
    #[serde(rename = "toolCallId")]
    pub tool_call_id: String,
    /// 调用消息 id。
    #[serde(rename = "callMessageId")]
    pub call_message_id: String,
    /// 结果消息 id。
    #[serde(rename = "resultMessageId")]
    pub result_message_id: String,
    /// absorb 调用 id。
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "absorbCallId"
    )]
    pub absorb_call_id: Option<String>,
    /// 摘要。
    pub summary: String,
    /// 回收的 token 数。
    #[serde(rename = "tokensReclaimed")]
    pub tokens_reclaimed: u64,
    /// 创建时间。
    #[serde(rename = "createdAt")]
    pub created_at: u64,
}

/// 一条持久化规则（acp_rule）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuleRecord {
    /// 规则 id（`ruleN`）。
    pub id: String,
    /// 规则文本。
    pub text: String,
}

/// 压缩状态 —— 内核唯一的持久化产物。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompressionState {
    /// 全部块。
    #[serde(default)]
    pub blocks: Vec<CompressionBlock>,
    /// ref 映射。
    #[serde(default, rename = "messageRefs")]
    pub message_refs: MessageRefMap,
    /// 首次渲染时的 token 快照（按 ref 键控，保持前缀缓存稳定）。
    #[serde(default, rename = "tokenSnapshot")]
    pub token_snapshot: BTreeMap<String, u64>,
    /// 提醒状态。
    #[serde(default)]
    pub nudge: NudgeState,
    /// 累计统计。
    #[serde(default)]
    pub stats: CompressionStats,
    /// 吸收记录。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub absorbed: Vec<AbsorbRecord>,
    /// 连续「贴顶」事件计数（终端逃逸信号）。
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "terminalStreak"
    )]
    pub terminal_streak: Option<u32>,
    /// 持久规则。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<RuleRecord>,
    /// 规则 id 单调计数器。
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "nextRuleId"
    )]
    pub next_rule_id: Option<u64>,
    /// 下一个块 id。
    #[serde(rename = "nextBlockId")]
    pub next_block_id: u64,
    /// 下一个批次 id。
    #[serde(rename = "nextRunId")]
    pub next_run_id: u64,
}

/// 层级配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierConfig {
    /// 是否启用分层。
    pub enabled: bool,
    /// 活跃 T1 块数量达到该值后蒸馏到 T2。
    #[serde(rename = "tier2Trigger")]
    pub tier2_trigger: usize,
    /// 活跃 T2 块数量达到该值后浓缩到 T3。
    #[serde(rename = "tier3Trigger")]
    pub tier3_trigger: usize,
}

impl Default for TierConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            tier2_trigger: 5,
            tier3_trigger: 10,
        }
    }
}

/// 提醒（nudge）配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NudgeConfig {
    /// 使用率上限阈值。
    #[serde(rename = "maxContextLimitPct")]
    pub max_context_limit_pct: f64,
    /// 使用率下限阈值。
    #[serde(rename = "minContextLimitPct")]
    pub min_context_limit_pct: f64,
    /// 频率。
    pub frequency: u64,
    /// 迭代阈值。
    #[serde(rename = "iterationThreshold")]
    pub iteration_threshold: u64,
    /// 强制等级。
    #[serde(rename = "force")]
    pub force: NudgeForce,
    /// 增长比例。
    #[serde(rename = "growthRatio")]
    pub growth_ratio: f64,
    /// 增长阈值下界。
    #[serde(rename = "growthFloor")]
    pub growth_floor: u64,
    /// 增长阈值上界。
    #[serde(rename = "growthCap")]
    pub growth_cap: u64,
    /// 抗抖动最小增长下界。
    #[serde(rename = "minGrowthFloor")]
    pub min_growth_floor: u64,
    /// 抗抖动最小增长比例。
    #[serde(rename = "minGrowthRatio")]
    pub min_growth_ratio: f64,
    /// 紧急阈值。
    #[serde(rename = "emergencyThresholdPct")]
    pub emergency_threshold_pct: f64,
    /// T2 触发增长倍数。
    #[serde(rename = "tier2GrowthMultiplier")]
    pub tier2_growth_multiplier: f64,
    /// 压力带最小收益。
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "minPressureBenefitTokens"
    )]
    pub min_pressure_benefit_tokens: Option<u64>,
}

/// 提醒强制等级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum NudgeForce {
    /// 温和。
    #[default]
    Soft,
    /// 强。
    Strong,
}

impl Default for NudgeConfig {
    fn default() -> Self {
        Self {
            max_context_limit_pct: 0.75,
            min_context_limit_pct: 0.45,
            frequency: 5,
            iteration_threshold: 15,
            force: NudgeForce::Soft,
            growth_ratio: 0.05,
            growth_floor: 50_000,
            growth_cap: 50_000,
            min_growth_floor: 20_000,
            min_growth_ratio: 0.45,
            emergency_threshold_pct: 0.95,
            tier2_growth_multiplier: 1.5,
            min_pressure_benefit_tokens: None,
        }
    }
}

/// 紧急截断配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TruncateConfig {
    /// 触发使用率。
    pub threshold: f64,
    /// 连续贴顶多少次后发终端逃逸信号。
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "terminalEscapeAfter"
    )]
    pub terminal_escape_after: Option<u32>,
}

impl Default for TruncateConfig {
    fn default() -> Self {
        Self {
            threshold: 0.95,
            terminal_escape_after: Some(3),
        }
    }
}

/// 压缩校验配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressValidationConfig {
    /// 允许压缩的最小原文总字符数。
    #[serde(rename = "minCompressRange")]
    pub min_compress_range: usize,
    /// 摘要最大字符数。
    #[serde(rename = "maxSummaryLength")]
    pub max_summary_length: usize,
    /// 摘要最小字符数。
    #[serde(rename = "minSummaryLength")]
    pub min_summary_length: usize,
    /// 摘要 token 数相对被压缩内容 token 数的上限比例。
    ///
    /// 「压缩」的摘要不能和被压掉的内容差不多大——否则它不叫压缩，只是
    /// 把原文又写了一遍。超过该比例直接拒绝，逼模型重新写一份更精炼的。
    /// 0 表示关闭该校验。
    #[serde(default = "default_max_summary_ratio", rename = "maxSummaryRatio")]
    pub max_summary_ratio: f64,
}

fn default_max_summary_ratio() -> f64 {
    0.5
}

impl Default for CompressValidationConfig {
    fn default() -> Self {
        Self {
            min_compress_range: 5000,
            max_summary_length: 20_000,
            min_summary_length: 50,
            max_summary_ratio: default_max_summary_ratio(),
        }
    }
}

/// 吸收（absorb）配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbsorbConfig {
    /// 是否启用。
    pub enabled: bool,
    /// 工具名。
    #[serde(rename = "toolName")]
    pub tool_name: String,
    /// 工具结果最小 token 数。
    #[serde(rename = "minToolTokens")]
    pub min_tool_tokens: u64,
    /// 上下文使用率门槛。
    #[serde(rename = "contextThresholdPct")]
    pub context_threshold_pct: f64,
    /// 排除的工具名模式（`*` 通配 / 子串）。
    #[serde(default, rename = "excludeTools")]
    pub exclude_tools: Vec<String>,
    /// 保留的头部字符数。
    #[serde(default = "default_absorb_prefix", rename = "keepPrefixChars")]
    pub keep_prefix_chars: usize,
    /// 保留的尾部字符数。
    #[serde(default = "default_absorb_suffix", rename = "keepSuffixChars")]
    pub keep_suffix_chars: usize,
    /// 使用率门槛之下仍强制吸收的 token 数（0 = 关闭）。
    ///
    /// 门槛存在的意义是「先长后收」的波动，而不是让**巨型**输出在低水位时
    /// 完整留在历史里：一条 ≥ 该值的输出无论当前使用率多少都值得压成 stub。
    #[serde(default = "default_absorb_always_above", rename = "alwaysAboveTokens")]
    pub always_above_tokens: u64,
}

fn default_absorb_always_above() -> u64 {
    2000
}

fn default_absorb_prefix() -> usize {
    2000
}

fn default_absorb_suffix() -> usize {
    800
}

impl Default for AbsorbConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            tool_name: "absorb".to_string(),
            min_tool_tokens: 1000,
            context_threshold_pct: 0.0,
            exclude_tools: Vec::new(),
            keep_prefix_chars: default_absorb_prefix(),
            keep_suffix_chars: default_absorb_suffix(),
            always_above_tokens: default_absorb_always_above(),
        }
    }
}

/// 内核配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// 层级。
    pub tiers: TierConfig,
    /// 提醒。
    pub nudge: NudgeConfig,
    /// young→old 晋升阈值。
    #[serde(rename = "promotionThreshold")]
    pub promotion_threshold: u32,
    /// 截断。
    pub truncate: TruncateConfig,
    /// 压缩校验。
    pub compress: CompressValidationConfig,
    /// 受保护工具名模式。
    #[serde(rename = "protectedTools")]
    pub protected_tools: Vec<String>,
    /// 仅最新实例受保护的工具名模式。
    #[serde(default, rename = "protectedLatestTools")]
    pub protected_latest_tools: Vec<String>,
    /// 保留最近 N 条消息。
    #[serde(rename = "preserveRecentMessages")]
    pub preserve_recent_messages: usize,
    /// 保留最近 N token。
    #[serde(rename = "preserveRecentTokens")]
    pub preserve_recent_tokens: u64,
    /// 模型上下文上限。
    #[serde(rename = "modelContextLimit")]
    pub model_context_limit: u64,
    /// 吸收配置。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub absorb: Option<AbsorbConfig>,
}

impl Config {
    /// 以 `model_context_limit` 为基础构造默认配置。
    pub fn default_for(model_context_limit: u64) -> Self {
        Self {
            tiers: TierConfig::default(),
            nudge: NudgeConfig::default(),
            promotion_threshold: 5,
            truncate: TruncateConfig::default(),
            compress: CompressValidationConfig::default(),
            protected_tools: Vec::new(),
            protected_latest_tools: Vec::new(),
            preserve_recent_messages: 5,
            preserve_recent_tokens: 5000,
            model_context_limit,
            absorb: Some(AbsorbConfig::default()),
        }
    }
}

/// 压缩模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompressMode {
    /// 按 ref 范围。
    Range,
    /// 按单条消息。
    Message,
}

/// 一次压缩范围规格。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompressRangeSpec {
    /// 起始 ref。
    #[serde(rename = "startRef")]
    pub start_ref: String,
    /// 结束 ref。
    #[serde(rename = "endRef")]
    pub end_ref: String,
    /// 摘要。
    pub summary: String,
    /// 主题。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    /// 触发压缩的调用 id。
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "compressCallId"
    )]
    pub compress_call_id: Option<String>,
    /// 单次覆盖的摘要最大长度。
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "summaryMaxChars"
    )]
    pub summary_max_chars: Option<usize>,
}

/// 可压缩范围。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompressibleRange {
    /// 起始 ref。
    #[serde(rename = "startRef")]
    pub start_ref: String,
    /// 结束 ref。
    #[serde(rename = "endRef")]
    pub end_ref: String,
    /// 消息条数。
    pub count: usize,
    /// token 数。
    pub tokens: u64,
    /// 字符数。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chars: Option<usize>,
    /// 工具消息占比。
    #[serde(rename = "toolPct")]
    pub tool_pct: u32,
    /// 文本占比。
    #[serde(rename = "textPct")]
    pub text_pct: u32,
    /// 是否危险。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dangerous: Option<bool>,
    /// 范围内用户消息数。
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "userMsgs")]
    pub user_msgs: Option<usize>,
    /// 起始数组下标。
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "startIndex"
    )]
    pub start_index: Option<usize>,
    /// 结束数组下标。
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "endIndex")]
    pub end_index: Option<usize>,
}

/// 受保护范围。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProtectedRange {
    /// 起始 ref。
    #[serde(rename = "startRef")]
    pub start_ref: String,
    /// 结束 ref。
    #[serde(rename = "endRef")]
    pub end_ref: String,
    /// 消息条数。
    pub count: usize,
    /// token 数。
    pub tokens: u64,
    /// 涉及工具名。
    pub tools: Vec<String>,
    /// 起始数组下标。
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "startIndex"
    )]
    pub start_index: Option<usize>,
    /// 结束数组下标。
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "endIndex")]
    pub end_index: Option<usize>,
}

/// 可压缩 + 受保护范围集合。
#[derive(Debug, Clone, Default)]
pub struct ContextRanges {
    /// 可压缩范围。
    pub compressible: Vec<CompressibleRange>,
    /// 受保护范围。
    pub protected: Vec<ProtectedRange>,
}

/// 推荐结果。
#[derive(Debug, Clone, Default)]
pub struct Recommendation {
    /// 范围集合。
    pub context_ranges: ContextRanges,
    /// 推荐压缩的范围。
    pub recommended_ranges: Vec<CompressibleRange>,
    /// 是否无可压缩内容。
    pub nothing_to_compress: bool,
}

/// 提醒决策。
#[derive(Debug, Clone, Default)]
pub struct NudgeDecision {
    /// 是否注入。
    pub should_inject: bool,
    /// 原因。
    pub reason: String,
    /// 可压缩范围。
    pub compressible_ranges: Vec<CompressibleRange>,
    /// 受保护范围。
    pub protected_ranges: Vec<ProtectedRange>,
    /// 活跃块跨度。
    pub active_block_spans: Vec<BlockSpan>,
    /// 层级目标块。
    pub tier_target_blocks: Vec<CompressionBlock>,
    /// 上下文使用率。
    pub context_usage: f64,
    /// 层级（None 表示普通提醒）。
    pub tier: Option<CompressionTier>,
    /// 数字诊断。
    pub breakdown: NudgeBreakdown,
}

/// 提醒的数字诊断字段。
#[derive(Debug, Clone, Default)]
pub struct NudgeBreakdown {
    /// 使用率。
    pub usage: f64,
    /// 增长量。
    pub growth: u64,
    /// 增长参考点。
    pub growth_reference: u64,
    /// 生效阈值。
    pub effective_threshold: u64,
    /// 增长阈值。
    pub nudge_growth_tokens: u64,
    /// 增长下界。
    pub growth_floor: u64,
    /// 是否有待处理提醒。
    pub has_pending_nudge: u64,
    /// 是否超限。
    pub over_limit: u64,
    /// 是否紧急覆盖。
    pub emergency_override: u64,
    /// T1 待压缩。
    pub pending_t1: u64,
    /// T2 待压缩。
    pub pending_t2: u64,
    /// T3 待压缩。
    pub pending_t3: u64,
    /// 各层最大待压缩量。
    pub max_pending: u64,
    /// 压力带最小收益。
    pub min_pressure_benefit: u64,
    /// 判定首次提醒用的层级标记：`Some(t)` 表示本次提醒是会话中该层级第一次。
    ///
    /// 完整 T2/T3 规则只在首次携带（提醒文本会永久留在会话历史里，重复携带
    /// 几百 token 是纯亏损）。
    pub first_by_tier: Option<CompressionTier>,
}

/// 应用压缩的结果。
#[derive(Debug, Clone, Default)]
pub struct ApplyResult {
    /// 新建块数。
    pub blocks_created: usize,
    /// 压缩 token 数。
    pub tokens_compressed: u64,
    /// 错误。
    pub errors: Vec<String>,
    /// 警告。
    pub warnings: Vec<String>,
}

/// 应用压缩的完整返回。
#[derive(Debug, Clone, Default)]
pub struct ApplyCompressionOutcome {
    /// 新状态。
    pub state: CompressionState,
    /// 结果。
    pub result: ApplyResult,
}

/// processTurn 的返回。
#[derive(Debug, Clone, Default)]
pub struct ProcessTurnOutcome {
    /// 处理后的消息。
    pub messages: Vec<CoreMessage>,
    /// 新状态。
    pub state: CompressionState,
    /// 提醒决策。
    pub nudge: Option<NudgeDecision>,
    /// 渲染后的上下文细分（供提醒文本展示）。
    pub context_breakdown: Option<crate::nudge::ContextBreakdown>,
    /// 终端逃逸信号。
    pub terminal_escape: Option<TerminalEscapeSignal>,
    /// 截断被跳过时的说明。
    pub truncation_skipped: Option<String>,
}

/// 终端逃逸信号。
#[derive(Debug, Clone, Default)]
pub struct TerminalEscapeSignal {
    /// 面向模型 / 宿主的说明。
    pub message: String,
    /// 使用率。
    pub usage: f64,
    /// token 数。
    pub token_count: u64,
    /// 模型上下文上限。
    pub model_context_limit: u64,
    /// 连续贴顶事件数。
    pub stuck_events: u32,
}

/// 状态报告。
#[derive(Debug, Clone, Default)]
pub struct StatusReport {
    /// 上下文使用率。
    pub context_usage: f64,
    /// token 数。
    pub token_count: u64,
    /// 模型上下文上限。
    pub model_context_limit: u64,
    /// 活跃块数。
    pub active_blocks: usize,
    /// 总块数。
    pub total_blocks: usize,
    /// 累计压缩 token 数。
    pub tokens_compressed: u64,
    /// 细分统计。
    pub breakdown: BTreeMap<String, u64>,
}
