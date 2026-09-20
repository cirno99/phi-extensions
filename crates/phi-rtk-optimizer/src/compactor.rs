// compactor.rs — 工具输出压缩总入口。
//
// 由 pi 版 pi-rtk-optimizer 的 src/output-compactor.ts 移植。
//
// 与 pi 版的差异（受 phi 协议影响）：
// - pi 的 tool_result `content` 是「内容块数组」，需要遍历并回写；
//   phi 的 `ToolResultEvent.content` 已是单个字符串，因此压缩直接作用于整串。
// - pi 按「锚点安全」重建 read 输出；这里保留同一套锚点识别与重映射逻辑，
//   但因为不再有内容块包装，banner 直接加在字符串开头。
// - 技能目录按 phi 约定：`~/.phi/skills`、`~/.agents/skills`、`<cwd>/.phi/skills`
//   以及各级祖先目录的 `.agents/skills`。

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use phi_ext_common::paths;

use crate::config::{RtkIntegrationConfig, SourceFilterLevel};
use crate::metrics::OutputMetrics;
use crate::techniques::{
    ansi::strip_ansi_fast,
    build::filter_build_output,
    git::compact_git_output,
    linter::aggregate_linter_output,
    search::group_search_results,
    source::{detect_language, filter_source_code, smart_truncate, FilterLevel},
    test_output::aggregate_test_output,
    truncate::truncate,
};

/// read 输出小于该行数时保持原样。
const READ_EXACT_OUTPUT_LINE_THRESHOLD: usize = 80;
/// read 压缩 banner 前缀。
const READ_COMPACTION_BANNER_PREFIX: &str = "[RTK compacted output:";
/// 锚点行识别所需的最少匹配数。
const ANCHORED_READ_LINE_MIN_MATCHES: usize = 2;
/// 锚点行占比下限。
const ANCHORED_READ_LINE_MIN_RATIO: f64 = 0.5;
/// 锚点行采样上限。
const ANCHORED_READ_LINE_SAMPLE_LIMIT: usize = 200;

/// 会造成信息损失的压缩技术前缀。
const LOSSY_TECHNIQUE_PREFIXES: [&str; 8] = [
    "build",
    "test",
    "git",
    "linter",
    "search",
    "truncate",
    "smart-truncate",
    "source:",
];

/// 锚点行的三种形态。
static ANCHORED_READ_LINE_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"^\s*(?:>>>|>>|[>+\-*]+)?\s*(\d+)\s*#\s*[A-Za-z0-9_-]{2,32}:(.*)$",
        r"^\s*(?:>>>|>>|[>+\-*]+)?\s*(\d+)\s*:\s*[A-Za-z0-9_-]{1,32}\|(.*)$",
        r"^\s*(?:>>>|>>|[>+\-*]+)?\s*(\d+)[a-z]{2}\|(.*)$",
    ]
    .iter()
    .map(|p| Regex::new(p).expect("锚点行正则应可编译"))
    .collect()
});

/// 与锚点无关的信息行（`<file>`、`...`、`[x]`、`Read ...: N lines`）。
static ANCHORED_READ_INFORMATIONAL_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:$|</?file>|\.{3}|\[[^\]]+\]|Read\s+.+:\s+\d+\s+lines\b)")
        .expect("信息行正则应可编译")
});

/// 压缩结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionOutcome {
    /// 内容是否发生变化。
    pub changed: bool,
    /// 压缩后的文本。
    pub text: String,
    /// 生效的技术列表。
    pub techniques: Vec<String>,
    /// 是否发生了有损压缩。
    pub truncated: bool,
    /// 压缩前字符数。
    pub original_chars: usize,
    /// 压缩后字符数。
    pub compacted_chars: usize,
    /// 压缩前行数。
    pub original_lines: usize,
    /// 压缩后行数。
    pub compacted_lines: usize,
}

