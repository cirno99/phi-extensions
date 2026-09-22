//! 项目工作目录解析。
//!
//! phi 的工具处理器拿不到命令侧 `Context`，无法直接读 `ctx.cwd()`。而宿主
//! 启动扩展时把子进程 cwd 设成了**扩展自己的目录**（`cmd.Dir = <扩展目录>`），
//! 所以扩展进程的 cwd/PWD 都指向 `~/.phi/extensions/<name>`，不是用户项目目录。
//!
//! 沿用 phi-acp 的既有做法，按可信度取候选：
//! 1. 父进程 cwd（`/proc/<ppid>/cwd`，Linux）——宿主的 cwd 才是项目目录；
//! 2. `PWD` 环境变量；
//! 3. `std::env::current_dir()`（最后兜底）。
//!
//! **已知降级**：非 Linux 没有 `/proc`，父进程 cwd 不可读，只能退回 PWD /
//! current_dir，两者都可能指向扩展目录——此时默认 `paths` 会落到扩展目录。
//! 与 phi-acp 的定位限制一致（SDK 未把宿主的 `SessionMeta.cwd` 转发给工具
//! 处理器，扩展无从拿到权威值）。

use std::fs;

/// 解析当前项目工作目录（作为工具默认 `paths`）。
pub fn project_cwd() -> String {
    if let Some(cwd) = parent_process_cwd() {
        return cwd;
    }
    if let Some(pwd) = std::env::var("PWD").ok().filter(|v| !v.trim().is_empty()) {
        return pwd;
    }
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string())
}

/// 宿主（父进程）的工作目录。非 Linux 或读取失败时返回 `None`。
fn parent_process_cwd() -> Option<String> {
    let stat = fs::read_to_string("/proc/self/stat").ok()?;
    // 格式：`pid (comm) state ppid ...`；`comm` 可能含空格 / 括号，必须从
    // **最后一个** `)` 之后切分才可靠。
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    let _state = fields.next()?;
    let ppid = fields.next()?;
    let cwd = fs::read_link(format!("/proc/{ppid}/cwd")).ok()?;
    Some(cwd.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_cwd_is_non_empty() {
        assert!(!project_cwd().trim().is_empty());
    }
}