//! 内置检索算法 —— 对应 `src/search/algorithms/`。
//!
//! 上游还有 `semantic.ts`（embedding 余弦相似度），但它**默认不注册**，且
//! `score()` 是异步的（需要宿主提供 `embed` 函数）。phi 扩展没有异步检索通道，
//! 这里不移植；词法三件套 + hybrid 已覆盖上游默认行为。

pub mod bm25;
pub mod fuzzy;
pub mod hybrid;
pub mod substring;