impl CompactionOutcome {
    /// 未发生压缩的结果。
    fn unchanged(text: &str) -> Self {
        let chars = text.chars().count();
        Self {
            changed: false,
            text: text.to_string(),
            techniques: Vec::new(),
            truncated: false,
            original_chars: chars,
            compacted_chars: chars,
            original_lines: count_lines(text),
            compacted_lines: count_lines(text),
        }
    }
}

/// 文本行数（与 pi 的 `countLines` 一致：去掉末尾换行后再数）。
pub fn count_lines(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let normalized = text.strip_suffix('\n').unwrap_or(text);
    if normalized.is_empty() {
        return 1;
    }
    normalized.split('\n').count()
}

/// 压缩过程中的可变状态。
struct CompactionState {
    text: String,
    techniques: Vec<String>,
}

/// 判断技术列表里是否含损失性压缩。
fn has_lossy_compaction(techniques: &[String]) -> bool {
    techniques.iter().any(|technique| {
        LOSSY_TECHNIQUE_PREFIXES.iter().any(|prefix| {
            if prefix.ends_with(':') {
                technique.starts_with(prefix)
            } else {
                technique == prefix
            }
        })
    })
}

/// 去掉 ANSI（如启用）。
fn apply_ansi_stripping(state: &mut CompactionState, config: &RtkIntegrationConfig) {
    if !config.output_compaction.strip_ansi {
        return;
    }
    let stripped = strip_ansi_fast(&state.text);
    if stripped != state.text {
        state.text = stripped;
        state.techniques.push("ansi".to_string());
    }
}

/// 硬字符截断（如启用且超限）。
fn apply_truncation(state: &mut CompactionState, config: &RtkIntegrationConfig) {
    let compaction = &config.output_compaction;
    if compaction.truncate.enabled && state.text.chars().count() > compaction.truncate.max_chars as usize
    {
        state.text = truncate(&state.text, compaction.truncate.max_chars as usize);
        state.techniques.push("truncate".to_string());
    }
}

/// 应用一个「返回 Option<String>」的压缩技术。
fn apply_nullable_technique(
    state: &mut CompactionState,
    transform: impl Fn(&str) -> Option<String>,
    technique: &str,
) {
    if let Some(compacted) = transform(&state.text) {
        if compacted != state.text {
            state.text = compacted;
            state.techniques.push(technique.to_string());
        }
    }
}

/// 初始化压缩状态（先做 ANSI 剥离）。
fn begin_compaction(text: &str, config: &RtkIntegrationConfig) -> CompactionState {
    let mut state = CompactionState {
        text: text.to_string(),
        techniques: Vec::new(),
    };
    apply_ansi_stripping(&mut state, config);
    state
}

/// bash 输出压缩。
fn compact_bash_text(
    text: &str,
    command: Option<&str>,
    config: &RtkIntegrationConfig,
) -> (String, Vec<String>) {
    let mut state = begin_compaction(text, config);
    let compaction = &config.output_compaction;

    if compaction.filter_build_output {
        apply_nullable_technique(&mut state, |t| filter_build_output(t, command), "build");
    }
    if compaction.aggregate_test_output {
        apply_nullable_technique(&mut state, |t| aggregate_test_output(t, command), "test");
    }
    if compaction.compact_git_output {
        apply_nullable_technique(&mut state, |t| compact_git_output(t, command), "git");
    }
    if compaction.aggregate_linter_output {
        apply_nullable_technique(&mut state, |t| aggregate_linter_output(t, command), "linter");
    }

    apply_truncation(&mut state, config);
    (state.text, state.techniques)
}

/// grep 输出压缩。
fn compact_grep_text(text: &str, config: &RtkIntegrationConfig) -> (String, Vec<String>) {
    let mut state = begin_compaction(text, config);
    if config.output_compaction.group_search_output {
        apply_nullable_technique(&mut state, |t| group_search_results(t, 50), "search");
    }
    apply_truncation(&mut state, config);
    (state.text, state.techniques)
}

