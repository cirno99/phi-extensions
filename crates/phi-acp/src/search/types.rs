//! 检索类型 —— 对应 acp-kernel `src/search/types.ts`。
//!
//! 可检索的文档有两类：
//! - **块**（压缩摘要，ref = `bN`）：活跃与失活块都检索（上游的 "inactive fix"）。
//! - **消息**（会话日志里的原文，ref = `mNNNNN`）：让模型定位到被压缩折叠掉的
//!   细节，再解压拥有它的块拿全文。
//!
//! [`SearchAlgorithm`] 是对统一文档集的**无状态**打分器；角色带权重
//! （用户意图 > 助手推理 > 工具噪声）。

/// 文档来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchDocKind {
    /// 压缩块摘要。
    Block,
    /// 历史消息原文。
    Message,
}

impl SearchDocKind {
    /// 上游的字面量（`"block"` / `"message"`）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::Message => "message",
        }
    }
}

/// 消息角色（只对 message 文档有意义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    /// 用户。
    User,
    /// 助手。
    Assistant,
    /// 工具。
    Tool,
}

impl MessageRole {
    /// 上游的字面量。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

/// 一篇可检索文档。
#[derive(Debug, Clone)]
pub struct SearchDoc {
    /// 来源。
    pub kind: SearchDocKind,
    /// 稳定 ref，可直接交给解压：块是 `b3`，消息是 `m00350`。
    pub reference: String,
    /// 参与打分的文本（块是 `topic + summary`，消息是原文）。
    pub text: String,
    /// 展示用标题。
    pub title: String,
    /// 消息角色（块为 `None`），驱动角色加权。
    pub role: Option<MessageRole>,
    /// 拥有这篇文档的块：块就是自身；消息是压缩掉它的那个块
    /// （模型据此知道该解压哪个块看细节）。
    pub block_id: Option<String>,
    /// 拥有块的层级。
    pub tier: Option<u8>,
    /// 近似 token 数（展示用）。
    pub tokens: Option<u64>,
}

/// 一次打分的结果。
///
/// 上游把类型命名为 `ScoredBlock`（历史遗留），但它同样承载消息文档，这里
/// 用 `ScoredDoc` 以免误导。
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredDoc {
    /// 文档 ref。
    pub reference: String,
    /// 相关度。
    pub score: f64,
}

/// 每条角色的分数乘子。默认偏向用户意图、压制工具噪声。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RoleWeights {
    /// 用户消息权重。
    pub user: f64,
    /// 助手消息权重。
    pub assistant: f64,
    /// 工具消息权重。
    pub tool: f64,
    /// 块权重。
    pub block: f64,
}

impl Default for RoleWeights {
    /// 对应上游 `DEFAULT_ROLE_WEIGHTS`。
    fn default() -> Self {
        Self {
            user: 1.5,
            assistant: 1.0,
            tool: 0.6,
            block: 1.0,
        }
    }
}

/// 检索算法：对统一文档集的无状态打分器。
pub trait SearchAlgorithm {
    /// 注册名（`hybrid` / `bm25` / `fuzzy` / `substring`）。
    fn name(&self) -> &'static str;
    /// 人类可读说明。
    fn description(&self) -> &'static str;
    /// 对每篇文档打分。
    fn score(&self, docs: &[SearchDoc], query: &str) -> Vec<ScoredDoc>;
}

/// 一条检索结果。
#[derive(Debug, Clone)]
pub struct SearchResult {
    /// 来源。
    pub kind: SearchDocKind,
    /// 交给解压的 ref：`b3` 或 `m00350`。
    pub reference: String,
    /// 拥有块（消息命中时是压缩掉它的块）。
    pub block_id: Option<String>,
    /// 层级。
    pub tier: u8,
    /// 相关度（已乘角色权重）。
    pub score: f64,
    /// 展示标题。
    pub title: String,
    /// 命中上下文片段。
    pub preview: String,
    /// 消息角色。
    pub role: Option<MessageRole>,
    /// 近似 token 数。
    pub tokens: Option<u64>,
}

/// 检索选项。
#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    /// 算法名；缺省用 [`DEFAULT_ALGORITHM`]。
    pub algorithm: Option<String>,
    /// 返回条数上限。
    pub limit: Option<usize>,
    /// 预览字符数。
    pub preview_length: Option<usize>,
    /// 最低相关度。
    pub min_score: Option<f64>,
    /// 角色权重；缺省用 [`RoleWeights::default`]。
    pub role_weights: Option<RoleWeights>,
}

/// 默认算法（对应上游 `DEFAULT_ALGORITHM`）。
pub const DEFAULT_ALGORITHM: &str = "hybrid";

/// 默认返回条数。
pub const DEFAULT_LIMIT: usize = 10;

/// 默认预览长度（字符）。
pub const DEFAULT_PREVIEW_LENGTH: usize = 200;

/// 默认最低相关度。
pub const DEFAULT_MIN_SCORE: f64 = 0.01;
