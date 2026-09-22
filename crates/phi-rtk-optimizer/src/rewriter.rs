// rewriter.rs — 命令重写：调用 `rtk rewrite` 子进程并做 shell 安全处理。
//
// 由 pi 版 pi-rtk-optimizer 的以下模块合并移植：
// - src/shell-quote-state.ts      （引号/转义状态机）
// - src/shell-env-prefix.ts       （前导环境赋值切分）
// - src/rtk-executable-resolver.ts（定位 rtk 可执行文件）
// - src/rtk-rewrite-provider.ts   （`rtk rewrite` 调用与退出码语义）
// - src/command-rewriter.ts       （重写决策）
// - src/rtk-command-environment.ts（RTK_DB_PATH 环境前缀）
// - src/rewrite-pipeline-safety.ts（Windows 管道安全改写）
// - src/windows-command-helpers.ts（Windows bash 兼容修正）
//
// 与 pi 版的差异：pi 用 `pi.exec`（宿主提供的带超时子进程），这里直接用
// `std::process::Command` + 轮询 `try_wait` 实现超时。

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::Regex;

/// 环境变量名：rtk 历史库路径。
const RTK_DB_PATH_ENV_NAME: &str = "RTK_DB_PATH";

/// 单引号 shell 值。
const SINGLE_QUOTED_VALUE: &str = r"'(?:'\\''|[^'])*'";

/// 前导环境赋值：`FOO=bar `。
static LEADING_ENV_ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"^((?:[A-Za-z_][A-Za-z0-9_]*=(?:"[^"]*"|{SINGLE_QUOTED_VALUE}|[^\s]+)\s+)*)"#
    ))
    .expect("环境前缀正则应可编译")
});

/// `RTK_DB_PATH=` 赋值（出现在前导环境前缀里）。
static RTK_DB_PATH_ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"(?:^|\s)RTK_DB_PATH=(?:"[^"]*"|{SINGLE_QUOTED_VALUE}|[^\s;]+)"#
    ))
    .expect("RTK_DB_PATH 赋值正则应可编译")
});

/// `export RTK_DB_PATH=...;` 前置语句。
static RTK_DB_PATH_EXPORT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"^export\s+RTK_DB_PATH=(?:"[^"]*"|{SINGLE_QUOTED_VALUE}|[^\s;]+)"#
    ))
    .expect("RTK_DB_PATH export 正则应可编译")
});

/// 带环境前缀的 `export RTK_DB_PATH=...;` 前置语句（用于管道安全改写）。
static LEADING_RTK_DB_PATH_EXPORT_PRELUDE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"^(\s*export\s+RTK_DB_PATH=(?:"(?:\\.|[^"])*"|{SINGLE_QUOTED_VALUE}|[^\s;]+)\s*;\s*)([\s\S]*)$"#
    ))
    .expect("export 前置正则应可编译")
});

/// `... 2>&1` 结尾。
static STDERR_MERGE_SUFFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.*?)(?:\s+)?2>\s*&1\s*$").expect("stderr 合并正则应可编译"));

/// `cd /d <path>` 前缀。
static LEADING_CD_SLASH_D: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*cd\s+/d\s+").expect("cd /d 正则应可编译"));

/// 已存在 `PYTHONIOENCODING=`。
static PYTHONIOENCODING_ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bPYTHONIOENCODING\s*=").expect("PYTHONIOENCODING 正则应可编译")
});

/// 命令链中出现 python。
static PYTHON_INVOCATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(^|[;&|]\s*|&&\s*|\|\|\s*)python(?:3(?:\.\d+)?)?\b")
        .expect("python 正则应可编译")
});

/// 引号/转义状态。
#[derive(Debug, Clone, Copy, Default)]
struct QuoteEscapeState {
    quote: Option<char>,
    escaped: bool,
}

