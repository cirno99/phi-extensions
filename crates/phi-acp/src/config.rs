//! 扩展级配置 —— 对应 billion-context 的 `config.ts` 可落地子集。
//!
//! 配置持久化到 `<phi_home>/extensions/phi-acp/config.json`（原子写）。
//! 它同时负责构造内核 [`crate::types::Config`]。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use phi_ext_common::config as cfg;
use phi_ext_common::paths;

use crate::render::RenderStrategy;
use crate::types::Config;

/// 扩展名（也是数据目录名）。
pub const EXTENSION_NAME: &str = "phi-acp";

/// 当前配置架构版本。
///
/// 老配置（没有 `configVersion` 字段）里保存的 `absorbEnabled: false` 来自
/// absorb 尚未成为主要回收通道的时期——它会直接关掉 phi 上**唯一**能真正
/// 删 token 的机制，让上下文单调堆积。因此升级时按版本号一次性重写这部分
/// 默认值；此后用户再手动 `/acp config absorb false` 会被尊重。
pub const CURRENT_CONFIG_VERSION: u32 = 2;

/// 配置架构版本。
fn default_config_version() -> u32 {
    CURRENT_CONFIG_VERSION
}

/// 扩展级配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpConfig {
    /// 配置架构版本（用于一次性迁移旧默认值）。
    #[serde(default, rename = "configVersion")]
    pub config_version: u32,
    /// 是否启用。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 模型上下文上限（token）。
    #[serde(default = "default_context_limit", rename = "modelContextLimit")]
    pub model_context_limit: u64,
    /// 标签渲染策略。
    #[serde(default, rename = "renderTags")]
    pub render_tags: String,
    /// 保留最近消息数。
    #[serde(
        default = "default_preserve_messages",
        rename = "preserveRecentMessages"
    )]
    pub preserve_recent_messages: usize,
    /// 保留最近 token 数。
    #[serde(default = "default_preserve_tokens", rename = "preserveRecentTokens")]
    pub preserve_recent_tokens: u64,
    /// 受保护工具名模式。
    #[serde(default, rename = "protectedTools")]
    pub protected_tools: Vec<String>,
    /// 允许压缩的最小字符数。
    #[serde(default = "default_min_compress", rename = "minCompressRange")]
    pub min_compress_range: usize,
    /// 单会话最多连续提醒次数（防止死循环）。
    #[serde(default = "default_max_nudges", rename = "maxConsecutiveNudges")]
    pub max_consecutive_nudges: u32,
    /// 是否启用即时吸收（absorb）。
    ///
    /// phi 上这是**唯一**能真正减少上游 token 的通道：`tool_result` 拦截把巨型
    /// 工具输出换成 stub 后回写，模型看到的上下文才真的变小。仅写 state.json
    /// 的 compress 块对宿主历史无效（详见 `crate::absorb`）。
    #[serde(default = "default_true", rename = "absorbEnabled")]
    pub absorb_enabled: bool,
    /// 触发 absorb 的最小工具输出 token 数。
    #[serde(default = "default_absorb_min_tokens", rename = "absorbMinToolTokens")]
    pub absorb_min_tool_tokens: u64,
    /// absorb 保留的头部字符数。
    #[serde(
        default = "default_absorb_prefix_chars",
        rename = "absorbKeepPrefixChars"
    )]
    pub absorb_keep_prefix_chars: usize,
    /// absorb 保留的尾部字符数。
    #[serde(
        default = "default_absorb_suffix_chars",
        rename = "absorbKeepSuffixChars"
    )]
    pub absorb_keep_suffix_chars: usize,
    /// absorb 的上下文使用率门槛（0 = 不设门槛）。
    #[serde(
        default = "default_absorb_context_threshold_pct",
        rename = "absorbContextThresholdPct"
    )]
    pub absorb_context_threshold_pct: f64,
    /// 使用率门槛之下仍强制吸收的 token 数（0 = 关闭）。
    ///
    /// 门槛只负责「先长后收」的波动，不应让**巨型**输出在低水位时完整留在
    /// 历史里：一条 ≥ 该值的工具输出无论当前使用率多少都值得压成 stub。
    #[serde(
        default = "default_absorb_always_above_tokens",
        rename = "absorbAlwaysAboveTokens"
    )]
    pub absorb_always_above_tokens: u64,
    /// absorb 排除的工具名模式（`*` 通配 / 子串）。
    #[serde(default, rename = "absorbExcludeTools")]
    pub absorb_exclude_tools: Vec<String>,
    /// 是否读取宿主会话文件里的真实 token 数（替代本地估算）。
    #[serde(default = "default_true", rename = "useHostTokens")]
    pub use_host_tokens: bool,
    /// T1 提醒所需的最小可压缩 token 质量（内核 growthFloor/growthCap）。
    #[serde(default = "default_nudge_growth_tokens", rename = "nudgeGrowthTokens")]
    pub nudge_growth_tokens: u64,
    /// 两次提醒之间至少新增的 token（内核 minGrowthFloor）。
    #[serde(
        default = "default_nudge_min_growth_tokens",
        rename = "nudgeMinGrowthTokens"
    )]
    pub nudge_min_growth_tokens: u64,
    /// 触发提醒的最低上下文使用率（内核 minContextLimitPct）。
    #[serde(
        default = "default_nudge_min_context_pct",
        rename = "nudgeMinContextPct"
    )]
    pub nudge_min_context_pct: f64,
    /// 进入「超限压力带」的使用率（内核 maxContextLimitPct）。
    #[serde(
        default = "default_nudge_max_context_pct",
        rename = "nudgeMaxContextPct"
    )]
    pub nudge_max_context_pct: f64,
    /// 是否启用**自动**提醒（`turn_stopping` 注入那一种）。默认关闭。
    ///
    /// # 为什么默认关
    ///
    /// 自动提醒在 phi 宿主上是**严格负收益**，三件事同时发生：
    /// 1. 转向消息被当成 user 消息永久 append 进会话历史（`main.rs` 注释），
    ///    每次提醒 = 永久 +几十到几百 token，且自己永远无法被回收；
    /// 2. `turn_stopping` 返回 `continue` 会**跳过**同轮的宿主
    ///    `runCompact`（`internal/agent/engine.go`：`continue` 之后才轮到
    ///    compaction），把唯一能真正重置上下文的机制挡住；
    /// 3. 提醒想换来的 `compress` 块在 phi 上只写扩展自己的 `state.json`，
    ///    压不掉宿主历史里的任何内容（见 `crate::absorb` 文档）。
    ///
    /// 于是「多提醒」= 多花 token + 阻止重置 + 零回收。真正能删 token 的是
    /// `tool_result` 里的 absorb（见 `absorb*` 配置）：它静默工作，不需要模型
    /// 配合，也不会阻断宿主压缩。手动 `/acp compress` 仍然可用（由用户显式
    /// 触发，不在此开关控制内）。
    #[serde(default, rename = "autoNudgeEnabled")]
    pub auto_nudge_enabled: bool,
    /// 活跃 T1 块达到该数量后蒸馏到 T2（内核 tier2Trigger）。
    #[serde(default = "default_tier2_trigger", rename = "tier2Trigger")]
    pub tier2_trigger: usize,
    /// 活跃 T2 块达到该数量后浓缩到 T3（内核 tier3Trigger）。
    #[serde(default = "default_tier3_trigger", rename = "tier3Trigger")]
    pub tier3_trigger: usize,
}

