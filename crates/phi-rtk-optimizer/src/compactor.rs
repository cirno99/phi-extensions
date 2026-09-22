// compactor.rs — 工具输出压缩总入口。
//
// 由 pi 版 pi-rtk-optimizer 的 src/output-compactor.ts 移植。
//
// 与 pi 版的差异（受 phi 协议影响）：
// - pi 的 tool_result `content` 是「内容块数组」，需要遍历并回写；
//   phi 的 `ToolResultEvent.content` 已是单个字符串，因此压缩直接作用于整串。
// - 技能目录按 phi 约定：`~/.phi/skills`、`~/.agents/skills`、`<cwd>/.phi/skills`
//   以及各级祖先目录的 `.agents/skills`。
//
// 与 pi 版的差异（性能）：
// - 中间量全部放进调用方提供的竞技场：行索引、错误块、失败块、锚点行、
//   源码过滤结果都借用 `&str`，不再逐行/逐条 `String` 克隆。
// - 命令只归一化一次（原先 4 类技术各自归一化，同一条命令被解析 6 次）。
// - 压缩正文用 `Cow`：没有任何技术命中时不产生拷贝。
// - ANSI 剥离复用 `phi-ext-common::ansi` 的单遍扫描，且无转义序列时零分配。

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use bumpalo::collections::Vec as ArenaVec;
use bumpalo::Bump;
use regex::Regex;
use phi_ext_common::json::{Value, ValueAsScalar, ValueObjectAccess};

use phi_ext_common::ansi::{has_ansi, strip_ansi};
use phi_ext_common::arena::split_lines;
use phi_ext_common::paths;

use crate::config::{RtkIntegrationConfig, SourceFilterLevel};
use crate::metrics::OutputMetrics;
use crate::techniques::{
    build::filter_build_output,
    command_detection::normalize_command_for_detection,
    git::compact_git_output,
    linter::aggregate_linter_output,
    search::{group_search_results, is_search_command},
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
        let lines = count_lines(text);
        Self {
            changed: false,
            text: text.to_string(),
            techniques: Vec::new(),
            truncated: false,
            original_chars: chars,
            compacted_chars: chars,
            original_lines: lines,
            compacted_lines: lines,
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
///
/// `text` 用 `Cow`：没有任何技术命中时保持对输入或竞技场内容的借用，避免拷贝。
struct CompactionState<'a> {
    text: Cow<'a, str>,
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

/// 初始化压缩状态（先做 ANSI 剥离，无转义序列时零分配）。
fn begin_compaction<'a>(text: &'a str, config: &RtkIntegrationConfig) -> CompactionState<'a> {
    let mut state = CompactionState {
        text: Cow::Borrowed(text),
        techniques: Vec::new(),
    };
    if config.output_compaction.strip_ansi && has_ansi(text) {
        state.text = Cow::Owned(strip_ansi(text));
        state.techniques.push("ansi".to_string());
    }
    state
}

/// 硬字符截断（如启用且超限）。
fn apply_truncation(state: &mut CompactionState<'_>, config: &RtkIntegrationConfig) {
    let compaction = &config.output_compaction;
    let max_chars = compaction.truncate.max_chars as usize;
    // 字节数是字符数的上界：不超过上限时必然无需截断，省掉一次全量字符计数。
    if compaction.truncate.enabled
        && state.text.len() > max_chars
        && state.text.chars().count() > max_chars
    {
        state.text = Cow::Owned(truncate(&state.text, max_chars));
        state.techniques.push("truncate".to_string());
    }
}

/// 应用一个「返回 Option<String>」的压缩技术。
fn apply_nullable_technique(
    state: &mut CompactionState<'_>,
    transform: impl Fn(&str) -> Option<String>,
    technique: &str,
) {
    if let Some(compacted) = transform(&state.text) {
        if compacted.as_str() != state.text.as_ref() {
            state.text = Cow::Owned(compacted);
            state.techniques.push(technique.to_string());
        }
    }
}

