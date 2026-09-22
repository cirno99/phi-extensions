//! ast-grep 二进制解析。
//!
//! 由 pi 版 pi-ast-grep 的 `src/ast-grep/binary-path.ts` 精简移植：**只保留
//! PATH 探测**，删除缓存目录、`@ast-grep/cli` npm 包解析、平台包解析与 GitHub
//! 自动下载（本扩展要求宿主自备 ast-grep 二进制）。
//!
//! 探测顺序：`sg` → `ast-grep`（不同发行渠道装出来的名字不同：cargo/brew 多
//! 半是 `sg`，部分包管理器给的是 `ast-grep`）。

use std::path::{Path, PathBuf};

/// 小于该字节数的候选视为「不是真二进制」（沿用 pi 版阈值，挡住同名脚本/占位）。
const MIN_BINARY_SIZE_BYTES: u64 = 10_000;

/// 可执行文件名候选，按优先级排列。
const BINARY_NAMES: [&str; 2] = ["sg", "ast-grep"];

/// 在 PATH 上探测 ast-grep 可执行文件，找不到返回 `None`。
pub fn find_sg_cli_path() -> Option<PathBuf> {
    for name in BINARY_NAMES {
        if let Some(path) = find_on_path(name) {
            return Some(path);
        }
    }
    None
}

/// 在 PATH 的每个目录里查找指定名字的可执行文件。
fn find_on_path(binary_name: &str) -> Option<PathBuf> {
    let path_env = std::env::var_os("PATH")?;
    let exts: &[&str] = if cfg!(windows) {
        &["", ".exe"]
    } else {
        &[""]
    };
    for dir in std::env::split_paths(&path_env) {
        for ext in exts {
            let candidate = dir.join(format!("{binary_name}{ext}"));
            if is_valid_binary(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// 候选路径是否存在且体积超过阈值。
fn is_valid_binary(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.len() > MIN_BINARY_SIZE_BYTES,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_binary_is_invalid() {
        assert!(!is_valid_binary(Path::new("/definitely/not/here/sg")));
    }

    #[test]
    fn tiny_file_is_rejected_as_placeholder() {
        let dir = std::env::temp_dir().join(format!("phi-ast-grep-bin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建目录");
        let fake = dir.join("sg");
        std::fs::write(&fake, b"#!/bin/sh\n").expect("写文件");
        assert!(!is_valid_binary(&fake), "小脚本应被阈值挡掉");
        let _ = std::fs::remove_dir_all(&dir);
    }
}