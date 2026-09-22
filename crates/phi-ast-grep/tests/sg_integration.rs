// sg_integration.rs — 依赖宿主 ast-grep 二进制的端到端集成测试。
//
// 探测 PATH 上是否有可用的 ast-grep（`sg` 或 `ast-grep`），没有则**跳过**
// （打印原因后直接返回），因为该二进制由宿主自备。
//
// 有二进制时：启动扩展二进制走完握手，然后直接经 PXB 发送 `ast_grep_search`
// 与 `ast_grep_replace` 的 ToolInvoke，验证搜索命中、dry-run 不落盘、apply 落盘。

use std::io::BufReader;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use phi_ext::pxb;
use phi_ext_common::json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_phi-ast-grep");
const MIN_BINARY_SIZE_BYTES: u64 = 10_000;

/// 在 PATH 上找一个体积达标的 ast-grep 可执行文件。
fn find_sg() -> Option<PathBuf> {
    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        for name in ["sg", "ast-grep"] {
            let candidate = dir.join(name);
            if let Ok(meta) = std::fs::metadata(&candidate) {
                if meta.is_file() && meta.len() > MIN_BINARY_SIZE_BYTES {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

/// 临时样例目录，Drop 时清理。
struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("phi-ast-grep-it-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        std::fs::write(dir.join("sample.ts"), "console.log(\"hi\");\n").expect("写样例");
        Fixture { dir }
    }

    fn sample(&self) -> PathBuf {
        self.dir.join("sample.ts")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 启动扩展、握手、读完注册帧。`path_override` 非空时覆盖子进程 PATH。
fn start_extension_with_path(path_override: Option<&str>) -> (ChildStdin, BufReader<ChildStdout>, Child) {
    let mut command = Command::new(BIN);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(path) = path_override {
        command.env("PATH", path);
    }
    let mut child = command.spawn().expect("启动扩展");

    let mut stdin = child.stdin.take().expect("stdin");
    let mut reader = BufReader::new(child.stdout.take().expect("stdout"));

    let frame = pxb::read_frame(&mut reader).expect("Hello");
    assert_eq!(frame.header.typ, pxb::TYPE_HELLO);

    let ack = pxb::HelloAck {
        protocol: pxb::PROTOCOL_VERSION,
        phi_version: "test".to_string(),
        cwd: "/tmp".to_string(),
        session_id: "test-session".to_string(),
        extension_dir: "/tmp".to_string(),
    }
    .encode();
    pxb::write_frame(&mut stdin, pxb::TYPE_HELLO_ACK, 0, 0, &ack).expect("HelloAck");

    loop {
        let frame = pxb::read_frame(&mut reader).expect("注册帧");
        if frame.header.typ == pxb::TYPE_READY {
            break;
        }
    }

    (stdin, reader, child)
}

/// 发送一次 ToolInvoke，返回结果的 content。
fn invoke_tool(
    stdin: &mut ChildStdin,
    reader: &mut BufReader<ChildStdout>,
    name: &str,
    args: Value,
) -> String {
    let body = pxb::ToolInvoke {
        name: name.to_string(),
        args: phi_ext_common::json::to_vec(&args).expect("序列化入参"),
    }
    .encode();
    pxb::write_frame(stdin, pxb::TYPE_TOOL_INVOKE, 0, 42, &body).expect("发 ToolInvoke");

    loop {
        let frame = pxb::read_frame(reader).expect("工具结果帧");
        if frame.header.typ == pxb::TYPE_TOOL_RESULT {
            let result = pxb::ToolResultMsg::decode(&frame.body).expect("解码工具结果");
            assert!(!result.is_error, "工具返回错误：{}", result.error);
            return result.content;
        }
    }
}

#[test]
fn search_and_replace_against_real_binary() {
    let Some(_sg) = find_sg() else {
        eprintln!("跳过：PATH 上没有体积达标的 ast-grep（sg / ast-grep）");
        return;
    };

    let fixture = Fixture::new();
    let (mut stdin, mut reader, mut child) = start_extension_with_path(None);
    let sample = fixture.sample().to_string_lossy().into_owned();

    // 1) 搜索命中。
    let search_args = phi_ext_common::json::json!({
        "pattern": "console.log($MSG)",
        "lang": "typescript",
        "paths": [sample.clone()],
    });
    let content = invoke_tool(&mut stdin, &mut reader, "ast_grep_search", search_args);
    assert!(content.contains("console.log"), "搜索结果应命中：{content}");

    // 2) dry-run 改写：不落盘。
    let before = std::fs::read_to_string(fixture.sample()).expect("读样例");
    let dry_args = phi_ext_common::json::json!({
        "pattern": "console.log($MSG)",
        "rewrite": "logger.info($MSG)",
        "lang": "typescript",
        "paths": [sample.clone()],
    });
    let dry_content = invoke_tool(&mut stdin, &mut reader, "ast_grep_replace", dry_args);
    assert!(dry_content.contains("[DRY RUN]"), "应标记 dry-run：{dry_content}");
    assert_eq!(
        std::fs::read_to_string(fixture.sample()).expect("读样例"),
        before,
        "dry-run 不应改动文件"
    );

    // 3) apply 改写：落盘。
    let apply_args = phi_ext_common::json::json!({
        "pattern": "console.log($MSG)",
        "rewrite": "logger.info($MSG)",
        "lang": "typescript",
        "paths": [sample.clone()],
        "dryRun": false,
    });
    let apply_content = invoke_tool(&mut stdin, &mut reader, "ast_grep_replace", apply_args);
    assert!(!apply_content.contains("[DRY RUN]"), "apply 不应标 dry-run");
    let after = std::fs::read_to_string(fixture.sample()).expect("读样例");
    assert!(after.contains("logger.info"), "apply 应改写文件：{after}");

    // 收尾：Shutdown。
    pxb::write_frame(&mut stdin, pxb::TYPE_SHUTDOWN, 0, 0, &[]).expect("Shutdown");
    loop {
        let frame = pxb::read_frame(&mut reader).expect("ShutdownAck");
        if frame.header.typ == pxb::TYPE_SHUTDOWN_ACK {
            break;
        }
    }
    let status = child.wait().expect("等待退出");
    assert!(status.success());
}

#[test]
fn missing_binary_returns_install_hint() {
    // 清空 PATH：扩展不应自动下载，而应返回安装提示。
    let (mut stdin, mut reader, mut child) = start_extension_with_path(Some(""));

    let args = phi_ext_common::json::json!({
        "pattern": "console.log($MSG)",
        "lang": "typescript",
        "paths": ["/tmp"],
    });
    let content = invoke_tool(&mut stdin, &mut reader, "ast_grep_search", args);
    assert!(
        content.contains("binary not found"),
        "应提示未找到二进制：{content}"
    );
    assert!(
        content.contains("npm install -g @ast-grep/cli"),
        "应给出安装指引：{content}"
    );

    pxb::write_frame(&mut stdin, pxb::TYPE_SHUTDOWN, 0, 0, &[]).expect("Shutdown");
    loop {
        let frame = pxb::read_frame(&mut reader).expect("ShutdownAck");
        if frame.header.typ == pxb::TYPE_SHUTDOWN_ACK {
            break;
        }
    }
    let status = child.wait().expect("等待退出");
    assert!(status.success());
}