/// bash 输出压缩。
fn compact_bash_text<'a>(
    arena: &'a Bump,
    text: &'a str,
    normalized_command: Option<&str>,
    config: &RtkIntegrationConfig,
) -> (String, Vec<String>) {
    let mut state = begin_compaction(text, config);
    let compaction = &config.output_compaction;

    if compaction.filter_build_output {
        apply_nullable_technique(
            &mut state,
            |t| filter_build_output(arena, t, normalized_command),
            "build",
        );
    }
    if compaction.aggregate_test_output {
        apply_nullable_technique(
            &mut state,
            |t| aggregate_test_output(arena, t, normalized_command),
            "test",
        );
    }
    if compaction.compact_git_output {
        apply_nullable_technique(
            &mut state,
            |t| compact_git_output(arena, t, normalized_command),
            "git",
        );
    }
    if compaction.aggregate_linter_output {
        apply_nullable_technique(
            &mut state,
            |t| aggregate_linter_output(arena, t, normalized_command),
            "linter",
        );
    }
    if compaction.group_search_output && is_search_command(normalized_command) {
        apply_nullable_technique(
            &mut state,
            |t| group_search_results(arena, t, 50),
            "search",
        );
    }

    apply_truncation(&mut state, config);
    (state.text.into_owned(), state.techniques)
}

/// grep 输出压缩。
fn compact_grep_text<'a>(
    arena: &'a Bump,
    text: &'a str,
    config: &RtkIntegrationConfig,
) -> (String, Vec<String>) {
    let mut state = begin_compaction(text, config);
    if config.output_compaction.group_search_output {
        apply_nullable_technique(&mut state, |t| group_search_results(arena, t, 50), "search");
    }
    apply_truncation(&mut state, config);
    (state.text.into_owned(), state.techniques)
}

/// 把源码过滤强度映射为过滤级别。
fn filter_level(level: SourceFilterLevel) -> FilterLevel {
    match level {
        SourceFilterLevel::None => FilterLevel::None,
        SourceFilterLevel::Minimal => FilterLevel::Minimal,
        SourceFilterLevel::Aggressive => FilterLevel::Aggressive,
    }
}

/// 源码过滤强度的线上名称（用于技术标签）。
fn filter_level_name(level: SourceFilterLevel) -> &'static str {
    match level {
        SourceFilterLevel::None => "none",
        SourceFilterLevel::Minimal => "minimal",
        SourceFilterLevel::Aggressive => "aggressive",
    }
}

/// 一条锚点行（字段借用输入或竞技场内容）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AnchorSafeReadLine<'a> {
    text: &'a str,
    content: &'a str,
}

/// 锚点行在整段输出中的位置。
struct AnchorSafeReadParts<'a> {
    prefix_lines: ArenaVec<'a, &'a str>,
    anchored_lines: ArenaVec<'a, AnchorSafeReadLine<'a>>,
    suffix_lines: ArenaVec<'a, &'a str>,
    trailing_newline: bool,
}

/// 拆分文本为行，并记录是否以换行结尾。
fn split_read_lines<'a>(arena: &'a Bump, text: &'a str) -> (ArenaVec<'a, &'a str>, bool) {
    if text.is_empty() {
        return (ArenaVec::new_in(arena), false);
    }
    let trailing_newline = text.ends_with('\n');
    let mut lines = split_lines(arena, text);
    if trailing_newline {
        lines.pop();
    }
    (lines, trailing_newline)
}

/// 按是否以换行结尾拼回文本。
fn join_read_lines<'a>(arena: &'a Bump, lines: &[&str], trailing_newline: bool) -> &'a str {
    let mut out = bumpalo::collections::String::new_in(arena);
    let mut first = true;
    for line in lines {
        if !first {
            out.push('\n');
        }
        first = false;
        out.push_str(line);
    }
    if trailing_newline && !out.is_empty() {
        out.push('\n');
    }
    out.into_bump_str()
}

/// 解析一条锚点行，返回 (内容, 原始行)。
fn parse_anchored_read_line(line: &str) -> Option<(&str, &str)> {
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
        return Some((captures.get(2).map_or("", |m| m.as_str()), line));
    }
    None
}

