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

/// 扩展级配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpConfig {
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
    #[serde(default, rename = "absorbEnabled")]
    pub absorb_enabled: bool,
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
fn default_nudge_max_context_pct() -> f64 {
    0.65
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
            enabled: true,
            model_context_limit: default_context_limit(),
            render_tags: "all".to_string(),
            preserve_recent_messages: default_preserve_messages(),
            preserve_recent_tokens: default_preserve_tokens(),
            protected_tools: Vec::new(),
            min_compress_range: default_min_compress(),
            max_consecutive_nudges: default_max_nudges(),
            absorb_enabled: false,
            use_host_tokens: true,
            nudge_growth_tokens: default_nudge_growth_tokens(),
            nudge_min_growth_tokens: default_nudge_min_growth_tokens(),
            nudge_min_context_pct: default_nudge_min_context_pct(),
            nudge_max_context_pct: default_nudge_max_context_pct(),
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

/// 加载配置。
pub fn load() -> AcpConfig {
    cfg::load_or_default(&config_path())
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

    #[test]
    fn tuned_defaults_should_nudge_more_frequently() {
        let kernel = AcpConfig::default().to_kernel_config();
        assert_eq!(kernel.nudge.growth_floor, 20_000);
        assert_eq!(kernel.nudge.growth_cap, 20_000);
        assert_eq!(kernel.nudge.min_growth_floor, 10_000);
        assert_eq!(kernel.nudge.min_context_limit_pct, 0.30);
        assert_eq!(kernel.nudge.max_context_limit_pct, 0.65);
        assert_eq!(kernel.tiers.tier2_trigger, 3);
        assert_eq!(kernel.tiers.tier3_trigger, 6);
    }
}