fn default_true() -> bool {
    true
}
fn default_context_limit() -> u64 {
    200_000
}
fn default_preserve_messages() -> usize {
    5
}
fn default_preserve_tokens() -> u64 {
    5000
}
/**
 * absorb 默认门槛。
 *
 * 实测：工具输出占一个编码会话上下文的 70–80%（样本：261 条工具结果共
 * 163_921 token；130 条共 113_684 token）。`min_tool_tokens` 定在 500 而非
 * 1000，保留窗口缩到 1500 / 500 字符后，同一批数据的可回收量从 ~69K 升到 ~81K。
 * 保留的头/尾仍足以定位错误行与结论，中段由标记提示「按需重跑」。
 */
fn default_absorb_min_tokens() -> u64 {
    500
}
fn default_absorb_prefix_chars() -> usize {
    1500
}
fn default_absorb_suffix_chars() -> usize {
    500
}

/**
 * absorb 的使用率门槛（默认 0.30）。
 *
 * 这是让「上下文在 50K～～200K 之间波动」的关键旋钮：低于门槛时不动手，
 * 上下文自然长大；越过门槛后新工具输出被压成 stub，使用率回落，
 * 回落过低后重新暂停。取 `0` 则从第一轮就无差别吸收，上下文会一直偏小。
 */