/// 判断整段输出是否像锚点行输出。
fn looks_like_anchored_read_output(arena: &Bump, text: &str) -> bool {
    let (lines, _) = split_read_lines(arena, text);
    let mut match_count = 0usize;
    let mut relevant_line_count = 0usize;
    let mut previous_matched: Option<u64> = None;
    let mut has_increasing_anchors = false;

    for line in lines.iter().take(ANCHORED_READ_LINE_SAMPLE_LIMIT).copied() {
        if !ANCHORED_READ_INFORMATIONAL_LINE.is_match(line) {
            relevant_line_count += 1;
        }
        let Some((_, original)) = parse_anchored_read_line(line) else {
            continue;
        };
        match_count += 1;
        let number = original
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
fn extract_anchored_read_parts<'a>(
    arena: &'a Bump,
    text: &'a str,
) -> Option<AnchorSafeReadParts<'a>> {
    if !looks_like_anchored_read_output(arena, text) {
        return None;
    }

    let (lines, trailing_newline) = split_read_lines(arena, text);
    let mut first_anchor = None;
    let mut last_anchor = None;
    for (index, line) in lines.iter().enumerate() {
        if parse_anchored_read_line(line).is_some() {
            first_anchor.get_or_insert(index);
            last_anchor = Some(index);
        }
    }
    let (first_anchor, last_anchor) = (first_anchor?, last_anchor?);

    let mut anchored_lines = ArenaVec::new_in(arena);
    for line in lines.iter().take(last_anchor + 1).skip(first_anchor) {
        let (content, original) = parse_anchored_read_line(line)?;
        anchored_lines.push(AnchorSafeReadLine {
            text: original,
            content,
        });
    }

    let mut prefix_lines = ArenaVec::new_in(arena);
    prefix_lines.extend(lines.iter().take(first_anchor).copied());
    let mut suffix_lines = ArenaVec::new_in(arena);
    suffix_lines.extend(lines.iter().skip(last_anchor + 1).copied());

    Some(AnchorSafeReadParts {
        prefix_lines,
        anchored_lines,
        suffix_lines,
        trailing_newline,
    })
}

/// 渲染锚点行正文。
fn render_anchor_safe_read_body<'a>(arena: &'a Bump, lines: &[AnchorSafeReadLine<'_>]) -> &'a str {
    let parts: Vec<&str> = lines.iter().map(|line| line.text).collect();
    join_read_lines(arena, &parts, false)
}

/// 渲染含前后缀的完整文本。
fn render_anchor_safe_read_text<'a>(
    arena: &'a Bump,
    parts: &AnchorSafeReadParts<'_>,
    lines: &[AnchorSafeReadLine<'_>],
) -> &'a str {
    let mut all: Vec<&str> = Vec::with_capacity(
        parts.prefix_lines.len() + lines.len() + parts.suffix_lines.len(),
    );
    all.extend(parts.prefix_lines.iter().copied());
    all.extend(lines.iter().map(|line| line.text));
    all.extend(parts.suffix_lines.iter().copied());
    join_read_lines(arena, &all, parts.trailing_newline)
}

/// 把压缩后的内容重新映射回原始锚点行（内容匹配不上时退化为新行）。
fn remap_transformed_content<'a>(
    arena: &'a Bump,
    source_lines: &[AnchorSafeReadLine<'a>],
    transformed: &'a str,
) -> ArenaVec<'a, AnchorSafeReadLine<'a>> {
    let (transformed_lines, _) = split_read_lines(arena, transformed);
    let mut remapped = ArenaVec::new_in(arena);
    let mut search_start = 0usize;

    for transformed_line in transformed_lines.iter().copied() {
        let matched = source_lines
            .iter()
            .enumerate()
            .skip(search_start)
            .find(|(_, line)| line.content == transformed_line);
        match matched {
            Some((index, line)) => {
                remapped.push(*line);
                search_start = index + 1;
            }
            None => remapped.push(AnchorSafeReadLine {
                text: transformed_line,
                content: transformed_line,
            }),
        }
    }

    remapped
}

