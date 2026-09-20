// pbx_handshake.rs — PXB 生命周期端到端冒烟测试。
//
// 以子进程方式启动本 crate 的扩展二进制，走完
// Hello（扩展→宿主）→ HelloAck（宿主→扩展）→ Register* → Ready
// → Shutdown → ShutdownAck，并校验注册出来的命令/工具名，
// 确保扩展真的能被宿主加载。

use std::io::BufReader;
use std::process::{Command, Stdio};

use phi_ext::pxb;

/// 本 crate 的扩展二进制路径（由 cargo 注入）。
const BIN: &str = env!("CARGO_BIN_EXE_phi-deepseek-enhanced");

/// 扩展名（Hello 里自报的名字）。
const EXTENSION_NAME: &str = "phi-deepseek-enhanced";

/// 期望注册的斜杠命令。
const EXPECTED_COMMANDS: &[&str] = &["deepseek"];

/// 期望注册的 LLM 工具。
const EXPECTED_TOOLS: &[&str] = &["str_replace_editor"];

#[test]
fn extension_should_complete_pbx_lifecycle() {
    let mut child = Command::new(BIN)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("应能启动扩展二进制");

    let mut stdin = child.stdin.take().expect("应能写入扩展 stdin");
    let mut reader = BufReader::new(child.stdout.take().expect("应能读取扩展 stdout"));

    // 1) 扩展先发 Hello。
    let frame = pxb::read_frame(&mut reader).expect("应能收到 Hello");
    assert_eq!(frame.header.typ, pxb::TYPE_HELLO, "首帧应为 Hello");
    let hello = pxb::Hello::decode(&frame.body).expect("应能解码 Hello");
    assert_eq!(hello.name, EXTENSION_NAME, "扩展名应为 {}", EXTENSION_NAME);
    assert_eq!(hello.protocol, pxb::PROTOCOL_VERSION);

    // 2) 宿主回 HelloAck。
    let ack = pxb::HelloAck {
        protocol: pxb::PROTOCOL_VERSION,
        phi_version: "test".to_string(),
        cwd: "/tmp".to_string(),
        session_id: "test-session".to_string(),
        extension_dir: "/tmp".to_string(),
    }
    .encode();
    pxb::write_frame(&mut stdin, pxb::TYPE_HELLO_ACK, 0, 0, &ack).expect("应能发送 HelloAck");

    // 3) 收集 Register*，直到 Ready。
    let mut commands: Vec<String> = Vec::new();
    let mut tools: Vec<String> = Vec::new();
    loop {
        let frame = pxb::read_frame(&mut reader).expect("应能收到注册帧");
        match frame.header.typ {
            pxb::TYPE_REGISTER_COMMAND => {
                let message = pxb::RegisterCommand::decode(&frame.body).expect("解码命令注册");
                assert!(!message.name.is_empty(), "命令名不应为空");
                commands.push(message.name);
            }
            pxb::TYPE_REGISTER_TOOL => {
                let message = pxb::RegisterTool::decode(&frame.body).expect("解码工具注册");
                assert!(!message.name.is_empty(), "工具名不应为空");
                tools.push(message.name);
            }
            // Subscribe 声明订阅/拦截兴趣，内容不参与本测试断言。
            pxb::TYPE_SUBSCRIBE => {}
            pxb::TYPE_READY => break,
            other => panic!("注册阶段出现了意外帧类型：{other}"),
        }
    }

    for expected in EXPECTED_COMMANDS {
        assert!(
            commands.iter().any(|name| name == expected),
            "缺少命令 {expected}，实际：{commands:?}"
        );
    }
    for expected in EXPECTED_TOOLS {
        assert!(
            tools.iter().any(|name| name == expected),
            "缺少工具 {expected}，实际：{tools:?}"
        );
    }

    // 4) Shutdown → ShutdownAck。
    pxb::write_frame(&mut stdin, pxb::TYPE_SHUTDOWN, 0, 0, &[]).expect("应能发送 Shutdown");
    loop {
        let frame = pxb::read_frame(&mut reader).expect("应能收到 ShutdownAck");
        if frame.header.typ == pxb::TYPE_SHUTDOWN_ACK {
            break;
        }
    }

    let status = child.wait().expect("扩展应正常退出");
    assert!(status.success(), "扩展退出码非零：{status:?}");
}