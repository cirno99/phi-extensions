// editor.rs — `str_replace_editor` 工具。
//
// 逐条移植自 Oh My Pi 版 deepseek-enhanced.ts 的 registerStrReplaceEditor
// 与 view/create/str_replace/insert 四个命令，行为尽量保持一致：
// - 只接受绝对路径；
// - view 对目录列出最多 2 层（跳过隐藏项、node_modules、__pycache__，按名排序）；
// - view 对文件输出 6 位右对齐行号，view_range 语义与 pi 一致（-1 表示到结尾）；
// - 输出超过 16000 字符时截断并追加 `<response clipped>`；
// - str_replace 要求 old_str 非空且唯一，否则报错。

use std::fs;
use std::path::{Path, PathBuf};

use phi_ext::phi;
use serde_json::Value;

/// 编辑器输出保留的最大字符数。
const MAX_OUTPUT_CHARS: usize = 16_000;

/// 工具描述（与 pi 版一字不差）。
pub const DESCRIPTION: &str =
    "Custom editing tool for viewing, creating and editing files. Commands: view, create, str_replace, insert. Use absolute paths. old_str must be unique.";

/// 构建工具参数的 JSON Schema。
pub fn schema() -> phi::Schema {
    phi::Schema::object()
        .property(
            "command",
            phi::Schema::string().enum_values(["view", "create", "str_replace", "insert"]),
        )
        .property("path", phi::Schema::string())
        .property("file_text", phi::Schema::string())
        .property("insert_line", phi::Schema::integer())
        .property("new_str", phi::Schema::string())
        .property("old_str", phi::Schema::string())
        .property("view_range", phi::Schema::array(phi::Schema::integer()))
        .required(["command", "path"])
}

/// 注册 `str_replace_editor` 工具。
pub fn register(ext: &mut phi::Extension) {
    ext.register_tool(
        phi::Tool::new(
            "str_replace_editor",
            DESCRIPTION,
            schema(),
            |args: &[u8]| -> Result<phi::ToolResult, String> {
                let text = run(args)?;
                Ok(phi::ToolResult {
                    content: text,
                    ..Default::default()
                })
            },
        )
        .detail_from_args(|args| {
            serde_json::from_slice::<Value>(args)
                .ok()
                .and_then(|value| {
                    let command = value.get("command")?.as_str()?;
                    let path = value.get("path")?.as_str()?;
                    Some(format!("{command} {path}"))
                })
                .unwrap_or_default()
        }),
    );
}

/// 解析参数并分发到具体命令。
pub fn run(args: &[u8]) -> Result<String, String> {
    let params: Value = serde_json::from_slice(args).map_err(|err| format!("参数解析失败：{err}"))?;
    let command = params
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| "缺少参数 command".to_string())?;
    match command {
        "view" => {
            let path = require_path(&params)?;
            let range = parse_view_range(&params)?;
            view_path(&path, range.as_deref())
        }
        "create" => {
            let path = require_path(&params)?;
            let file_text = params.get("file_text").and_then(Value::as_str);
            create_file(&path, file_text)
        }
        "str_replace" => {
            let path = require_path(&params)?;
            let old_str = params.get("old_str").and_then(Value::as_str);
            let new_str = params.get("new_str").and_then(Value::as_str);
            replace_text(&path, old_str, new_str)
        }
        "insert" => {
            let path = require_path(&params)?;
            let insert_line = params.get("insert_line").and_then(Value::as_i64);
            let new_str = params.get("new_str").and_then(Value::as_str);
            insert_text(&path, insert_line, new_str)
        }
        other => Err(format!("未知命令：{other}")),
    }
}

/// 取必填的绝对路径参数。
fn require_path(params: &Value) -> Result<PathBuf, String> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| "缺少参数 path".to_string())?;
    editor_path(path)
}

/// 校验并归一化路径（非空、绝对）。
fn editor_path(path: &str) -> Result<PathBuf, String> {
    if path.trim().is_empty() {
        return Err("path must be a non-empty string".to_string());
    }
    let buf = PathBuf::from(path);
    if !buf.is_absolute() {
        return Err(format!("The path {path} is not an absolute path"));
    }
    Ok(buf)
}