/// 按字符预算截断锚点行（保证不切断锚点）。
fn truncate_anchor_safe_read_lines<'a>(
    arena: &'a Bump,
    lines: &[AnchorSafeReadLine<'a>],
    max_chars: usize,
) -> ArenaVec<'a, AnchorSafeReadLine<'a>> {
    let mut truncated = ArenaVec::new_in(arena);
    if render_anchor_safe_read_body(arena, lines).chars().count() <= max_chars {
        truncated.extend(lines.iter().copied());
        return truncated;
    }

    const MARKER: &str = "[RTK anchor-safe truncate: remaining anchored read lines omitted to preserve complete anchors]";
    let marker = AnchorSafeReadLine {
        text: MARKER,
        content: MARKER,
    };
    let mut char_count = 0usize;

    for (index, line) in lines.iter().enumerate() {
        let separator_length = if truncated.is_empty() { 0 } else { 1 };
        let next_char_count = char_count + separator_length + line.text.chars().count();
        let remaining_after = lines.len() - index - 1;
        let marker_length = if remaining_after > 0 {
            (if next_char_count > 0 { 1 } else { 0 }) + MARKER.chars().count()
        } else {
            0
        };

        if next_char_count + marker_length > max_chars {
            if truncated.is_empty() {
                let mut only = ArenaVec::new_in(arena);
                only.push(marker);
                return only;
            }
            truncated.push(marker);
            return truncated;
        }

        truncated.push(*line);
        char_count = next_char_count;
    }

    truncated
}

/// 锚点输出专用压缩。
fn compact_anchored_read_text<'a>(
    arena: &'a Bump,
    text: &'a str,
    file_path: &str,
    config: &RtkIntegrationConfig,
) -> (String, Vec<String>) {
    let Some(parts) = extract_anchored_read_parts(arena, text) else {
        return (text.to_string(), Vec::new());
    };

    let mut lines = parts.anchored_lines.clone();
    let mut techniques: Vec<String> = Vec::new();
    let compaction = &config.output_compaction;
    let language = detect_language(file_path);

    if compaction.source_code_filtering_enabled
        && compaction.source_code_filtering != SourceFilterLevel::None
        && should_apply_read_source_filtering(text, config)
    {
        let current: Vec<&str> = lines.iter().map(|line| line.content).collect();
        let current = join_read_lines(arena, &current, false);
        let filtered = filter_source_code(
            arena,
            current,
            language,
            filter_level(compaction.source_code_filtering),
        );
        let remapped = remap_transformed_content(arena, &lines, filtered);
        if render_anchor_safe_read_body(arena, &remapped) != render_anchor_safe_read_body(arena, &lines)
        {
            lines = remapped;
            techniques.push(format!(
                "source:{}",
                filter_level_name(compaction.source_code_filtering)
            ));
        }
    }

    if compaction.smart_truncate.enabled && lines.len() > compaction.smart_truncate.max_lines as usize
    {
        let current: Vec<&str> = lines.iter().map(|line| line.content).collect();
        let current = join_read_lines(arena, &current, false);
        let compacted = smart_truncate(
            arena,
            current,
            compaction.smart_truncate.max_lines as usize,
            language,
        );
        let remapped = remap_transformed_content(arena, &lines, compacted);
        if render_anchor_safe_read_body(arena, &remapped) != render_anchor_safe_read_body(arena, &lines)
        {
            lines = remapped;
            techniques.push("smart-truncate".to_string());
        }
    }

    if compaction.truncate.enabled
        && render_anchor_safe_read_text(arena, &parts, &lines)
            .chars()
            .count()
            > compaction.truncate.max_chars as usize
    {
        let overhead = render_anchor_safe_read_text(arena, &parts, &[]).chars().count();
        let body_max = (compaction.truncate.max_chars as usize)
            .saturating_sub(overhead)
            .max(1);
        let truncated = truncate_anchor_safe_read_lines(arena, &lines, body_max);
        if render_anchor_safe_read_body(arena, &truncated) != render_anchor_safe_read_body(arena, &lines)
        {
            lines = truncated;
            techniques.push("truncate".to_string());
        }
    }

    (
        render_anchor_safe_read_text(arena, &parts, &lines).to_string(),
        techniques,
    )
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
        normalize_path(path)
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        normalize_path(&cwd.join(path))
    }
}