/// 推进一格引号/转义状态；返回 `true` 表示该字符已被状态机消费。
fn advance_quote_escape_state(
    state: &mut QuoteEscapeState,
    character: char,
    quote_chars: &str,
) -> bool {
    if state.escaped {
        state.escaped = false;
        return true;
    }
    if let Some(active) = state.quote {
        if character == '\\' && active != '\'' {
            state.escaped = true;
            return true;
        }
        if character == active {
            state.quote = None;
        }
        return true;
    }
    if character == '\\' {
        state.escaped = true;
        return true;
    }
    if quote_chars.contains(character) {
        state.quote = Some(character);
        return true;
    }
    false
}

/// 取 `index` 处的字符与下一个字符（越界返回 `None`）。
fn read_shell_chars(chars: &[char], index: usize) -> (Option<char>, Option<char>) {
    (chars.get(index).copied(), chars.get(index + 1).copied())
}

/// 切出前导环境赋值与其余命令。
pub fn split_leading_env_assignments(input: &str) -> (String, String) {
    let env_prefix = LEADING_ENV_ASSIGNMENT
        .captures(input)
        .and_then(|caps| caps.get(1))
        .map_or(String::new(), |m| m.as_str().to_string());
    let command = input[env_prefix.len()..].to_string();
    (env_prefix, command)
}

/// 子进程输出。
#[derive(Debug, Clone)]
pub struct ProcOutput {
    /// 退出码。
    pub code: i32,
    /// 标准输出。
    pub stdout: String,
    /// 标准错误。
    pub stderr: String,
}