/// 把源码过滤强度映射为技术名后缀。
fn filter_level_name(level: SourceFilterLevel) -> FilterLevel {
    match level {
        SourceFilterLevel::None => FilterLevel::None,
        SourceFilterLevel::Minimal => FilterLevel::Minimal,
        SourceFilterLevel::Aggressive => FilterLevel::Aggressive,
    }
}

/// 一条锚点行。
#[derive(Debug, Clone, PartialEq, Eq)]
struct AnchoredReadLine {
    content: String,
    original_line: String,
}

/// 锚点行在整段输出中的位置。
struct AnchorSafeReadParts {
    prefix_lines: Vec<String>,
    anchored_lines: Vec<AnchoredReadLine>,
    suffix_lines: Vec<String>,
    trailing_newline: bool,
}

/// 拆分文本为行，并记录是否以换行结尾。
fn split_read_lines(text: &str) -> (Vec<String>, bool) {
    if text.is_empty() {
        return (Vec::new(), false);
    }
    let trailing_newline = text.ends_with('\n');
    let mut lines: Vec<String> = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
        .collect();
    if trailing_newline {
        lines.pop();
    }
    (lines, trailing_newline)
}

/// 按是否以换行结尾拼回文本。
fn join_read_lines(lines: &[String], trailing_newline: bool) -> String {
    let joined = lines.join("\n");
    if trailing_newline && !joined.is_empty() {
        format!("{joined}\n")
    } else {
        joined
    }
}

/// 解析一条锚点行。
fn parse_anchored_read_line(line: &str) -> Option<AnchoredReadLine> {
    for pattern in ANCHORED_READ_LINE_PATTERNS.iter() {
        let Some(captures) = pattern.captures(line) else {
            continue;
        };
        let line_number = captures
            .get(1)
            .and_then(|m| m.as_str().parse::<u64>().ok())
            .filter(|number| *number > 0);
        if line_number.is_none() {
            continue;
        }
        return Some(AnchoredReadLine {
            content: captures.get(2).map_or(String::new(), |m| m.as_str().to_string()),
            original_line: line.to_string(),
        });
    }
    None
}

/// 判断整段输出是否像锚点行输出。
fn looks_like_anchored_read_output(text: &str) -> bool {
    let (lines, _) = split_read_lines(text);
    let mut match_count = 0usize;
    let mut relevant_line_count = 0usize;
    let mut previous_matched: Option<u64> = None;
    let mut has_increasing_anchors = false;

    for line in lines.iter().take(ANCHORED_READ_LINE_SAMPLE_LIMIT) {
        if !ANCHORED_READ_INFORMATIONAL_LINE.is_match(line) {
            relevant_line_count += 1;
        }
        let Some(anchored) = parse_anchored_read_line(line) else {
            continue;
        };
        match_count += 1;
        let number = anchored
            .original_line
            .split(|c: char| !c.is_ascii_digit())
            .find(|part| !part.is_empty())
            .and_then(|part| part.parse::<u64>().ok())
            .unwrap_or(0);
        if previous_matched.is_some_and(|previous| number > previous) {
            has_increasing_anchors = true;
        }
        previous_matched = Some(number);
    }

    if match_count < ANCHORED_READ_LINE_MIN_MATCHES || !has_increasing_anchors {
        return false;
    }

    let ratio_base = relevant_line_count.max(match_count);
    match_count as f64 / ratio_base as f64 >= ANCHORED_READ_LINE_MIN_RATIO
}

/// 提取前缀行、锚点行与后缀行。
fn extract_anchored_read_parts(text: &str) -> Option<AnchorSafeReadParts> {
    if !looks_like_anchored_read_output(text) {
        return None;
    }

    let (lines, trailing_newline) = split_read_lines(text);
    let parsed: Vec<Option<AnchoredReadLine>> =
        lines.iter().map(|line| parse_anchored_read_line(line)).collect();
    let first_anchor = parsed.iter().position(Option::is_some)?;
    let last_anchor = parsed
        .iter()
        .rposition(Option::is_some)
        .unwrap_or(first_anchor);

    let mut anchored_lines = Vec::new();
    for entry in parsed.iter().take(last_anchor + 1).skip(first_anchor) {
        anchored_lines.push(entry.clone()?);
    }

    Some(AnchorSafeReadParts {
        prefix_lines: lines[..first_anchor].to_vec(),
        anchored_lines,
        suffix_lines: lines[last_anchor + 1..].to_vec(),
        trailing_newline,
    })
}

