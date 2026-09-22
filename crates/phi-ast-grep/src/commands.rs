//! `/ast-grep` 斜杠命令。
//!
//! 由 pi 版 pi-ast-grep 的 `src/index.ts` 里注册的 `ast-grep` 命令移植。
//! 与 pi 的差异：`/ast-grep install` 不再触发下载（本扩展要求宿主自备二进制），
//! 改为打印安装指引。

use std::time::Duration;

use phi_ext::phi;

use crate::binary::find_sg_cli_path;
use crate::cli::INSTALL_HINT;
use crate::process::run_with_timeout;

/// 查询版本时的超时。
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

/// 注册 `/ast-grep` 命令。
pub fn register(ext: &mut phi::Extension) {
    ext.register_command(
        "ast-grep",
        phi::Command::new(
            "Show ast-grep binary path and version",
            |args, ctx| {
                let trimmed = args.trim();
                if trimmed == "install" || trimmed == "download" {
                    ctx.notify(
                        "info",
                        &format!(
                            "This extension does not auto-download ast-grep.\n{INSTALL_HINT}"
                        ),
                    );
                    return Ok(());
                }

                let message = match find_sg_cli_path() {
                    Some(path) => {
                        let version = query_version(&path);
                        format!(
                            "phi-ast-grep\n  Binary : {}\n  Version: {}",
                            path.display(),
                            version
                        )
                    }
                    None => format!("ast-grep binary not found on PATH.\n{INSTALL_HINT}"),
                };
                ctx.notify("info", &message);
                Ok(())
            },
        ),
    );
}

/// 运行 `--version` 取版本字符串。
fn query_version(path: &std::path::Path) -> String {
    match run_with_timeout(path, &["--version".to_string()], VERSION_TIMEOUT, None) {
        Ok(output) => {
            let text = output.stdout.trim();
            if text.is_empty() {
                "unknown".to_string()
            } else {
                text.to_string()
            }
        }
        Err(err) => format!("unavailable ({err})"),
    }
}