/// 带超时地运行子进程。
///
/// 超时后杀死子进程并返回 `Err`；启动失败同样返回 `Err`。
pub fn run_with_timeout(program: &str, args: &[&str], timeout_ms: u64) -> Result<ProcOutput, String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| err.to_string())?;

    // 并发抽干 stdout/stderr：若等子进程退出后再读，写满管道缓冲（~64KB）的
    // 子进程会阻塞在写、永不退出，于是被误判为超时。
    let out_reader = child.stdout.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            buf
        })
    });
    let err_reader = child.stderr.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            buf
        })
    });

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("{program} timed out after {timeout_ms}ms"));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) => return Err(err.to_string()),
        }
    };

    let stdout = out_reader.map(|h| h.join().unwrap_or_default()).unwrap_or_default();
    let stderr = err_reader.map(|h| h.join().unwrap_or_default()).unwrap_or_default();
    Ok(ProcOutput {
        code: status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

/// rtk 可执行文件解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtkExecutableResolution {
    /// 实际调用的命令（解析成功时是绝对路径）。
    pub command: String,
    /// 解析到的路径（失败时为 `None`）。
    pub resolved_path: Option<String>,
    /// 使用的解析器：`which` / `where`。
    pub resolver: &'static str,
    /// 解析失败时的告警。
    pub warning: Option<String>,
}

/// 归一化解析输出中的细节文本。
fn trim_resolution_detail(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 去掉包裹的成对引号。
fn strip_wrapping_quotes(value: &str) -> &str {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() < 2 {
        return value;
    }
    let first = chars[0];
    let last = chars[chars.len() - 1];
    if (first == '"' && last == '"') || (first == '\'' && last == '\'') {
        return &value[first.len_utf8()..value.len() - last.len_utf8()];
    }
    value
}

/// 从 `which` / `where` 的输出中取第一个非空路径。
pub fn parse_rtk_executable_path(stdout: &str) -> Option<String> {
    stdout
        .split('\n')
        .map(|line| strip_wrapping_quotes(line.trim()).to_string())
        .find(|candidate| !candidate.is_empty())
}

/// 解析器命令：Windows 用 `where`，其余用 `which`。
fn resolver_command(platform: &str) -> (&'static str, &'static str) {
    if platform == "win32" {
        ("where", "where")
    } else {
        ("which", "which")
    }
}

/// 定位 `rtk` 可执行文件。
pub fn resolve_rtk_executable(platform: &str, timeout_ms: u64) -> RtkExecutableResolution {
    let (program, resolver) = resolver_command(platform);
    match run_with_timeout(program, &["rtk"], timeout_ms) {
        Ok(output) => {
            let resolved = parse_rtk_executable_path(&output.stdout);
            if output.code == 0 {
                if let Some(path) = resolved {
                    return RtkExecutableResolution {
                        command: path.clone(),
                        resolved_path: Some(path),
                        resolver,
                        warning: None,
                    };
                }
            }
            let detail = trim_resolution_detail(if output.stderr.is_empty() {
                &output.stdout
            } else {
                &output.stderr
            });
            let warning = if detail.is_empty() {
                format!("rtk executable path resolution via {resolver} failed")
            } else {
                format!("rtk executable path resolution via {resolver} failed: {detail}")
            };
            RtkExecutableResolution {
                command: "rtk".to_string(),
                resolved_path: None,
                resolver,
                warning: Some(warning),
            }
        }
        Err(err) => RtkExecutableResolution {
            command: "rtk".to_string(),
            resolved_path: None,
            resolver,
            warning: Some(format!(
                "rtk executable path resolution via {resolver} failed: {}",
                trim_resolution_detail(&err)
            )),
        },
    }
}

/// 命令是否已经是 rtk 调用。
fn is_already_rtk(command: &str) -> bool {
    let trimmed = command.trim_start();
    trimmed == "rtk" || trimmed.starts_with("rtk ")
}

/// `rtk rewrite` 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtkRewriteResult {
    /// 是否发生了改写。
    pub changed: bool,
    /// 原始命令。
    pub original_command: String,
    /// 改写后的命令（未改写时等于原始命令）。
    pub rewritten_command: String,
    /// `rtk rewrite` 的退出码。
    pub exit_code: i32,
    /// 失败原因。
    pub error: Option<String>,
}

/// 调用 `rtk rewrite <command>`。
///
/// 退出码语义（与 rtk 约定一致）：
/// - `1` 无匹配（保持原样）
/// - `2` 被拒绝改写（保持原样，带错误信息）
/// - `0` / `3` 成功，stdout 为改写结果
pub fn resolve_rtk_rewrite(
    command: &str,
    platform: &str,
    timeout_ms: u64,
    resolver_timeout_ms: u64,
    executable: Option<&RtkExecutableResolution>,
) -> RtkRewriteResult {
    let unchanged = |exit_code: i32, error: Option<String>| RtkRewriteResult {
        changed: false,
        original_command: command.to_string(),
        rewritten_command: command.to_string(),
        exit_code,
        error,
    };

    if command.trim().is_empty() {
        return unchanged(1, None);
    }
    if is_already_rtk(command) {
        return unchanged(1, None);
    }

    let owned_resolution;
    let resolution = match executable {
        Some(resolution) => resolution,
        None => {
            owned_resolution = resolve_rtk_executable(platform, resolver_timeout_ms);
            &owned_resolution
        }
    };

    match run_with_timeout(&resolution.command, &["rewrite", command], timeout_ms) {
        Ok(output) => match output.code {
            1 => unchanged(1, None),
            2 => {
                let detail = output.stderr.trim();
                unchanged(
                    2,
                    Some(if detail.is_empty() {
                        "rtk denied rewrite".to_string()
                    } else {
                        detail.to_string()
                    }),
                )
            }
            0 | 3 => {
                let rewritten = output.stdout.trim();
                if rewritten.is_empty() {
                    return unchanged(output.code, Some("rtk returned empty output".to_string()));
                }
                if rewritten == command {
                    return unchanged(output.code, None);
                }
                RtkRewriteResult {
                    changed: true,
                    original_command: command.to_string(),
                    rewritten_command: rewritten.to_string(),
                    exit_code: output.code,
                    error: None,
                }
            }
            other => unchanged(other, Some(format!("unexpected exit code {other}"))),
        },
        Err(err) => unchanged(-1, Some(err)),
    }
}

/// 重写决策原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewriteReason {
    /// 改写成功。
    Ok,
    /// 命令为空。
    Empty,
    /// 命令本身就是 rtk 调用。
    AlreadyRtk,
    /// rtk 未给出改写。
    NoMatch,
}