/// 锚点安全行：原文行 + 其内容（用于重映射比对）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct AnchorSafeReadLine {
    text: String,
    content: String,
}

/// 渲染锚点行正文。
fn render_anchor_safe_read_body(lines: &[AnchorSafeReadLine]) -> String {
    lines
        .iter()
        .map(|line| line.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 渲染含前后缀的完整文本。
fn render_anchor_safe_read_text(parts: &AnchorSafeReadParts, lines: &[AnchorSafeReadLine]) -> String {
    let mut all: Vec<String> = parts.prefix_lines.clone();
    all.extend(lines.iter().map(|line| line.text.clone()));
    all.extend(parts.suffix_lines.iter().cloned());
    join_read_lines(&all, parts.trailing_newline)
}

/// 把压缩后的内容重新映射回原始锚点行（内容匹配不上时退化为新行）。
fn remap_transformed_content(
    source_lines: &[AnchorSafeReadLine],
    transformed_content: &str,
) -> Vec<AnchorSafeReadLine> {
    let (transformed_lines, _) = split_read_lines(transformed_content);
    let mut remapped: Vec<AnchorSafeReadLine> = Vec::new();
    let mut search_start = 0usize;

    for transformed in &transformed_lines {
        let matched = source_lines
            .iter()
            .enumerate()
            .skip(search_start)
            .find(|(_, line)| &line.content == transformed);
        match matched {
            Some((index, line)) => {
                remapped.push(line.clone());
                search_start = index + 1;
            }
            None => remapped.push(AnchorSafeReadLine {
                text: transformed.clone(),
                content: transformed.clone(),
            }),
        }
    }

    remapped
}

/// 按字符预算截断锚点行（保证不切断锚点）。
fn truncate_anchor_safe_read_lines(
    lines: &[AnchorSafeReadLine],
    max_chars: usize,
) -> Vec<AnchorSafeReadLine> {
    if render_anchor_safe_read_body(lines).chars().count() <= max_chars {
        return lines.to_vec();
    }

    let marker = "[RTK anchor-safe truncate: remaining anchored read lines omitted to preserve complete anchors]";
    let mut truncated: Vec<AnchorSafeReadLine> = Vec::new();
    let mut char_count = 0usize;

    for (index, line) in lines.iter().enumerate() {
        let separator_length = if truncated.is_empty() { 0 } else { 1 };
        let next_char_count = char_count + separator_length + line.text.chars().count();
        let remaining_after = lines.len() - index - 1;
        let marker_length = if remaining_after > 0 {
            (if next_char_count > 0 { 1 } else { 0 }) + marker.chars().count()
        } else {
            0
        };

        if next_char_count + marker_length > max_chars {
            let marker_line = AnchorSafeReadLine {
                text: marker.to_string(),
                content: marker.to_string(),
            };
            return if truncated.is_empty() {
                vec![marker_line]
            } else {
                let mut result = truncated;
                result.push(marker_line);
                result
            };
        }

        truncated.push(line.clone());
        char_count = next_char_count;
    }

    truncated
}

/// 锚点输出专用压缩。
fn compact_anchored_read_text(
    text: &str,
    file_path: &str,
    config: &RtkIntegrationConfig,
) -> (String, Vec<String>) {
    let Some(parts) = extract_anchored_read_parts(text) else {
        return (text.to_string(), Vec::new());
    };

    let mut lines: Vec<AnchorSafeReadLine> = parts
        .anchored_lines
        .iter()
        .map(|line| AnchorSafeReadLine {
            text: line.original_line.clone(),
            content: line.content.clone(),
        })
        .collect();
    let mut techniques: Vec<String> = Vec::new();
    let compaction = &config.output_compaction;
    let language = detect_language(file_path);

    if compaction.source_code_filtering_enabled
        && compaction.source_code_filtering != SourceFilterLevel::None
        && should_apply_read_source_filtering(text, config)
    {
        let current: String = lines
            .iter()
            .map(|line| line.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let filtered = filter_source_code(
            &current,
            language,
            filter_level_name(compaction.source_code_filtering),
        );
        let remapped = remap_transformed_content(&lines, &filtered);
        if render_anchor_safe_read_body(&remapped) != render_anchor_safe_read_body(&lines) {
            lines = remapped;
            techniques.push(format!("source:{}", filter_level_name_name(compaction.source_code_filtering)));
        }
    }

    if compaction.smart_truncate.enabled && lines.len() > compaction.smart_truncate.max_lines as usize
    {
        let current: String = lines
            .iter()
            .map(|line| line.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let compacted = smart_truncate(&current, compaction.smart_truncate.max_lines as usize, language);
        let remapped = remap_transformed_content(&lines, &compacted);
        if render_anchor_safe_read_body(&remapped) != render_anchor_safe_read_body(&lines) {
            lines = remapped;
            techniques.push("smart-truncate".to_string());
        }
    }

    if compaction.truncate.enabled
        && render_anchor_safe_read_text(&parts, &lines).chars().count()
            > compaction.truncate.max_chars as usize
    {
        let overhead = render_anchor_safe_read_text(&parts, &[]).chars().count();
        let body_max = (compaction.truncate.max_chars as usize).saturating_sub(overhead).max(1);
        let truncated = truncate_anchor_safe_read_lines(&lines, body_max);
        if render_anchor_safe_read_body(&truncated) != render_anchor_safe_read_body(&lines) {
            lines = truncated;
            techniques.push("truncate".to_string());
        }
    }

    (render_anchor_safe_read_text(&parts, &lines), techniques)
}

/// 源码过滤强度的线上名称。
fn filter_level_name_name(level: SourceFilterLevel) -> &'static str {
    match level {
        SourceFilterLevel::None => "none",
        SourceFilterLevel::Minimal => "minimal",
        SourceFilterLevel::Aggressive => "aggressive",
    }
}

/// 是否需要对该 read 输出做源码过滤（下游还有行/字符兜底时才做）。
fn should_apply_read_source_filtering(text: &str, config: &RtkIntegrationConfig) -> bool {
    let compaction = &config.output_compaction;
    let line_count = count_lines(text);
    (compaction.smart_truncate.enabled && line_count > compaction.smart_truncate.max_lines as usize)
        || (compaction.truncate.enabled
            && text.chars().count() > compaction.truncate.max_chars as usize)
}

/// 技能目录根（phi 约定）。
fn skill_roots() -> Vec<PathBuf> {
    let mut roots = vec![paths::phi_home().join("skills")];
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home).join(".agents").join("skills"));
    }
    roots
}

