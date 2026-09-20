//! JSON 编解码，以 simd-json 为主路径。
//!
//! 为什么用 simd-json：扩展进程每次工具调用都要解析一次入参 JSON
//! （`bash` 的 `{"command": ...}`、`write` 的整段文件内容），这是最热的
//! 解析路径；配置与状态文件的读写也走同一条实现。simd-json 利用 SIMD 指令
//! 做结构定位与转义扫描，并带运行时 CPU 特性探测，无 AVX2 时自动降级。
//!
//! simd-json 要求输入**可变**（解析时就地反转义、就地解析数字），因此接口
//! 分成两类：
//! - `parse_owned*` / `value_owned`：接收 `Vec<u8>` 所有权，零拷贝原地解析。
//!   phi 的 `ToolCallEvent.input` 与 `ToolResultEvent.input` 都是 `Vec<u8>`，
//!   钩子里可以直接走这条路径。
//! - `parse*` / `value`：接收 `&[u8]` / `&str`，内部复制一份再解析。工具处理器
//!   拿到的 `args` 是借用，只能走这条路径。

use serde::de::DeserializeOwned;
use serde::Serialize;

/// 拥有所有权的 JSON 值。
pub type Value = simd_json::OwnedValue;

/// simd-json 的 `json!` 宏，供扩展直接构造 JSON 值。
pub use simd_json::json;
/// 标量 / 数组 / 对象读取与可变对象访问所需的一整套 trait。
pub use simd_json::prelude::{
    MutableObject, ObjectMut, ValueAsArray, ValueAsMutObject, ValueAsObject, ValueAsScalar,
    ValueObjectAccess, Writable,
};

/// 借用输入的 JSON 值（零拷贝读取字符串与数组）。
pub type Borrowed<'a> = simd_json::BorrowedValue<'a>;

/// JSON 编解码错误。
#[derive(Debug, thiserror::Error)]
#[error("JSON 解析失败：{0}")]
pub struct ParseError(#[from] pub simd_json::Error);

/// JSON 序列化错误。
#[derive(Debug, thiserror::Error)]
#[error("JSON 序列化失败：{0}")]
pub struct SerializeError(pub simd_json::Error);

/// 原地解析拥有所有权的字节缓冲。
pub fn parse_owned<T: DeserializeOwned>(mut bytes: Vec<u8>) -> Result<T, ParseError> {
    Ok(simd_json::serde::from_slice(&mut bytes)?)
}

/// 解析借用的字节切片（内部复制一份）。
pub fn parse<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ParseError> {
    parse_owned(bytes.to_vec())
}

/// 解析字符串（内部复制一份）。
pub fn parse_str<T: DeserializeOwned>(s: &str) -> Result<T, ParseError> {
    parse(s.as_bytes())
}

/// 原地解析为通用 JSON 值。
pub fn value_owned(mut bytes: Vec<u8>) -> Result<Value, ParseError> {
    Ok(simd_json::to_owned_value(&mut bytes)?)
}

/// 解析为通用 JSON 值（内部复制一份）。
pub fn value(bytes: &[u8]) -> Result<Value, ParseError> {
    value_owned(bytes.to_vec())
}

/// 原地解析为借用输入的 JSON 值，零拷贝。
pub fn borrowed_value(bytes: &mut [u8]) -> Result<Borrowed<'_>, ParseError> {
    Ok(simd_json::to_borrowed_value(bytes)?)
}

/// 读取对象字段的字符串值。
pub fn get_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key)?.as_str()
}

/// 读取对象字段。
pub fn get<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.get(key)
}

/// 序列化为紧凑 JSON。
pub fn to_string<T: Serialize>(value: &T) -> Result<String, SerializeError> {
    simd_json::to_string(value).map_err(SerializeError)
}

/// 序列化为带缩进的 JSON（配置文件人工可读）。
pub fn to_string_pretty<T: Serialize>(value: &T) -> Result<String, SerializeError> {
    simd_json::to_string_pretty(value).map_err(SerializeError)
}

/// 序列化为字节（工具 schema 等需要 `Vec<u8>` 的场景）。
pub fn to_vec<T: Serialize>(value: &T) -> Result<Vec<u8>, SerializeError> {
    simd_json::to_vec(value).map_err(SerializeError)
}

/// 把实现了 `Serialize` 的值转换为通用 JSON 值。
pub fn to_value<T: Serialize>(value: T) -> Result<Value, SerializeError> {
    simd_json::serde::to_owned_value(value).map_err(SerializeError)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
    struct Args {
        command: String,
        timeout: u32,
    }

    #[test]
    fn parse_str_reads_typed_struct() {
        let got: Args = parse_str(r#"{"command":"ls","timeout":5}"#).expect("解析失败");
        assert_eq!(got, Args { command: "ls".into(), timeout: 5 });
    }

    #[test]
    fn parse_owned_takes_ownership() {
        let bytes = br#"{"command":"pwd","timeout":1}"#.to_vec();
        let got: Args = parse_owned(bytes).expect("解析失败");
        assert_eq!(got.command, "pwd");
    }

    #[test]
    fn parse_reports_error_on_invalid_json() {
        let err = parse::<Args>(b"{oops}").expect_err("应报错");
        assert!(err.to_string().contains("JSON 解析失败"));
    }

    #[test]
    fn get_str_returns_none_for_missing_key() {
        let v = value(br#"{"a":"x"}"#).expect("解析失败");
        assert_eq!(get_str(&v, "a"), Some("x"));
        assert_eq!(get_str(&v, "b"), None);
    }

    #[test]
    fn get_str_returns_none_for_non_string() {
        let v = value(br#"{"a":1}"#).expect("解析失败");
        assert_eq!(get_str(&v, "a"), None);
    }

    #[test]
    fn round_trip_preserves_fields() {
        let args = Args { command: "echo hi".into(), timeout: 9 };
        let text = to_string(&args).expect("序列化失败");
        let back: Args = parse_str(&text).expect("解析失败");
        assert_eq!(back, args);
    }

    #[test]
    fn pretty_output_contains_newline() {
        let args = Args { command: "ls".into(), timeout: 1 };
        let text = to_string_pretty(&args).expect("序列化失败");
        assert!(text.contains('\n'));
    }
}