/// 解析 view_range（必须是两个整数）。
fn parse_view_range(params: &Value) -> Result<Option<Vec<i64>>, String> {
    match params.get("view_range") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let items = value
                .as_array()
                .ok_or_else(|| "Invalid view_range. It should be two integers.".to_string())?;
            if items.len() != 2 {
                return Err("Invalid view_range. It should be two integers.".to_string());
            }
            let mut range = Vec::with_capacity(2);
            for item in items {
                let number = item
                    .as_i64()
                    .ok_or_else(|| "Invalid view_range. It should be two integers.".to_string())?;
                range.push(number);
            }
            Ok(Some(range))
        }
    }
}

/// 按字符边界截断输出。
fn clip(text: String) -> String {
    if text.chars().count() <= MAX_OUTPUT_CHARS {
        return text;
    }
    let clipped: String = text.chars().take(MAX_OUTPUT_CHARS).collect();
    format!("{clipped}<response clipped>")
}

/// `view` 命令：列目录或带行号显示文件内容。
fn view_path(path: &Path, view_range: Option<&[i64]>) -> Result<String, String> {
    let meta = fs::metadata(path).map_err(|err| format!("cannot view {}: {err}", path.display()))?;
    if meta.is_dir() {
        if view_range.is_some() {
            return Err("view_range is not allowed for directories".to_string());
        }
        let mut rows = vec![format!("d\t{}", path.display())];
        visit_dir(path, 1, &mut rows);
        let body = format!("{}\n", rows.join("\n"));
        return Ok(format!(
            "Here're the files and directories up to 2 levels deep in {}:\n{}",
            path.display(),
            clip(body)
        ));
    }
    if !meta.is_file() {
        return Err(format!(
            "cannot view {}: not a regular file or directory",
            path.display()
        ));
    }
    let content = fs::read_to_string(path).map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    let all_lines: Vec<&str> = content.split('\n').collect();
    let total = all_lines.len();
    let (initial, final_line) = match view_range {
        None => (1usize, None),
        Some(range) => {
            let initial = range[0];
            let final_line = range[1];
            let valid = initial >= 1
                && (initial as usize) <= total
                && (final_line == -1 || (final_line as usize) <= total)
                && (final_line == -1 || final_line >= initial);
            if !valid {
                return Err(format!("Invalid view_range: [{}, {}].", range[0], range[1]));
            }
            (initial as usize, Some(final_line))
        }
    };
    let selected: Vec<&str> = match final_line {
        None => all_lines.clone(),
        Some(-1) => all_lines[initial - 1..].to_vec(),
        Some(last) => all_lines[initial - 1..last as usize].to_vec(),
    };
    let numbered = selected
        .iter()
        .enumerate()
        .map(|(index, line)| format!("{:>6}  {}", initial + index, line))
        .collect::<Vec<_>>()
        .join("\n");
    let range = match final_line {
        None => String::new(),
        Some(last) => format!(" with view_range=[{}, {}]", initial, last),
    };
    Ok(clip(format!(
        "Here's the content of {} with line numbers (which has a total of {} lines){}:\n{}\n",
        path.display(),
        total,
        range,
        numbered
    )))
}

/// 递归列出目录（最多 2 层），跳过隐藏项与常见缓存目录。
fn visit_dir(dir: &Path, depth: usize, rows: &mut Vec<String>) {
    if depth > 2 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            !name.starts_with('.') && name != "node_modules" && name != "__pycache__"
        })
        .collect();
    children.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    for child in children {
        let is_dir = child.is_dir();
        rows.push(format!("{}\t{}", if is_dir { "d" } else { "f" }, child.display()));
        if is_dir {
            visit_dir(&child, depth + 1, rows);
        }
    }
}

/// `create` 命令：新建文件（已存在则报错）。
fn create_file(path: &Path, file_text: Option<&str>) -> Result<String, String> {
    let Some(file_text) = file_text else {
        return Err("Parameter file_text is required for command: create".to_string());
    };
    if path.exists() {
        return Err(format!(
            "File already exists at: {}. Cannot overwrite files using command create.",
            path.display()
        ));
    }
    fs::write(path, file_text).map_err(|err| format!("cannot write {}: {err}", path.display()))?;
    Ok(format!("New file created successfully at: {}", path.display()))
}