/// 判断路径是否位于技能目录下（含各级祖先的 `.agents/skills`）。
fn is_skill_read_path(file_path: &str) -> bool {
    if file_path.trim().is_empty() {
        return false;
    }
    let resolved = absolutize(Path::new(file_path));
    if skill_roots()
        .iter()
        .any(|root| is_path_under_root(&resolved, root))
    {
        return true;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if is_path_under_root(&resolved, &cwd.join(".phi").join("skills")) {
        return true;
    }
    is_under_any_ancestor_agents_skills(&resolved, &cwd)
}

/// 相对路径按当前目录补全（不要求文件存在）。
fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        normalize(path)
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        normalize(&cwd.join(path))
    }
}

/// 归一化 `.` / `..`（不做符号链接解析，与 pi 的 `resolve` 语义接近）。
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// 目标是否在根目录之下（含相等）。
fn is_path_under_root(target: &Path, root: &Path) -> bool {
    let target = normalize(target);
    let root = normalize(root);
    target == root || target.starts_with(&root)
}

/// 任一祖先目录的 `.agents/skills` 之下。
fn is_under_any_ancestor_agents_skills(target: &Path, cwd: &Path) -> bool {
    let mut current = normalize(cwd);
    loop {
        if is_path_under_root(target, &current.join(".agents").join("skills")) {
            return true;
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => return false,
        }
    }
}

