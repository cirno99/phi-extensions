// main.rs — phi-cache-optimizer 扩展入口。
//
// 由 pi 版 pi-cache-optimizer 移植（**能力子集**）。
//
// 为什么是子集：pi 版的核心优化全部依赖宿主钩子，而 phi 的 Rust SDK
// 目前不提供这些钩子——
// - `before_agent_start` 只给用户提示词，**不给系统提示词及其组成项**，
//   因此「稳定候选前置 / skills 压缩 / session-overview 去抖」都无法实现；
// - 没有 `before_provider_request` / `before_provider_headers`，
//   因此无法注入 `prompt_cache_key`、cache retention 或工具排序；
// - 不推送会话用量事件，因此无法统计缓存命中率与 token 速率
//   （计算函数已在 `phi-ext-common::stats` 就绪）。
//
// 本扩展因此只保留**确实可落地**的部分：配置持久化 + 能力诊断
// （`/cache-optimizer status|doctor|config|enable|disable|reset|stats`），
// 并用 `doctor` 明确报告哪些能力不可用及原因，避免静默失效。

mod commands;
mod config;
mod doctor;
mod runtime;

use phi_ext::{phi, pxb};

fn main() -> Result<(), phi::Error> {
    let mut ext = phi::Extension::new(config::EXTENSION_NAME, env!("CARGO_PKG_VERSION"));
    let shared = runtime::shared();

    commands::register(&mut ext, shared.clone());

    // 会话开始/结束时重载配置，保证手改 config.json 后无需重启扩展。
    ext.subscribe(pxb::Event::SessionStart, move |_ev| {
        shared.borrow_mut().reload();
    });

    ext.run()
}