fn default_absorb_context_threshold_pct() -> f64 {
    0.30
}
/**
 * 门槛之下仍强制吸收的 token 数（默认 2000）。
 *
 * 使用率门槛负责「先长后收」的波动，但它不能成为**巨型**输出的免死金牌：
 * 早期会话（水位远低于门槛）或高门槛配置下，一条几万 token 的构建/测试日志
 * 会完整留在历史里，直到水位涨到门槛才被处理。这里给一个「无论水位多低都吸」
 * 的上界，把这类纯噪声尽早压成 stub。取 0 则关闭该例外。
 */
fn default_absorb_always_above_tokens() -> u64 {
    2000
}
fn default_min_compress() -> usize {
    5000
}
fn default_max_nudges() -> u32 {
    5
}
fn default_nudge_growth_tokens() -> u64 {
    20_000
}
fn default_nudge_min_growth_tokens() -> u64 {
    10_000
}
fn default_nudge_min_context_pct() -> f64 {
    0.30
}
/**
 * 进入「超限压力带」的使用率（默认 0.90）。
 *
 * # 为什么比原来的 0.65 高
 *
 * 压力带内 `turn_stopping` 会**每 turn**注入一条提醒，而 phi 把转向消息当
 * user 消息永久留在历史里（每次提醒 = 永久 +几十到几百 token，见 `main.rs`）。
 * 而提醒想要换来的 `compress` 块在 phi 上只写进扩展自己的 `state.json`，
 * **压不掉宿主历史里的任何内容**（见 `crate::absorb` 模块文档）。
 *
 * 于是「在 65% 就持续提醒」是一条净亏损路径：期望水位 110K+ 时每 turn 都发，
 * 真实回收为零，只留下永久堆叠的提醒文本。把压力带抬到 0.90（约 162K），
 * 日常振荡就完全交给能真正删 token 的 absorb（其门槛/强度随使用率自适应），
 * 提醒退回为「真的快顶到上限」时的告警。
 */
fn default_nudge_max_context_pct() -> f64 {
    0.90
}
fn default_tier2_trigger() -> usize {
    3
}
fn default_tier3_trigger() -> usize {
    6
}

impl Default for AcpConfig {
    fn default() -> Self {
        Self {
            config_version: default_config_version(),
            enabled: true,
            model_context_limit: default_context_limit(),
            render_tags: "all".to_string(),
            preserve_recent_messages: default_preserve_messages(),
            preserve_recent_tokens: default_preserve_tokens(),
            protected_tools: Vec::new(),
            min_compress_range: default_min_compress(),
            max_consecutive_nudges: default_max_nudges(),
            absorb_enabled: true,
            absorb_min_tool_tokens: default_absorb_min_tokens(),
            absorb_keep_prefix_chars: default_absorb_prefix_chars(),
            absorb_keep_suffix_chars: default_absorb_suffix_chars(),
            absorb_context_threshold_pct: default_absorb_context_threshold_pct(),
            absorb_always_above_tokens: default_absorb_always_above_tokens(),
            absorb_exclude_tools: Vec::new(),
            use_host_tokens: true,
            nudge_growth_tokens: default_nudge_growth_tokens(),
            nudge_min_growth_tokens: default_nudge_min_growth_tokens(),
            nudge_min_context_pct: default_nudge_min_context_pct(),
            nudge_max_context_pct: default_nudge_max_context_pct(),
            auto_nudge_enabled: false,
            tier2_trigger: default_tier2_trigger(),
            tier3_trigger: default_tier3_trigger(),
        }
    }
}