/// read 是否应完全保留（不做压缩）。
fn should_preserve_exact_read_output(
    text: &str,
    input: &Value,
    config: &RtkIntegrationConfig,
) -> bool {
    let compaction = &config.output_compaction;
    if !compaction.read_compaction.enabled {
        return true;
    }
    if has_explicit_read_range(input) {
        return true;
    }
    if compaction.preserve_exact_skill_reads && is_skill_read_path(normalize_path(input)) {
        return true;
    }
    count_lines(text) <= READ_EXACT_OUTPUT_LINE_THRESHOLD
}

/// 入参是否显式指定了 offset / limit。
fn has_explicit_read_range(input: &Value) -> bool {
    input.get("offset").is_some() || input.get("limit").is_some()
}

/// 取 read 入参里的 path。
fn normalize_path(input: &Value) -> &str {
    input.get("path").and_then(Value::as_str).unwrap_or("")
}

/// 取 bash 入参里的 command。
fn normalize_command(input: &Value) -> Option<&str> {
    input
        .get("command")
        .and_then(Value::as_str)
        .filter(|command| !command.trim().is_empty())
}

/// read 输出压缩。
fn compact_read_text(
    text: &str,
    file_path: &str,
    config: &RtkIntegrationConfig,
    preserve_exact: bool,
) -> (String, Vec<String>) {
    if preserve_exact {
        return (text.to_string(), Vec::new());
    }

    let mut state = begin_compaction(text, config);
    let compaction = &config.output_compaction;

    if looks_like_anchored_read_output(&state.text) {
        let (anchored_text, anchored_techniques) =
            compact_anchored_read_text(&state.text, file_path, config);
        state.text = anchored_text;
        state.techniques.extend(anchored_techniques);
        apply_read_compaction_banner(&mut state);
        return (state.text, state.techniques);
    }

    let language = detect_language(file_path);
    if compaction.source_code_filtering_enabled
        && compaction.source_code_filtering != SourceFilterLevel::None
        && should_apply_read_source_filtering(text, config)
    {
        let technique = format!(
            "source:{}",
            filter_level_name_name(compaction.source_code_filtering)
        );
        let level = filter_level_name(compaction.source_code_filtering);
        apply_nullable_technique(
            &mut state,
            |t| Some(filter_source_code(t, language, level)),
            &technique,
        );
    }

    if compaction.smart_truncate.enabled
        && state.text.split('\n').count() > compaction.smart_truncate.max_lines as usize
    {
        let compacted = smart_truncate(
            &state.text,
            compaction.smart_truncate.max_lines as usize,
            language,
        );
        if compacted != state.text {
            state.text = compacted;
            state.techniques.push("smart-truncate".to_string());
        }
    }

    apply_truncation(&mut state, config);
    apply_read_compaction_banner(&mut state);
    (state.text, state.techniques)
}

/// 在 read 压缩结果前加 banner。
fn apply_read_compaction_banner(state: &mut CompactionState) {
    if !state.techniques.is_empty() && !state.text.starts_with(READ_COMPACTION_BANNER_PREFIX) {
        state.text = format!(
            "{READ_COMPACTION_BANNER_PREFIX} {}]\n{}",
            state.techniques.join(", "),
            state.text
        );
    }
}