/// 归一化 `.` / `..`（不做符号链接解析，与 pi 的 `resolve` 语义接近）。
fn normalize_path(path: &Path) -> PathBuf {
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
    let target = normalize_path(target);
    let root = normalize_path(root);
    target == root || target.starts_with(&root)
}

/// 任一祖先目录的 `.agents/skills` 之下。
fn is_under_any_ancestor_agents_skills(target: &Path, cwd: &Path) -> bool {
    let mut current = normalize_path(cwd);
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
    if compaction.preserve_exact_skill_reads && is_skill_read_path(read_path(input)) {
        return true;
    }
    count_lines(text) <= READ_EXACT_OUTPUT_LINE_THRESHOLD
}

/// 入参是否显式指定了 offset / limit。
fn has_explicit_read_range(input: &Value) -> bool {
    input.get("offset").is_some() || input.get("limit").is_some()
}

/// 取 read 入参里的 path。
fn read_path(input: &Value) -> &str {
    input.get("path").and_then(Value::as_str).unwrap_or("")
}

/// 取 bash 入参里的 command。
fn normalize_command(input: &Value) -> Option<&str> {
    input
        .get("command")
        .and_then(Value::as_str)
        .filter(|command| !command.trim().is_empty())
}

/// 在 read 压缩结果前加 banner。
fn apply_read_compaction_banner(state: &mut CompactionState<'_>) {
    if !state.techniques.is_empty() && !state.text.starts_with(READ_COMPACTION_BANNER_PREFIX) {
        state.text = Cow::Owned(format!(
            "{READ_COMPACTION_BANNER_PREFIX} {}]\n{}",
            state.techniques.join(", "),
            state.text
        ));
    }
}

/// read 输出压缩。
fn compact_read_text<'a>(
    arena: &'a Bump,
    text: &'a str,
    file_path: &str,
    config: &RtkIntegrationConfig,
    preserve_exact: bool,
) -> (String, Vec<String>) {
    if preserve_exact {
        return (text.to_string(), Vec::new());
    }

    let mut state = begin_compaction(text, config);
    let compaction = &config.output_compaction;

    if looks_like_anchored_read_output(arena, &state.text) {
        let (anchored_text, anchored_techniques) =
            compact_anchored_read_text(arena, text, file_path, config);
        state.text = Cow::Owned(anchored_text);
        state.techniques.extend(anchored_techniques);
        apply_read_compaction_banner(&mut state);
        return (state.text.into_owned(), state.techniques);
    }

    let language = detect_language(file_path);
    if compaction.source_code_filtering_enabled
        && compaction.source_code_filtering != SourceFilterLevel::None
        && should_apply_read_source_filtering(text, config)
    {
        let technique = format!(
            "source:{}",
            filter_level_name(compaction.source_code_filtering)
        );
        let level = filter_level(compaction.source_code_filtering);
        apply_nullable_technique(
            &mut state,
            |t| Some(filter_source_code(arena, t, language, level).to_string()),
            &technique,
        );
    }

    if compaction.smart_truncate.enabled
        && state.text.split('\n').count() > compaction.smart_truncate.max_lines as usize
    {
        let compacted = smart_truncate(
            arena,
            &state.text,
            compaction.smart_truncate.max_lines as usize,
            language,
        );
        if compacted != state.text.as_ref() {
            state.text = Cow::Owned(compacted.to_string());
            state.techniques.push("smart-truncate".to_string());
        }
    }

    apply_truncation(&mut state, config);
    apply_read_compaction_banner(&mut state);
    (state.text.into_owned(), state.techniques)
}