impl AcpConfig {
    /// 渲染策略解析。
    pub fn render_strategy(&self) -> RenderStrategy {
        match self.render_tags.trim().to_ascii_lowercase().as_str() {
            "none" => RenderStrategy::None,
            "text-only" | "textonly" => RenderStrategy::TextOnly,
            _ => RenderStrategy::All,
        }
    }

    /// 构造内核配置。
    pub fn to_kernel_config(&self) -> Config {
        let mut config = Config::default_for(self.model_context_limit);
        config.preserve_recent_messages = self.preserve_recent_messages;
        config.preserve_recent_tokens = self.preserve_recent_tokens;
        config.protected_tools = self.protected_tools.clone();
        config.compress.min_compress_range = self.min_compress_range;
        config.tiers.tier2_trigger = self.tier2_trigger;
        config.tiers.tier3_trigger = self.tier3_trigger;
        config.nudge.growth_floor = self.nudge_growth_tokens;
        config.nudge.growth_cap = self.nudge_growth_tokens;
        config.nudge.min_growth_floor = self.nudge_min_growth_tokens;
        config.nudge.min_context_limit_pct = self.nudge_min_context_pct;
        config.nudge.max_context_limit_pct = self.nudge_max_context_pct;
        if let Some(absorb) = config.absorb.as_mut() {
            absorb.enabled = self.absorb_enabled;
            absorb.min_tool_tokens = self.absorb_min_tool_tokens;
            absorb.keep_prefix_chars = self.absorb_keep_prefix_chars;
            absorb.keep_suffix_chars = self.absorb_keep_suffix_chars;
            absorb.context_threshold_pct = self.absorb_context_threshold_pct;
            absorb.always_above_tokens = self.absorb_always_above_tokens;
            absorb.exclude_tools = self.absorb_exclude_tools.clone();
        }
        config
    }
}

/// 配置文件路径。
pub fn config_path() -> PathBuf {
    paths::extension_config_path(EXTENSION_NAME)
}

/// 压缩状态文件路径。
pub fn state_path() -> PathBuf {
    paths::extension_state_dir(EXTENSION_NAME).join("state.json")
}

/// 加载配置，并做一次性的架构迁移。
///
/// 迁移是**必要**的：`config.json` 是用户的持久化文件，没有 `configVersion`
/// 就说明它写入时 absorb 还没成为主要回收通道。那份配置里通常带着
/// `absorbEnabled: false`（旧默认），直接沿用会让新默认值完全失效——上下文
/// 继续单调堆积，正是本次要修掉的现象。迁移只改写与「保持上下文可选区间」
/// 直接相关且旧版本默认为「关」的字段，用户改过的其它值一律不动。
///
/// 迁移结果**不在这里落盘**：`load()` 是每会话都会跑的热路径（单测也会调），
/// 在里面写磁盘既无必要也危险（会把测试跑成对真实用户配置的写入）。迁移
/// 幂等，下次用户真的执行 `/acp config …` 时随 `save()` 一并持久化。
pub fn load() -> AcpConfig {
    let raw: AcpConfig = cfg::load_or_default(&config_path());
    migrate(raw).0
}

/// 把旧版本配置迁移到当前架构。返回（迁移后的配置，是否发生改动）。
///
/// 目前只有 1 → 2：打开 absorb、补上使用率门槛。这里的「旧值」判定不带歧义
/// ——absorb 在 v1 的默认值是关闭，而开启它是本次修复的核心。
fn migrate(mut config: AcpConfig) -> (AcpConfig, bool) {
    if config.config_version >= CURRENT_CONFIG_VERSION {
        return (config, false);
    }
    if config.config_version == 0 {
        config.absorb_enabled = true;
        // v1 的 absorb 无门槛（写多少吸多少），会把上下文永久钉在低水位。
        // 0 在这里代表「旧默认」而非用户的显式选择（迁移只跑一次），
        // 换成默认门槛后「先长后收」的波动才成立。
        if config.absorb_context_threshold_pct <= 0.0 {
            config.absorb_context_threshold_pct = default_absorb_context_threshold_pct();
        }
    }
    config.config_version = CURRENT_CONFIG_VERSION;
    (config, true)
}

