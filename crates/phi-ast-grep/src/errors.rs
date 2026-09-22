//! 错误类型。
//!
//! 由 pi 版 pi-ast-grep 的 `src/ast-grep/errors.ts` 移植。自动下载相关错误
//! （`AstGrepDownloadError`）随下载链路一并删除，只保留超时。

/// 子进程收集结果：正常退出或超时/IO 失败。
#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    /// 超时（毫秒）。消息与 pi 版 `SearchTimeoutError` 逐字一致。
    #[error("Search timeout after {0}ms")]
    Timeout(u64),
    /// 启动或等待子进程时的 IO 错误。
    #[error("{0}")]
    Io(#[from] std::io::Error),
}