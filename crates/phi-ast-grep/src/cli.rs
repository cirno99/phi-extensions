//! `sg run` 子进程调用。
//!
//! 由 pi 版 pi-ast-grep 的 `src/ast-grep/cli.ts` 移植：参数构建、退出码语义、
//! 以及 replace 的「读 pass + 写 pass」两段式。
//!
//! 关于双 pass 的巧思：`--json=compact` 会让 ast-grep 只输出 JSON、**不落盘**，
//! 即便同时带了 `--update-all`（实测 ast-grep 0.44）。因此读 pass 用
//! `--json=compact` 拿到匹配清单但不动文件；写 pass 去掉 `--json` 并加
//! `--update-all` 才真正改写。

use std::path::Path;
use std::time::Duration;

use crate::binary::find_sg_cli_path;
use crate::errors::ProcessError;
use crate::json_output::create_sg_result_from_stdout;
use crate::process::{run_with_timeout, ProcessOutput};
use crate::types::{RunSgOptions, SgResult, SgTruncationReason, DEFAULT_TIMEOUT_MS};
use crate::cwd::project_cwd;

/// 找不到二进制时的安装提示。
pub const INSTALL_HINT: &str = "ast-grep (sg) binary not found.\n\nInstall options:\n  npm install -g @ast-grep/cli\n  cargo install ast-grep --locked\n  brew install ast-grep";

/// 构建 `sg run` 的参数。
///
/// `include_update_all` 表示本次调用是否允许带 `--update-all`（即「主调用」，
/// 而非独立的写 pass）。
pub fn build_sg_args(options: &RunSgOptions, include_update_all: bool) -> Vec<String> {
    let is_write_pass = options.update_all && !include_update_all;
    let mut args = vec![
        "run".to_string(),
        "-p".to_string(),
        options.pattern.clone(),
        "--lang".to_string(),
        options.lang.clone(),
    ];

    if !is_write_pass {
        args.push("--json=compact".to_string());
    }

    if let Some(rewrite) = &options.rewrite {
        args.push("-r".to_string());
        args.push(rewrite.clone());
        if include_update_all {
            args.push("--update-all".to_string());
        }
    }

    if let Some(context) = options.context {
        if context > 0 {
            args.push("-C".to_string());
            args.push(context.to_string());
        }
    }

    for glob in &options.globs {
        args.push("--globs".to_string());
        args.push(glob.clone());
    }

    if options.paths.is_empty() {
        args.push(".".to_string());
    } else {
        args.extend(options.paths.iter().cloned());
    }

    args
}

/// 执行一次 `sg run`（含 replace 的写 pass）。
pub fn run_sg(options: &RunSgOptions) -> SgResult {
    let should_separate_write_pass = options.rewrite.is_some() && options.update_all;

    let read_options = if should_separate_write_pass {
        RunSgOptions {
            update_all: false,
            ..options.clone()
        }
    } else {
        options.clone()
    };
    let args = build_sg_args(&read_options, !should_separate_write_pass);

    let Some(cli_path) = find_sg_cli_path() else {
        return SgResult {
            error: Some(INSTALL_HINT.to_string()),
            ..Default::default()
        };
    };

    let timeout = Duration::from_millis(DEFAULT_TIMEOUT_MS);
    // 显式指定子进程工作目录：宿主把扩展进程 cwd 设成了扩展目录，模型传入的
    // 相对路径需要相对项目目录解析。
    let cwd = project_cwd();

    let output = match spawn_sg(&cli_path, &args, timeout, Path::new(&cwd)) {
        Ok(output) => output,
        Err(ProcessError::Timeout(ms)) => {
            return SgResult {
                truncated: true,
                truncated_reason: Some(SgTruncationReason::Timeout),
                error: Some(format!("Search timeout after {ms}ms")),
                ..Default::default()
            };
        }
        Err(ProcessError::Io(err)) => {
            return SgResult {
                error: Some(format!("Failed to spawn ast-grep: {err}")),
                ..Default::default()
            };
        }
    };

    if output.exit_code != 0 && output.stdout.trim().is_empty() {
        if output.stderr.contains("No files found") {
            return SgResult::default();
        }
        if !output.stderr.trim().is_empty() {
            return SgResult {
                error: Some(output.stderr.trim().to_string()),
                ..Default::default()
            };
        }
        return SgResult::default();
    }

    let json_result = create_sg_result_from_stdout(&output.stdout);

    if should_separate_write_pass && !json_result.matches.is_empty() {
        let mut write_args = build_sg_args(options, false);
        write_args.push("--update-all".to_string());

        match spawn_sg(&cli_path, &write_args, timeout, Path::new(&cwd)) {
            Ok(write_output) if write_output.exit_code != 0 => {
                let detail = if !write_output.stderr.trim().is_empty() {
                    write_output.stderr.trim().to_string()
                } else {
                    format!("ast-grep exited with code {}", write_output.exit_code)
                };
                return SgResult {
                    error: Some(format!("Replace failed: {detail}")),
                    ..json_result
                };
            }
            Ok(_) => {}
            Err(ProcessError::Timeout(ms)) => {
                return SgResult {
                    error: Some(format!("Replace failed: Search timeout after {ms}ms")),
                    ..json_result
                };
            }
            Err(ProcessError::Io(err)) => {
                return SgResult {
                    error: Some(format!("Replace failed: {err}")),
                    ..json_result
                };
            }
        }
    }

    json_result
}

