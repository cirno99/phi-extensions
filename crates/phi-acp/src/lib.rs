//! phi-acp —— ACP 上下文压缩 phi 扩展。
//!
//! 本 crate 把两个 TypeScript 项目合并移植为一个 phi 扩展：
//!
//! - **acp-kernel**：模型驱动、三级 LSM 的上下文压缩算法内核（纯逻辑）。
//! - **billion-context**：面向宿主的插件外壳（工具、命令、提示词注入、提醒）。
//!
//! # 模块划分
//!
//! 内核（纯函数、可完整单测）：
//! [`types`] · [`config`] · [`state`] · [`refs`] · [`tokenize`] · [`protected`] ·
//! [`boundaries`] · [`prune`] · [`block_map`] · [`recommend`] · [`nudge`] ·
//! [`truncate`] · [`decompress`] · [`render`] · [`prompts`] · [`compress`]。
//!
//! 扩展外壳：[`runtime`] · [`tools`] · [`commands`] · [`absorb`]。
//!
//! # 与 phi 宿主能力的取舍
//!
//! phi 扩展只暴露 6 个拦截钩子（tool_call / tool_result / before_agent_start /
//! session_before_switch / user_input / turn_stopping）与只读事件，**没有**消息
//! 历史访问、也没有请求体重写钩子。因此本扩展维护自己观测到的消息视图（用户
//! 输入 + 工具调用/结果），在其上运行内核；压缩产生的块摘要通过系统提示词注入
//! 与工具输出回写体现。详见 README。

pub mod absorb;
pub mod absorb_store;
pub mod block_map;
pub mod boundaries;
pub mod commands;
pub mod compress;
pub mod config;
pub mod decompress;
pub mod nudge;
pub mod prompts;
pub mod protected;
pub mod prune;
pub mod recommend;
pub mod refs;
pub mod render;
pub mod runtime;
pub mod session_tokens;
pub mod state;
pub mod tokenize;
pub mod tools;
pub mod truncate;
pub mod types;

/// 当前 Unix 毫秒时间戳。
pub fn time_now_ms() -> u64 {
    phi_ext_common::time::now_ms()
}