/// `str_replace` 命令：唯一匹配替换。
fn replace_text(path: &Path, old_str: Option<&str>, new_str: Option<&str>) -> Result<String, String> {
    let Some(old_str) = old_str.filter(|s| !s.is_empty()) else {
        return Err("Parameter old_str is required and must not be empty for command: str_replace".to_string());
    };
    let before = fs::read_to_string(path).map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    let matches: Vec<usize> = before.match_indices(old_str).map(|(index, _)| index).collect();
    if matches.is_empty() {
        return Err(format!(
            "No replacement was performed, old_str did not appear verbatim in {}.",
            path.display()
        ));
    }
    if matches.len() > 1 {
        return Err(format!(
            "No replacement was performed. Multiple occurrences of old_str in {}.",
            path.display()
        ));
    }
    let at = matches[0];
    let after = format!(
        "{}{}{}",
        &before[..at],
        new_str.unwrap_or(""),
        &before[at + old_str.len()..]
    );
    fs::write(path, after).map_err(|err| format!("cannot write {}: {err}", path.display()))?;
    Ok(format!("The file {} has been edited successfully.", path.display()))
}

/// `insert` 命令：在第 insert_line 行之后插入 new_str。
fn insert_text(path: &Path, insert_line: Option<i64>, new_str: Option<&str>) -> Result<String, String> {
    let Some(insert_line) = insert_line else {
        return Err("Parameter insert_line is required for command: insert".to_string());
    };
    let Some(new_str) = new_str else {
        return Err("Parameter new_str is required for command: insert".to_string());
    };
    let before = fs::read_to_string(path).map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    let mut lines: Vec<&str> = before.split('\n').collect();
    if insert_line < 0 || insert_line as usize > lines.len() {
        return Err(format!("Invalid insert_line parameter: {insert_line}."));
    }
    let index = insert_line as usize;
    let mut merged: Vec<&str> = Vec::with_capacity(lines.len() + 1);
    merged.extend_from_slice(&lines[..index]);
    merged.extend(new_str.split('\n'));
    merged.extend_from_slice(&lines[index..]);
    lines = merged;
    fs::write(path, lines.join("\n")).map_err(|err| format!("cannot write {}: {err}", path.display()))?;
    Ok(format!("The file {} has been edited successfully.", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("phi-dse-editor-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("建目录应成功");
        dir
    }

    #[test]
    fn view_should_number_lines() {
        let dir = temp_dir("view");
        let file = dir.join("a.txt");
        fs::write(&file, "alpha\nbeta\n").unwrap();
        let out = run(json!({"command": "view", "path": file}).to_string().as_bytes()).unwrap();
        assert!(out.contains("total of 3 lines"));
        assert!(out.contains("     1  alpha"));
        assert!(out.contains("     2  beta"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn view_range_minus_one_should_read_to_end() {
        let dir = temp_dir("range");
        let file = dir.join("a.txt");
        fs::write(&file, "1\n2\n3\n4\n").unwrap();
        let out = run(
            json!({"command": "view", "path": file, "view_range": [2, -1]})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        assert!(out.contains("     2  2"));
        assert!(out.contains("     5  "));
        assert!(!out.contains("     1  1"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn str_replace_should_require_unique_match() {
        let dir = temp_dir("replace");
        let file = dir.join("a.txt");
        fs::write(&file, "x\nx\n").unwrap();
        let err = run(
            json!({"command": "str_replace", "path": file, "old_str": "x", "new_str": "y"})
                .to_string()
                .as_bytes(),
        )
        .unwrap_err();
        assert!(err.contains("Multiple occurrences"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn insert_should_place_after_line() {
        let dir = temp_dir("insert");
        let file = dir.join("a.txt");
        fs::write(&file, "a\nb\n").unwrap();
        run(
            json!({"command": "insert", "path": file, "insert_line": 1, "new_str": "c"})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "a\nc\nb\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_should_reject_existing_file() {
        let dir = temp_dir("create");
        let file = dir.join("a.txt");
        fs::write(&file, "x").unwrap();
        let err = run(
            json!({"command": "create", "path": file, "file_text": "y"})
                .to_string()
                .as_bytes(),
        )
        .unwrap_err();
        assert!(err.contains("already exists"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn relative_path_should_be_rejected() {
        let err = run(json!({"command": "view", "path": "rel.txt"}).to_string().as_bytes()).unwrap_err();
        assert!(err.contains("not an absolute path"));
    }
}