/// 压缩单个工具结果。
///
/// `content` 是 phi 协议里的整段字符串；`input` 是工具入参 JSON。
/// 未发生任何变化时 `changed == false`，调用方应保留原文。
pub fn compact_tool_result(
    tool_name: &str,
    input: &Value,
    content: &str,
    config: &RtkIntegrationConfig,
    metrics: Option<&mut OutputMetrics>,
) -> CompactionOutcome {
    if !config.output_compaction.enabled || content.is_empty() {
        return CompactionOutcome::unchanged(content);
    }

    let (text, techniques) = match tool_name {
        "bash" => compact_bash_text(content, normalize_command(input), config),
        "read" => {
            let path = normalize_path(input);
            let preserve = should_preserve_exact_read_output(content, input, config);
            compact_read_text(content, path, config, preserve)
        }
        "grep" => compact_grep_text(content, config),
        _ => return CompactionOutcome::unchanged(content),
    };

    if text == content || techniques.is_empty() {
        return CompactionOutcome::unchanged(content);
    }

    if config.output_compaction.track_savings {
        if let Some(metrics) = metrics {
            metrics.track(content, &text, tool_name, &techniques);
        }
    }

    let original_chars = content.chars().count();
    let compacted_chars = text.chars().count();
    let outcome = CompactionOutcome {
        changed: true,
        text,
        truncated: has_lossy_compaction(&techniques),
        techniques,
        original_chars,
        compacted_chars,
        original_lines: count_lines(content),
        compacted_lines: count_lines(content),
    };
    CompactionOutcome {
        compacted_lines: count_lines(&outcome.text),
        ..outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RtkIntegrationConfig;
    use serde_json::json;

    fn config() -> RtkIntegrationConfig {
        RtkIntegrationConfig::default()
    }

    #[test]
    fn count_lines_should_match_pi_semantics() {
        assert_eq!(count_lines(""), 0);
        assert_eq!(count_lines("a"), 1);
        assert_eq!(count_lines("a\n"), 1);
        assert_eq!(count_lines("a\nb"), 2);
        assert_eq!(count_lines("\n"), 1);
    }

    #[test]
    fn bash_build_output_should_be_compressed() {
        let content = "   Compiling demo v0.1.0\n   Compiling other v0.2.0\n    Finished dev";
        let outcome = compact_tool_result(
            "bash",
            &json!({ "command": "cargo build" }),
            content,
            &config(),
            None,
        );
        assert!(outcome.changed);
        assert_eq!(outcome.text, "[OK] Build successful (2 units compiled)");
        assert_eq!(outcome.techniques, vec!["build".to_string()]);
        // `build` 属于有损技术（会丢弃进度行），与 pi 版判定一致。
        assert!(outcome.truncated);
    }

    #[test]
    fn bash_should_strip_ansi_before_techniques() {
        let content = "\x1b[31m   Compiling\x1b[0m demo v0.1.0";
        let outcome = compact_tool_result(
            "bash",
            &json!({ "command": "cargo build" }),
            content,
            &config(),
            None,
        );
        assert!(outcome.changed);
        assert!(outcome.techniques.contains(&"ansi".to_string()));
        assert!(outcome.techniques.contains(&"build".to_string()));
        assert!(!outcome.text.contains('\x1b'));
    }

    #[test]
    fn unknown_tool_should_be_unchanged() {
        let outcome = compact_tool_result("write", &json!({}), "hello", &config(), None);
        assert!(!outcome.changed);
        assert_eq!(outcome.text, "hello");
    }

    #[test]
    fn disabled_compaction_should_be_unchanged() {
        let mut config = config();
        config.output_compaction.enabled = false;
        let outcome = compact_tool_result("bash", &json!({}), "\x1b[31mx\x1b[0m", &config, None);
        assert!(!outcome.changed);
    }

    #[test]
    fn grep_output_should_be_grouped() {
        let content = "src/a.rs:1:let x = 1;\nsrc/a.rs:2:let y = 2;";
        let outcome = compact_tool_result("grep", &json!({ "pattern": "let" }), content, &config(), None);
        assert!(outcome.changed);
        assert_eq!(outcome.techniques, vec!["search".to_string()]);
        assert!(outcome.text.contains("2 matches in 1 files:"));
    }

    #[test]
    fn short_read_should_be_preserved_exactly() {
        let content = "1#ab:let x = 1;\n2#cd:let y = 2;";
        let outcome = compact_tool_result(
            "read",
            &json!({ "path": "src/a.rs" }),
            content,
            &config(),
            None,
        );
        // 默认 readCompaction.enabled = false → 完全保留
        assert!(!outcome.changed);
    }

    #[test]
    fn read_with_explicit_range_should_be_preserved() {
        let mut config = config();
        config.output_compaction.read_compaction.enabled = true;
        config.output_compaction.truncate.enabled = true;
        config.output_compaction.truncate.max_chars = 1_000;
        let content = (0..200)
            .map(|index| format!("{index}ab:line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let outcome = compact_tool_result(
            "read",
            &json!({ "path": "src/a.rs", "limit": 200 }),
            &content,
            &config,
            None,
        );
        assert!(!outcome.changed, "显式区间应原样保留");
    }

    #[test]
    fn long_read_should_be_truncated_with_banner() {
        let mut config = config();
        config.output_compaction.read_compaction.enabled = true;
        config.output_compaction.truncate.enabled = true;
        config.output_compaction.truncate.max_chars = 1_000;
        let content = (0..300)
            .map(|index| format!("{index}ab:line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let outcome = compact_tool_result(
            "read",
            &json!({ "path": "src/a.rs" }),
            &content,
            &config,
            None,
        );
        assert!(outcome.changed);
        assert!(outcome.text.starts_with(READ_COMPACTION_BANNER_PREFIX), "got {}", outcome.text);
        assert!(outcome.truncated);
        assert!(outcome.techniques.contains(&"truncate".to_string()));
    }

    #[test]
    fn anchored_lines_should_survive_truncation() {
        let mut config = config();
        config.output_compaction.read_compaction.enabled = true;
        config.output_compaction.truncate.enabled = true;
        config.output_compaction.truncate.max_chars = 200;
        // 必须超过 80 行，否则 should_preserve_exact_read_output 会原样保留。
        let content = (1..=120)
            .map(|index| format!("{index}#abc{index}:let value_{index} = {index};"))
            .collect::<Vec<_>>()
            .join("\n");
        let outcome = compact_tool_result(
            "read",
            &json!({ "path": "src/a.rs" }),
            &content,
            &config,
            None,
        );
        assert!(outcome.changed);
        // 保留下来的每一行都必须是完整锚点行
        for line in outcome.text.lines().filter(|line| !line.starts_with('[')) {
            assert!(
                parse_anchored_read_line(line).is_some(),
                "锚点被切断：{line}"
            );
        }
    }

    #[test]
    fn metrics_should_record_savings() {
        let mut metrics = OutputMetrics::default();
        let content = "   Compiling a\n   Compiling b\n    Finished";
        let outcome = compact_tool_result(
            "bash",
            &json!({ "command": "cargo build" }),
            content,
            &config(),
            Some(&mut metrics),
        );
        assert!(outcome.changed);
        assert_eq!(metrics.len(), 1);
        assert!(metrics.summary().contains("calls=1"));
    }

    #[test]
    fn looks_like_anchored_output_should_require_increasing_numbers() {
        assert!(looks_like_anchored_read_output("1#ab:a\n2#cd:b"));
        assert!(!looks_like_anchored_read_output("2#ab:a\n1#cd:b"));
        assert!(!looks_like_anchored_read_output("1#ab:a"));
    }

    #[test]
    fn remap_should_reuse_original_anchor_lines() {
        let source = vec![
            AnchorSafeReadLine {
                text: "1#ab:let a = 1;".to_string(),
                content: "let a = 1;".to_string(),
            },
            AnchorSafeReadLine {
                text: "2#cd:let b = 2;".to_string(),
                content: "let b = 2;".to_string(),
            },
        ];
        let remapped = remap_transformed_content(&source, "let a = 1;\nlet b = 2;");
        assert_eq!(remapped, source);
        let dropped = remap_transformed_content(&source, "let b = 2;");
        assert_eq!(dropped, vec![source[1].clone()]);
    }
}