/// 重写决策。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewriteDecision {
    /// 是否改写。
    pub changed: bool,
    /// 原始命令。
    pub original_command: String,
    /// 改写后的命令。
    pub rewritten_command: String,
    /// 决策原因。
    pub reason: RewriteReason,
    /// 附带告警。
    pub warning: Option<String>,
}

/// 计算重写决策。
pub fn compute_rewrite_decision(
    command: &str,
    platform: &str,
    timeout_ms: u64,
    executable: Option<&RtkExecutableResolution>,
) -> RewriteDecision {
    if command.trim().is_empty() {
        return RewriteDecision {
            changed: false,
            original_command: command.to_string(),
            rewritten_command: command.to_string(),
            reason: RewriteReason::Empty,
            warning: None,
        };
    }

    let trimmed_start = command.trim_start();
    let (_, effective) = split_leading_env_assignments(trimmed_start);
    let effective = effective.trim_start();
    if effective == "rtk" || effective.starts_with("rtk ") {
        return RewriteDecision {
            changed: false,
            original_command: command.to_string(),
            rewritten_command: command.to_string(),
            reason: RewriteReason::AlreadyRtk,
            warning: None,
        };
    }

    let result = resolve_rtk_rewrite(command, platform, timeout_ms, 1_000, executable);
    if result.changed {
        return RewriteDecision {
            changed: true,
            original_command: command.to_string(),
            rewritten_command: result.rewritten_command,
            reason: RewriteReason::Ok,
            warning: None,
        };
    }

    RewriteDecision {
        changed: false,
        original_command: command.to_string(),
        rewritten_command: command.to_string(),
        reason: RewriteReason::NoMatch,
        warning: result.error,
    }
}

/// 解析临时目录（与 pi 版逐平台一致）。
fn resolve_temporary_directory(platform: &str) -> String {
    let env = |key: &str| std::env::var(key).ok().filter(|v| !v.trim().is_empty());
    if platform == "win32" {
        if let Some(dir) = env("TEMP").or_else(|| env("TMP")) {
            return dir;
        }
        if let Some(dir) = env("LOCALAPPDATA") {
            return format!("{dir}/Temp");
        }
        if let Some(dir) = env("USERPROFILE") {
            return format!("{dir}/AppData/Local/Temp");
        }
        if let Some(dir) = env("SystemRoot").or_else(|| env("WINDIR")) {
            return format!("{dir}/Temp");
        }
        return "C:/Windows/Temp".to_string();
    }
    env("TMPDIR").or_else(|| env("TMP")).unwrap_or_else(|| "/tmp".to_string())
}

/// rtk 历史库路径：`<temp>/pi-rtk-optimizer/history.db`。
fn temporary_rtk_history_db_path(platform: &str) -> String {
    format!("{}/pi-rtk-optimizer/history.db", resolve_temporary_directory(platform))
}

/// 为 shell 环境变量值加单引号。
fn quote_for_shell_env(value: &str, platform: &str) -> String {
    let normalized = if platform == "win32" {
        value.replace('\\', "/")
    } else {
        value.to_string()
    };
    format!("'{}'", normalized.replace('\'', "'\\''"))
}

/// 是否已在前导环境里显式给出 RTK_DB_PATH。
fn has_leading_rtk_db_path_assignment(command: &str) -> bool {
    let trimmed = command.trim_start();
    let (env_prefix, _) = split_leading_env_assignments(trimmed);
    RTK_DB_PATH_ASSIGNMENT.is_match(&env_prefix) || RTK_DB_PATH_EXPORT.is_match(trimmed)
}

