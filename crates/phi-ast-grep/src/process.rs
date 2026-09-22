//! 子进程输出收集 + 超时。
//!
//! 由 pi 版 pi-ast-grep 的 `src/ast-grep/process-timeout.ts` 移植。pi 用 Node
//! 的 `data` 事件并发读取；这里用两个 std 线程边读边等，避免管道写满导致子
//! 进程阻塞（大输出时的经典死锁）。超时后直接 kill（等价 SIGKILL；pi 是
//! 先 SIGTERM 再 1s 后 SIGKILL，此处简化为一次性 kill）。

use std::io::Read;
use std::process::{Child, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::errors::ProcessError;

/// 一次子进程调用的产物。
#[derive(Debug, Clone, Default)]
pub struct ProcessOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// 轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// 启动子进程并收集输出，超过 `timeout` 则 kill 并返回超时错误。
///
/// `cwd` 非空且存在时作为子进程工作目录——宿主启动扩展时把 cwd 设成了扩展
/// 目录，若不显式指定，模型传入的**相对路径**会被解析到扩展目录而非项目目录。
pub fn run_with_timeout(
    program: &std::path::Path,
    args: &[String],
    timeout: Duration,
    cwd: Option<&std::path::Path>,
) -> Result<ProcessOutput, ProcessError> {
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        if dir.is_dir() {
            command.current_dir(dir);
        }
    }
    let mut child = command.spawn()?;
    collect_output(&mut child, timeout)
}

/// 并发读取 stdout/stderr，等待退出；超时则 kill。
fn collect_output(child: &mut Child, timeout: Duration) -> Result<ProcessOutput, ProcessError> {
    let stdout_reader = child.stdout.take().map(spawn_reader);
    let stderr_reader = child.stderr.take().map(spawn_reader);

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ProcessError::Timeout(timeout.as_millis() as u64));
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(err) => return Err(ProcessError::Io(err)),
        }
    };

    Ok(ProcessOutput {
        stdout: join_reader(stdout_reader),
        stderr: join_reader(stderr_reader),
        exit_code: status.code().unwrap_or(0),
    })
}

/// 起一个线程把 reader 读到底，返回字节。
fn spawn_reader<R: Read + Send + 'static>(mut reader: R) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = reader.read_to_end(&mut buf);
        buf
    })
}

/// 收敛读取线程，按 UTF-8 有损解码（与 Node 的 utf-8 解码语义接近）。
fn join_reader(handle: Option<JoinHandle<Vec<u8>>>) -> String {
    match handle {
        Some(handle) => match handle.join() {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(_) => String::new(),
        },
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Vec<String> {
        vec!["-c".to_string(), script.to_string()]
    }

    #[test]
    fn collects_stdout_and_exit_code() {
        let out = run_with_timeout(
            std::path::Path::new("sh"),
            &sh("printf 'hello'"),
            Duration::from_secs(5),
            None,
        )
        .expect("应能运行");
        assert_eq!(out.stdout, "hello");
        assert_eq!(out.exit_code, 0);
    }

    #[test]
    fn reports_nonzero_exit_code() {
        let out = run_with_timeout(
            std::path::Path::new("sh"),
            &sh("exit 3"),
            Duration::from_secs(5),
            None,
        )
        .expect("应能运行");
        assert_eq!(out.exit_code, 3);
    }

    #[test]
    fn times_out_and_kills() {
        let err = run_with_timeout(
            std::path::Path::new("sh"),
            &sh("sleep 5"),
            Duration::from_millis(100),
            None,
        )
        .expect_err("应超时");
        assert!(matches!(err, ProcessError::Timeout(_)));
        assert!(err.to_string().contains("Search timeout after"));
    }

    #[test]
    fn runs_in_requested_working_directory() {
        let dir = std::env::temp_dir().join(format!("phi-ast-grep-cwd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建目录");
        let out = run_with_timeout(
            std::path::Path::new("sh"),
            &sh("pwd"),
            Duration::from_secs(5),
            Some(&dir),
        )
        .expect("应能运行");
        // macOS 上 /tmp 是 /private/tmp 的符号链接，比较结尾更稳妥。
        let printed = out.stdout.trim();
        let expected = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        let got = std::path::Path::new(printed)
            .canonicalize()
            .unwrap_or_else(|_| std::path::PathBuf::from(printed));
        assert_eq!(got, expected);
        let _ = std::fs::remove_dir_all(&dir);
    }
}