/// 压缩单个工具结果。
///
/// `content` 是 phi 协议里的整段字符串；`input` 是工具入参 JSON。
/// `arena` 提供本次调用的临时内存（由调用方复用同一个 `Scratch`）。
/// 未发生任何变化时 `changed == false`，调用方应保留原文。
pub fn compact_tool_result<'a>(
    arena: &'a Bump,
    tool_name: &str,
    input: &Value,
    content: &'a str,
    config: &RtkIntegrationConfig,
    metrics: Option<&mut OutputMetrics>,
) -> CompactionOutcome {
    if !config.output_compaction.enabled || content.is_empty() {
        return CompactionOutcome::unchanged(content);
    }

    // 命令只归一化一次，供 build/test/git/linter 四类技术复用。
    let normalized_command = match tool_name {
        "bash" => normalize_command_for_detection(normalize_command(input)),
        _ => None,
    };

    let (text, techniques) = match tool_name {
        "bash" => compact_bash_text(arena, content, normalized_command.as_deref(), config),
        "read" => {
            let path = read_path(input);
            let preserve = should_preserve_exact_read_output(content, input, config);
            compact_read_text(arena, content, path, config, preserve)
        }
        // ast-grep 的 search 输出与 grep 同为 `path:line[:col]:content`，复用同一分组技术。
        "grep" | "ast_grep_search" => compact_grep_text(arena, content, config),
        _ => return CompactionOutcome::unchanged(content),
    };

    if text == content || techniques.is_empty() {
        return CompactionOutcome::unchanged(content);
    }

    // 字符数只算一次，同时供统计与结果结构使用。
    let original_chars = content.chars().count();
    let compacted_chars = text.chars().count();
    if config.output_compaction.track_savings {
        if let Some(metrics) = metrics {
            metrics.track(original_chars, compacted_chars, tool_name, &techniques);
        }
    }
    let truncated = has_lossy_compaction(&techniques);
    CompactionOutcome {
        changed: true,
        compacted_lines: count_lines(&text),
        original_lines: count_lines(content),
        text,
        techniques,
        truncated,
        original_chars,
        compacted_chars,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RtkIntegrationConfig;
    use phi_ext_common::arena::Scratch;
    use phi_ext_common::json::json;

    fn config() -> RtkIntegrationConfig {
        RtkIntegrationConfig::default()
    }

    /// 用一次性竞技场跑压缩。
    fn compact(tool: &str, input: &Value, content: &str) -> CompactionOutcome {
        compact_with(tool, input, content, &config())
    }

    fn compact_with(
        tool: &str,
        input: &Value,
        content: &str,
        config: &RtkIntegrationConfig,
    ) -> CompactionOutcome {
        let scratch = Scratch::with_capacity(4096);
        compact_tool_result(scratch.arena(), tool, input, content, config, None)
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
        let outcome = compact("bash", &json!({ "command": "cargo build" }), content);
        assert!(outcome.changed);
        assert_eq!(outcome.text, "[OK] Build successful (2 units compiled)");
        assert_eq!(outcome.techniques, vec!["build".to_string()]);
        // `build` 属于有损技术（会丢弃进度行），与 pi 版判定一致。
        assert!(outcome.truncated);
    }

    #[test]
    fn bash_should_strip_ansi_before_techniques() {
        let content = "\x1b[31m   Compiling\x1b[0m demo v0.1.0";
        let outcome = compact("bash", &json!({ "command": "cargo build" }), content);
        assert!(outcome.changed);
        assert!(outcome.techniques.contains(&"ansi".to_string()));
        assert!(outcome.techniques.contains(&"build".to_string()));
        assert!(!outcome.text.contains('\x1b'));
    }

    #[test]
    fn bash_without_ansi_should_not_record_ansi_technique() {
        let content = "   Compiling demo v0.1.0";
        let outcome = compact("bash", &json!({ "command": "cargo build" }), content);
        assert!(!outcome.techniques.contains(&"ansi".to_string()));
    }

    #[test]
    fn unknown_tool_should_be_unchanged() {
        let outcome = compact("write", &json!({}), "hello");
        assert!(!outcome.changed);
        assert_eq!(outcome.text, "hello");
    }

    #[test]
    fn disabled_compaction_should_be_unchanged() {
        let mut config = config();
        config.output_compaction.enabled = false;
        let outcome = compact_with("bash", &json!({}), "\x1b[31mx\x1b[0m", &config);
        assert!(!outcome.changed);
    }

    #[test]
    fn grep_output_should_be_grouped() {
        let content = "src/a.rs:1:let x = 1;\nsrc/a.rs:2:let y = 2;";
        let outcome = compact("grep", &json!({ "pattern": "let" }), content);
        assert!(outcome.changed);
        assert_eq!(outcome.techniques, vec!["search".to_string()]);
        assert!(outcome.text.contains("2 matches in 1 files:"));
    }

    #[test]
    fn ast_grep_search_output_should_be_grouped() {
        let content = "src/a.rs:1:5:let x = 1;\nsrc/a.rs:2:5:let y = 2;";
        let outcome = compact("ast_grep_search", &json!({ "pattern": "let" }), content);
        assert!(outcome.changed);
        assert_eq!(outcome.techniques, vec!["search".to_string()]);
        assert!(outcome.text.contains("2 matches in 1 files:"));
    }

    #[test]
    fn bash_ast_grep_output_should_be_grouped() {
        let content = "src/a.rs:1:5:let x = 1;\nsrc/a.rs:2:5:let y = 2;";
        let outcome = compact("bash", &json!({ "command": "sg -p 'let x' -l rust src" }), content);
        assert!(outcome.changed);
        assert_eq!(outcome.techniques, vec!["search".to_string()]);
        assert!(outcome.text.contains("2 matches in 1 files:"));
    }

    #[test]
    fn short_read_should_be_preserved_exactly() {
        let content = "1#ab:let x = 1;\n2#cd:let y = 2;";
        let outcome = compact("read", &json!({ "path": "src/a.rs" }), content);
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
        let outcome = compact_with(
            "read",
            &json!({ "path": "src/a.rs", "limit": 200 }),
            &content,
            &config,
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
        let outcome = compact_with("read", &json!({ "path": "src/a.rs" }), &content, &config);
        assert!(outcome.changed);
        assert!(
            outcome.text.starts_with(READ_COMPACTION_BANNER_PREFIX),
            "got {}",
            outcome.text
        );
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
        let outcome = compact_with("read", &json!({ "path": "src/a.rs" }), &content, &config);
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
        let scratch = Scratch::with_capacity(1024);
        let content = "   Compiling a\n   Compiling b\n    Finished";
        let outcome = compact_tool_result(
            scratch.arena(),
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
        let scratch = Scratch::with_capacity(256);
        let arena = scratch.arena();
        assert!(looks_like_anchored_read_output(arena, "1#ab:a\n2#cd:b"));
        assert!(!looks_like_anchored_read_output(arena, "2#ab:a\n1#cd:b"));
        assert!(!looks_like_anchored_read_output(arena, "1#ab:a"));
    }

    #[test]
    fn remap_should_reuse_original_anchor_lines() {
        let scratch = Scratch::with_capacity(256);
        let arena = scratch.arena();
        let source = vec![
            AnchorSafeReadLine {
                text: "1#ab:let a = 1;",
                content: "let a = 1;",
            },
            AnchorSafeReadLine {
                text: "2#cd:let b = 2;",
                content: "let b = 2;",
            },
        ];
        let remapped = remap_transformed_content(arena, &source, "let a = 1;\nlet b = 2;");
        assert_eq!(remapped.as_slice(), source.as_slice());
        let dropped = remap_transformed_content(arena, &source, "let b = 2;");
        assert_eq!(dropped.as_slice(), &[source[1]]);
    }


    #[test]
    fn arena_should_be_reusable_across_tool_results() {
        let mut scratch = Scratch::with_capacity(2048);
        for _ in 0..3 {
            let outcome = compact_tool_result(
                scratch.arena(),
                "bash",
                &json!({ "command": "cargo build" }),
                "   Compiling x\n   Compiling y",
                &config(),
                None,
            );
            assert!(outcome.changed);
            scratch.finish();
        }
        assert_eq!(scratch.resets(), 3);
    }
}