/// 给命令加上隔离的 RTK_DB_PATH，避免污染用户历史库。
pub fn apply_rtk_command_environment(command: &str, platform: &str) -> String {
    if command.trim().is_empty() {
        return command.to_string();
    }
    let inherited = std::env::var(RTK_DB_PATH_ENV_NAME)
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    if has_leading_rtk_db_path_assignment(command) || inherited {
        return command.to_string();
    }
    format!(
        "export {RTK_DB_PATH_ENV_NAME}={}; {command}",
        quote_for_shell_env(&temporary_rtk_history_db_path(platform), platform)
    )
}

/// 拆分出的简单管道。
struct ParsedPipeline {
    segments: Vec<String>,
    separators: Vec<String>,
    suffix: String,
}

/// 解析顶层简单管道（只含 `|` / `|&`，遇到 `&&` / `||` / `;` 停止）。
fn parse_simple_top_level_pipeline(command: &str) -> Option<ParsedPipeline> {
    let chars: Vec<char> = command.chars().collect();
    let mut segments: Vec<String> = Vec::new();
    let mut separators: Vec<String> = Vec::new();
    let mut state = QuoteEscapeState::default();
    let mut segment_start = 0usize;
    let mut suffix = String::new();
    let mut index = 0usize;

    while index < chars.len() {
        let (character, next_character) = read_shell_chars(&chars, index);
        let character = character?;
        let previous_character = if index > 0 { chars[index - 1] } else { '\0' };

        if advance_quote_escape_state(&mut state, character, "\"'`") {
            index += 1;
            continue;
        }

        if (character == '|' && next_character == Some('|'))
            || (character == '&' && next_character == Some('&'))
            || character == ';'
        {
            if separators.is_empty() {
                return None;
            }
            segments.push(char_slice(&chars, segment_start, index));
            suffix = char_slice(&chars, index, chars.len());
            break;
        }

        if character == '|' && previous_character != '>' {
            let separator_length = if next_character == Some('&') { 2 } else { 1 };
            segments.push(char_slice(&chars, segment_start, index));
            separators.push(char_slice(&chars, index, index + separator_length));
            segment_start = index + separator_length;
            index += separator_length;
            continue;
        }

        if character == '&'
            && next_character != Some('>')
            && previous_character != '>'
            && previous_character != '<'
        {
            return None;
        }

        index += 1;
    }

    if separators.is_empty() {
        return None;
    }
    if suffix.is_empty() {
        segments.push(char_slice(&chars, segment_start, chars.len()));
    }

    Some(ParsedPipeline {
        segments,
        separators,
        suffix,
    })
}

/// 按字符下标切片。
fn char_slice(chars: &[char], start: usize, end: usize) -> String {
    chars[start.min(chars.len())..end.min(chars.len())]
        .iter()
        .collect()
}

/// 管道首段的改写计划。
struct ProducerRewritePlan {
    command: String,
    capture_stderr: bool,
}

/// 从管道首段提取 rtk 生产者。
fn extract_producer_rewrite_plan(segment: &str, first_separator: &str) -> Option<ProducerRewritePlan> {
    let trimmed = segment.trim();
    let (env_prefix, command_with_redirect) = split_leading_env_assignments(trimmed);
    if !command_with_redirect.to_ascii_lowercase().starts_with("rtk ") {
        return None;
    }

    if let Some(captures) = STDERR_MERGE_SUFFIX.captures(&command_with_redirect) {
        let command = captures.get(1).map_or("", |m| m.as_str()).trim_end();
        if command.is_empty() {
            return None;
        }
        return Some(ProducerRewritePlan {
            command: format!("{env_prefix}{command}").trim().to_string(),
            capture_stderr: true,
        });
    }

    Some(ProducerRewritePlan {
        command: format!("{env_prefix}{command_with_redirect}")
            .trim()
            .to_string(),
        capture_stderr: first_separator == "|&",
    })
}