/// 保存配置（原子写）。
pub fn save(config: &AcpConfig) -> Result<(), cfg::ConfigError> {
    cfg::save_atomic(&config_path(), config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_strategy_should_parse_variants() {
        let mut config = AcpConfig::default();
        assert_eq!(config.render_strategy(), RenderStrategy::All);
        config.render_tags = "text-only".into();
        assert_eq!(config.render_strategy(), RenderStrategy::TextOnly);
        config.render_tags = "NONE".into();
        assert_eq!(config.render_strategy(), RenderStrategy::None);
    }

    #[test]
    fn kernel_config_should_carry_overrides() {
        let config = AcpConfig {
            model_context_limit: 128_000,
            min_compress_range: 1000,
            ..Default::default()
        };
        let kernel = config.to_kernel_config();
        assert_eq!(kernel.model_context_limit, 128_000);
        assert_eq!(kernel.compress.min_compress_range, 1000);
    }

    /// 扩展级的 absorb 旋钮必须完整透传到内核 `AbsorbConfig`（含巨型输出例外）。
    #[test]
    fn kernel_config_should_carry_absorb_overrides() {
        let config = AcpConfig {
            absorb_always_above_tokens: 1234,
            absorb_context_threshold_pct: 0.42,
            ..Default::default()
        };
        let absorb = config.to_kernel_config().absorb.expect("内核应带 absorb 配置");
        assert_eq!(absorb.always_above_tokens, 1234);
        assert_eq!(absorb.context_threshold_pct, 0.42);
    }

    /// 压力带提高到 0.90：日常振荡交给 absorb，提醒只在真接近上限时发。
    #[test]
    fn absorb_defaults_should_regulate_into_a_band() {
        let config = AcpConfig::default();
        // 使用率门槛让「先长后收」成立。
        assert_eq!(config.absorb_context_threshold_pct, 0.30);
        assert!(config.absorb_enabled);
        // 门槛比压力带低，中间才有波动空间。
        assert!(config.absorb_context_threshold_pct < config.nudge_max_context_pct);
        let kernel = config.to_kernel_config();
        assert_eq!(kernel.nudge.max_context_limit_pct, 0.90);
        assert_eq!(kernel.nudge.min_context_limit_pct, 0.30);
        assert_eq!(kernel.tiers.tier2_trigger, 3);
        assert_eq!(kernel.tiers.tier3_trigger, 6);
    }

    /// 回归：老配置（无 configVersion）里的 `absorbEnabled: false`
    /// 会把唯一能真正删 token 的通道关掉，迁移必须把它打开。
    #[test]
    fn migration_should_enable_absorb_for_legacy_config() {
        let legacy = AcpConfig {
            config_version: 0,
            absorb_enabled: false,
            ..Default::default()
        };
        let (migrated, changed) = migrate(legacy);
        assert!(changed, "旧配置应被迁移");
        assert!(migrated.absorb_enabled, "旧配置必须打开 absorb");
        // 无门槛的旧 absorb 会把上下文钉在低水位，迁移要补上门槛。
        assert_eq!(migrated.absorb_context_threshold_pct, 0.30);
        assert_eq!(migrated.config_version, CURRENT_CONFIG_VERSION);
    }

    /// 迁移后用户手动关掉 absorb 仍应被尊重（幂等且不覆盖显式选择）。
    #[test]
    fn migration_should_be_idempotent() {
        let current = AcpConfig {
            config_version: CURRENT_CONFIG_VERSION,
            absorb_enabled: false,
            ..Default::default()
        };
        let (migrated, changed) = migrate(current);
        assert!(!changed, "已是当前版本不应再迁移");
        assert!(!migrated.absorb_enabled, "用户的显式选择必须保留");
    }

    /// 老配置里的其它字段不能被迁移覆盖。
    #[test]
    fn migration_should_preserve_user_overrides() {
        let legacy = AcpConfig {
            config_version: 0,
            absorb_enabled: false,
            model_context_limit: 123_456,
            absorb_min_tool_tokens: 77,
            ..Default::default()
        };
        let (migrated, _) = migrate(legacy);
        assert_eq!(migrated.model_context_limit, 123_456);
        assert_eq!(migrated.absorb_min_tool_tokens, 77);
    }
}