/// 启动 sg 并收集输出。
fn spawn_sg(
    cli_path: &Path,
    args: &[String],
    timeout: Duration,
    cwd: &Path,
) -> Result<ProcessOutput, ProcessError> {
    run_with_timeout(cli_path, args, timeout, Some(cwd))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn search_options() -> RunSgOptions {
        RunSgOptions {
            pattern: "console.log($MSG)".to_string(),
            lang: "typescript".to_string(),
            paths: vec!["src".to_string()],
            ..Default::default()
        }
    }

    #[test]
    fn search_args_use_compact_json() {
        let args = build_sg_args(&search_options(), false);
        assert_eq!(
            args,
            vec![
                "run",
                "-p",
                "console.log($MSG)",
                "--lang",
                "typescript",
                "--json=compact",
                "src"
            ]
        );
    }

    #[test]
    fn context_inserted_before_paths() {
        let mut options = search_options();
        options.context = Some(3);
        let args = build_sg_args(&options, false);
        assert_eq!(
            args,
            vec![
                "run",
                "-p",
                "console.log($MSG)",
                "--lang",
                "typescript",
                "--json=compact",
                "-C",
                "3",
                "src"
            ]
        );
    }

    #[test]
    fn rewrite_dry_pass_has_no_update_all() {
        let mut options = search_options();
        options.rewrite = Some("logger.info($MSG)".to_string());
        let args = build_sg_args(&options, false);
        assert_eq!(
            args,
            vec![
                "run",
                "-p",
                "console.log($MSG)",
                "--lang",
                "typescript",
                "--json=compact",
                "-r",
                "logger.info($MSG)",
                "src"
            ]
        );
        assert!(!args.iter().any(|a| a == "--update-all"));
    }

    #[test]
    fn rewrite_update_pass_includes_update_all() {
        let mut options = search_options();
        options.rewrite = Some("logger.info($MSG)".to_string());
        let args = build_sg_args(&options, true);
        assert_eq!(
            args,
            vec![
                "run",
                "-p",
                "console.log($MSG)",
                "--lang",
                "typescript",
                "--json=compact",
                "-r",
                "logger.info($MSG)",
                "--update-all",
                "src"
            ]
        );
    }

    #[test]
    fn globs_are_repeated() {
        let mut options = search_options();
        options.globs = vec!["**/*.ts".to_string(), "!**/*.test.ts".to_string()];
        let args = build_sg_args(&options, false);
        assert_eq!(
            args,
            vec![
                "run",
                "-p",
                "console.log($MSG)",
                "--lang",
                "typescript",
                "--json=compact",
                "--globs",
                "**/*.ts",
                "--globs",
                "!**/*.test.ts",
                "src"
            ]
        );
    }

    #[test]
    fn undefined_paths_default_to_dot() {
        let options = RunSgOptions {
            pattern: "x".to_string(),
            lang: "rust".to_string(),
            ..Default::default()
        };
        assert_eq!(build_sg_args(&options, false).last().unwrap(), ".");
    }

    #[test]
    fn write_pass_omits_compact_json() {
        let mut options = search_options();
        options.rewrite = Some("logger.info($MSG)".to_string());
        options.update_all = true;
        let args = build_sg_args(&options, false);
        assert_eq!(
            args,
            vec![
                "run",
                "-p",
                "console.log($MSG)",
                "--lang",
                "typescript",
                "-r",
                "logger.info($MSG)",
                "src"
            ]
        );
        assert!(!args.iter().any(|a| a == "--json=compact"));
    }
}