/// 构造「先把生产者输出落到临时文件，再喂给消费者」的等价管道。
fn build_buffered_pipeline_command(producer: &ProducerRewritePlan, remainder: &str) -> String {
    let temp = "__pi_rtk_pipe_tmp";
    let status = "__pi_rtk_pipe_status";
    let redirect = if producer.capture_stderr {
        format!("> \"${temp}\" 2>&1")
    } else {
        format!("> \"${temp}\"")
    };
    let cleanup = format!("rm -f \"${temp}\"");
    [
        "{".to_string(),
        format!("{temp}=\"$(mktemp)\" || exit $?;"),
        format!("{status}=0;"),
        format!("trap '{cleanup}' EXIT HUP INT TERM;"),
        format!("{} {redirect};", producer.command),
        format!("{status}=$?;"),
        format!("if [ ${status} -eq 0 ]; then ({remainder}) < \"${temp}\"; {status}=$?; fi;"),
        format!("exit ${status};"),
        "}".to_string(),
    ]
    .join(" ")
}

/// Windows 管道安全改写：`rtk ... | consumer` 在 Windows bash 下会丢输出，
/// 改写为先落盘再消费。非 Windows 直接返回原命令。
pub fn apply_rewritten_command_shell_safety_fixups(command: &str, platform: &str) -> String {
    if platform != "win32" {
        return command.to_string();
    }

    let (environment_prelude, target_command) = match LEADING_RTK_DB_PATH_EXPORT_PRELUDE.captures(command)
    {
        Some(captures) => (
            captures.get(1).map_or(String::new(), |m| m.as_str().to_string()),
            captures.get(2).map_or(String::new(), |m| m.as_str().to_string()),
        ),
        None => (String::new(), command.to_string()),
    };

    let Some(pipeline) = parse_simple_top_level_pipeline(&target_command) else {
        return command.to_string();
    };
    let Some(producer) = extract_producer_rewrite_plan(
        pipeline.segments.first().map_or("", String::as_str),
        pipeline.separators.first().map_or("", String::as_str),
    ) else {
        return command.to_string();
    };

    let remainder = pipeline
        .segments
        .iter()
        .enumerate()
        .skip(1)
        .map(|(index, segment)| {
            let separator = if index == 0 {
                ""
            } else {
                pipeline.separators.get(index).map_or("", String::as_str)
            };
            format!("{separator}{segment}")
        })
        .collect::<String>()
        .trim()
        .to_string();
    if remainder.is_empty() {
        return command.to_string();
    }

    let suffix = if pipeline.suffix.is_empty() {
        String::new()
    } else {
        format!(" {}", pipeline.suffix.trim_start())
    };
    format!(
        "{environment_prelude}{}{suffix}",
        build_buffered_pipeline_command(&producer, &remainder)
    )
}

/// 归一化 Windows 路径以便在 bash 中使用。
fn normalize_windows_path_for_bash(raw_path: &str) -> String {
    let trimmed = raw_path.trim();
    let unquoted = if (trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2)
        || (trimmed.starts_with('\'') && trimmed.ends_with('\'') && trimmed.len() >= 2)
    {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };
    unquoted.replace('\\', "/")
}

/// 为 bash 加双引号。
fn quote_for_bash(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\\\""))
}

/// 解析 `cd /d <path>` 前缀。
fn parse_leading_cd_slash_d(command: &str) -> Option<(String, String, String)> {
    let captures = LEADING_CD_SLASH_D.captures(command)?;
    let prefix_len = captures.get(0).map_or(0, |m| m.end());
    let chars: Vec<char> = command.chars().collect();
    let mut state = QuoteEscapeState::default();
    let mut index = prefix_len;

    while index < chars.len() {
        let (character, next_character) = read_shell_chars(&chars, index);
        let character = character?;
        if advance_quote_escape_state(&mut state, character, "\"'") {
            index += 1;
            continue;
        }
        if character == '&' && next_character == Some('&') {
            return Some((
                char_slice(&chars, prefix_len, index),
                "&&".to_string(),
                char_slice(&chars, index + 2, chars.len()),
            ));
        }
        if character == '|' && next_character == Some('|') {
            return Some((
                char_slice(&chars, prefix_len, index),
                "||".to_string(),
                char_slice(&chars, index + 2, chars.len()),
            ));
        }
        if character == '|' || character == ';' {
            return Some((
                char_slice(&chars, prefix_len, index),
                character.to_string(),
                char_slice(&chars, index + 1, chars.len()),
            ));
        }
        index += 1;
    }

    Some((
        char_slice(&chars, prefix_len, chars.len()),
        String::new(),
        String::new(),
    ))
}

