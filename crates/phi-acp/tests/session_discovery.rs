//! 会话文件定位的端到端验证。
//!
//! # 背景
//!
//! absorb 的门槛与自适应强度都依赖**真实**上下文使用率，而真实值只能从宿主
//! 会话 JSONL 里的 `usage` 读到。要定位那份文件，扩展必须先知道用户的项目
//! 目录：`<phi_home>/session/<ProjectDirName(cwd)>/`。
//!
//! 但宿主启动扩展时把子进程的 cwd 设成了**扩展自己的目录**
//! （`internal/extension/proc.go`：`cmd.Dir = dir`），且没有给子进程设置 `PWD`。
//! 于是扩展进程的 `PWD` 与 cwd 都指向 `~/.phi/extensions/phi-acp`，用它算出的
//! 会话目录根本不存在 —— 扩展只能退回本地估算（实测与宿主真值相差可达 40%）。
//!
//! 修复手段是读宿主（父进程）的 cwd：`/proc/<ppid>/cwd`。本测试验证这条
//! 定位链路真的能拿到文件并解析出正确的 token 数。

use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

/// 环境变量（`PHI_HOME`）是进程级的，测试并行跑会互相干扰，串行化之。
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// 复刻扩展的父进程 cwd 读取逻辑，用于在测试里算出期望的会话目录。
fn parent_cwd() -> Option<PathBuf> {
    let stat = fs::read_to_string("/proc/self/stat").ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    let _state = fields.next()?;
    let ppid = fields.next()?;
    fs::read_link(format!("/proc/{ppid}/cwd")).ok()
}

/// 在临时 `PHI_HOME` 下为父进程 cwd 建一个会话文件，验证定位 + 解析。
///
/// 非 Linux（没有 `/proc`）时跳过：该定位路径本身就是 Linux 专属兜底，
/// 跳过比假装通过更诚实。
#[test]
fn reads_host_tokens_from_parent_cwd_session_dir() {
    let Some(host_cwd) = parent_cwd() else {
        eprintln!("跳过：当前平台没有 /proc，父进程 cwd 不可读");
        return;
    };
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let home = std::env::temp_dir().join(format!("phi-acp-session-probe-{}", std::process::id()));
    let _ = fs::remove_dir_all(&home);
    let dir = home
        .join("session")
        .join(phi_ext_common::paths::project_dir_name(
            &host_cwd.to_string_lossy(),
        ));
    fs::create_dir_all(&dir).expect("建会话目录失败");

    // 与宿主一致的 entry 形状：顶层 `usage`，`total_tokens` 优先。
    let file = dir.join("2026-01-01T00-00-00_probe.jsonl");
    fs::write(
        &file,
        concat!(
            r#"{"type":"session","id":"probe"}"#,
            "\n",
            r#"{"type":"message","usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}}"#,
            "\n",
            r#"{"type":"message","usage":{"prompt_tokens":1234,"completion_tokens":56,"total_tokens":1290}}"#,
            "\n",
        ),
    )
    .expect("写会话文件失败");

    // 清掉可能的旧缓存（来自其它测试或上一次调用）。
    phi_acp::session_tokens::reset_cache();
    let previous = std::env::var("PHI_HOME").ok();
    std::env::set_var("PHI_HOME", &home);

    let tokens = phi_acp::session_tokens::read_context_tokens();

    // 还原环境，避免影响其它测试。
    match previous {
        Some(v) => std::env::set_var("PHI_HOME", v),
        None => std::env::remove_var("PHI_HOME"),
    }
    let _ = fs::remove_dir_all(&home);
    phi_acp::session_tokens::reset_cache();

    assert_eq!(
        tokens,
        Some(1290),
        "应从父进程 cwd 对应的会话目录读到宿主 usage"
    );
}
