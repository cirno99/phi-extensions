// techniques/mod.rs — 输出压缩「技术」集合。
//
// 每个子模块对应 pi 版 pi-rtk-optimizer 的 `src/techniques/*`：
// 先按命令判定是否适用，再把整段输出压成摘要，无法识别时返回 `None`
// （调用方保留原文，绝不因压缩失败而丢信息）。
//
// ANSI 剥离不在此处：统一走 `phi-ext-common::ansi` 的单遍字节扫描，
// 避免同一件事维护两份实现（正则版还会漏剥 `ESC[?25l` 这类私有参数 CSI）。

pub mod build;
pub mod command_detection;
pub mod git;
pub mod linter;
pub mod path_utils;
pub mod search;
pub mod source;
pub mod test_output;
pub mod truncate;