/// Windows bash 兼容修正结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsCompatibilityResult {
    /// 修正后的命令。
    pub command: String,
    /// 实际应用的修正项。
    pub applied: Vec<&'static str>,
}

/// Windows 下 bash 的兼容修正：`cd /d` 与 python UTF-8 输出。
pub fn apply_windows_bash_compatibility_fixes(command: &str, platform: &str) -> WindowsCompatibilityResult {
    if platform != "win32" {
        return WindowsCompatibilityResult {
            command: command.to_string(),
            applied: Vec::new(),
        };
    }

    let mut next = command.to_string();
    let mut applied = Vec::new();

    if let Some((raw_path, operator, tail)) = parse_leading_cd_slash_d(&next) {
        let normalized = quote_for_bash(&normalize_windows_path_for_bash(&raw_path));
        next = if operator.is_empty() {
            format!("cd {normalized}")
        } else {
            format!("cd {normalized} {operator} {}", tail.trim_start())
        };
        applied.push("cd-/d");
    }

    if !PYTHONIOENCODING_ASSIGNMENT.is_match(&next) && PYTHON_INVOCATION.is_match(&next) {
        next = format!("PYTHONIOENCODING=utf-8 {next}");
        applied.push("python-utf8");
    }

    WindowsCompatibilityResult {
        command: next,
        applied,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_leading_env_assignments_should_handle_quoted_values() {
        let (prefix, command) = split_leading_env_assignments("FOO=1 BAR='x y' cargo build");
        assert_eq!(prefix, "FOO=1 BAR='x y' ");
        assert_eq!(command, "cargo build");
    }

    #[test]
    fn split_leading_env_assignments_should_be_empty_without_prefix() {
        let (prefix, command) = split_leading_env_assignments("cargo build");
        assert_eq!(prefix, "");
        assert_eq!(command, "cargo build");
    }

    #[test]
    fn is_already_rtk_should_detect_existing_invocation() {
        assert!(is_already_rtk("rtk ls"));
        assert!(is_already_rtk("  rtk"));
        assert!(!is_already_rtk("rtkfoo"));
    }

    #[test]
    fn compute_rewrite_decision_should_short_circuit_empty_and_rtk() {
        let empty = compute_rewrite_decision("   ", "linux", 100, None);
        assert_eq!(empty.reason, RewriteReason::Empty);
        let already = compute_rewrite_decision("rtk status", "linux", 100, None);
        assert_eq!(already.reason, RewriteReason::AlreadyRtk);
        let already_env = compute_rewrite_decision("FOO=1 rtk status", "linux", 100, None);
        assert_eq!(already_env.reason, RewriteReason::AlreadyRtk);
    }

    #[test]
    fn parse_rtk_executable_path_should_strip_quotes_and_skip_blank_lines() {
        assert_eq!(
            parse_rtk_executable_path("\n  \"/usr/local/bin/rtk\"\n"),
            Some("/usr/local/bin/rtk".to_string())
        );
        assert_eq!(parse_rtk_executable_path(""), None);
    }

    #[test]
    fn strip_wrapping_quotes_should_handle_both_quote_kinds() {
        assert_eq!(strip_wrapping_quotes("'abc'"), "abc");
        assert_eq!(strip_wrapping_quotes("\"abc\""), "abc");
        assert_eq!(strip_wrapping_quotes("abc"), "abc");
        assert_eq!(strip_wrapping_quotes("'"), "'");
    }

    #[test]
    fn run_with_timeout_should_capture_output() {
        let output = run_with_timeout("sh", &["-c", "echo hi"], 2_000).expect("应成功执行");
        assert_eq!(output.code, 0);
        assert_eq!(output.stdout.trim(), "hi");
    }

    #[test]
    fn run_with_timeout_should_report_nonzero_code() {
        let output = run_with_timeout("sh", &["-c", "exit 3"], 2_000).expect("应成功执行");
        assert_eq!(output.code, 3);
    }

    #[test]
    fn run_with_timeout_should_kill_on_timeout() {
        let result = run_with_timeout("sh", &["-c", "sleep 5"], 100);
        assert!(result.is_err(), "超时应返回 Err");
    }

    #[test]
    fn apply_rtk_command_environment_should_prefix_when_missing() {
        // 保证测试不受外部 RTK_DB_PATH 影响。
        std::env::remove_var(RTK_DB_PATH_ENV_NAME);
        let result = apply_rtk_command_environment("rtk ls", "linux");
        assert!(result.starts_with("export RTK_DB_PATH='"), "got {result}");
        assert!(result.ends_with("; rtk ls"), "got {result}");
    }

    #[test]
    fn apply_rtk_command_environment_should_keep_existing_assignment() {
        std::env::remove_var(RTK_DB_PATH_ENV_NAME);
        let original = "RTK_DB_PATH=/x rtk ls";
        assert_eq!(apply_rtk_command_environment(original, "linux"), original);
        let exported = "export RTK_DB_PATH='/y'; rtk ls";
        assert_eq!(apply_rtk_command_environment(exported, "linux"), exported);
    }

    #[test]
    fn apply_rtk_command_environment_should_leave_empty_command() {
        assert_eq!(apply_rtk_command_environment("   ", "linux"), "   ");
    }

    #[test]
    fn quote_for_shell_env_should_escape_single_quotes() {
        assert_eq!(quote_for_shell_env("/tmp/a'b", "linux"), "'/tmp/a'\\''b'");
    }

    #[test]
    fn shell_safety_fixups_should_be_noop_off_windows() {
        let command = "rtk ls | head";
        assert_eq!(apply_rewritten_command_shell_safety_fixups(command, "linux"), command);
    }

    #[test]
    fn shell_safety_fixups_should_buffer_rtk_pipeline_on_windows() {
        let command = "export RTK_DB_PATH='/tmp/x'; rtk ls | head -5";
        let fixed = apply_rewritten_command_shell_safety_fixups(command, "win32");
        assert!(fixed.starts_with("export RTK_DB_PATH='/tmp/x'; "), "got {fixed}");
        assert!(fixed.contains("__pi_rtk_pipe_tmp"), "got {fixed}");
        assert!(fixed.contains("(head -5)"), "got {fixed}");
    }

    #[test]
    fn windows_fixes_should_be_noop_off_windows() {
        let result = apply_windows_bash_compatibility_fixes("cd /d C:\\x && ls", "linux");
        assert_eq!(result.command, "cd /d C:\\x && ls");
        assert!(result.applied.is_empty());
    }

    #[test]
    fn windows_fixes_should_rewrite_cd_and_python() {
        let result = apply_windows_bash_compatibility_fixes("cd /d C:\\proj && python -V", "win32");
        // python 前缀会加在整条命令最前面（与 pi 版一致）。
        assert!(result.command.contains("cd \"C:/proj\" && "), "got {}", result.command);
        assert!(result.command.starts_with("PYTHONIOENCODING=utf-8 "), "got {}", result.command);
        assert_eq!(result.applied, vec!["cd-/d", "python-utf8"]);
    }

    #[test]
    fn windows_fixes_should_not_double_prefix_python_encoding() {
        let result =
            apply_windows_bash_compatibility_fixes("PYTHONIOENCODING=utf-8 python -V", "win32");
        assert_eq!(result.applied, Vec::<&str>::new());
    }
}