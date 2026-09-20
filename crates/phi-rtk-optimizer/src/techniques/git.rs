// git.rs — git 输出压缩：diff / status / log 三种摘要。
//
// 由 pi 版 pi-rtk-optimizer 的 src/techniques/git.ts 移植。
//
// 与 pi 版的差异（性能）：结果不再收集成 `Vec<String>` 再 `join`，
// 而是直接写进竞技场缓冲（[`PieceBuf`] 复刻 `join("\n")` 语义），
// 只在最后向全局分配器要一个 `String`。

use std::fmt::Write;
use std::sync::LazyLock;

use bumpalo::collections::String as ArenaString;
use bumpalo::Bump;
use regex::Regex;

use phi_ext_common::arena::split_lines;

use super::command_detection::matches_normalized_patterns;

/// git 类命令。
static GIT_COMMAND_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [r"^git\s+(diff|status|log|show|stash)\b"]
        .iter()
        .map(|p| Regex::new(p).expect("git 命令正则应可编译"))
        .collect()
});

/// 原始 diff 判定。
static RAW_GIT_DIFF_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^diff --git ").expect("diff 正则应可编译"));

/// 原始 status 判定。
static RAW_GIT_STATUS_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^(?:## |(?:M|A|D|R|C|U|\?| )\S)").expect("status 正则应可编译")
});

/// `diff --git a/x b/y` 的 y 部分。
static DIFF_HEADER_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"diff --git a/(.+) b/(.+)").expect("diff 头正则应可编译"));

/// `@@ ... @@` 段头。
static HUNK_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"@@ .+ @@").expect("段头正则应可编译"));

/// `## branch...remote` 行。
static BRANCH_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"## (.+)").expect("分支正则应可编译"));

/// 复刻 `pieces.join("\n")` 语义的竞技场缓冲。
struct PieceBuf<'a> {
    out: ArenaString<'a>,
    first: bool,
    pieces: usize,
}

impl<'a> PieceBuf<'a> {
    fn new(arena: &'a Bump) -> Self {
        Self {
            out: ArenaString::new_in(arena),
            first: true,
            pieces: 0,
        }
    }

    /// 追加一段（等价于 `push` 到 Vec，最后 `join("\n")`）。
    fn push(&mut self, piece: &str) {
        if !self.first {
            self.out.push('\n');
        }
        self.first = false;
        self.pieces += 1;
        self.out.push_str(piece);
    }

    /// 已追加的片段数（对应 pi 里 `result.length`）。
    fn pieces(&self) -> usize {
        self.pieces
    }

    fn finish(self) -> String {
        self.out.into_bump_str().to_string()
    }
}

/// 命令是否属于 git 类。
///
/// `normalized_command` 必须是 [`normalize_command_for_detection`] 的结果。
pub fn is_git_command(normalized_command: Option<&str>) -> bool {
    matches_normalized_patterns(normalized_command, &GIT_COMMAND_PATTERNS)
}

/// 压缩 diff：按文件汇总增删行数，每个 hunk 最多保留 10 行。
pub fn compact_diff(arena: &Bump, output: &str, max_lines: usize) -> String {
    let mut buf = PieceBuf::new(arena);
    let lines = split_lines(arena, output);

    let mut current_file = String::new();
    let mut added = 0usize;
    let mut removed = 0usize;
    let mut in_hunk = false;
    let mut hunk_lines = 0usize;
    let max_hunk_lines = 10usize;

    for line in lines.iter().copied() {
        if buf.pieces() >= max_lines {
            buf.push("\n... (more changes truncated)");
            break;
        }

        if line.starts_with("diff --git") {
            if !current_file.is_empty() && (added > 0 || removed > 0) {
                buf.push(&format!("  +{added} -{removed}"));
            }
            current_file = DIFF_HEADER_PATTERN
                .captures(line)
                .and_then(|caps| caps.get(2))
                .map_or_else(|| "unknown".to_string(), |m| m.as_str().to_string());
            buf.push(&format!("\n> {current_file}"));
            added = 0;
            removed = 0;
            in_hunk = false;
            continue;
        }

        if line.starts_with("@@") {
            in_hunk = true;
            hunk_lines = 0;
            let hunk_info = HUNK_PATTERN
                .find(line)
                .map_or_else(|| "@@".to_string(), |m| m.as_str().to_string());
            buf.push(&format!("  {hunk_info}"));
            continue;
        }

        if !in_hunk {
            continue;
        }

        if line.starts_with('+') && !line.starts_with("+++") {
            added += 1;
            if hunk_lines < max_hunk_lines {
                buf.push(&format!("  {line}"));
                hunk_lines += 1;
            }
        } else if line.starts_with('-') && !line.starts_with("---") {
            removed += 1;
            if hunk_lines < max_hunk_lines {
                buf.push(&format!("  {line}"));
                hunk_lines += 1;
            }
        } else if hunk_lines > 0 && hunk_lines < max_hunk_lines && !line.starts_with('\\') {
            buf.push(&format!("  {line}"));
            hunk_lines += 1;
        }

        if hunk_lines == max_hunk_lines {
            buf.push("  ... (truncated)");
            hunk_lines += 1;
        }
    }

    if !current_file.is_empty() && (added > 0 || removed > 0) {
        buf.push(&format!("  +{added} -{removed}"));
    }

    buf.finish()
}

/// 压缩 `git status`：按暂存/修改/未跟踪/冲突分组计数。
pub fn compact_status(arena: &Bump, output: &str) -> String {
    let lines = split_lines(arena, output);

    if lines.is_empty() || (lines.len() == 1 && lines[0].trim().is_empty()) {
        return "Clean working tree".to_string();
    }

    let mut staged = 0usize;
    let mut modified = 0usize;
    let mut untracked = 0usize;
    let mut conflicts = 0usize;
    let mut staged_files: Vec<&str> = Vec::new();
    let mut modified_files: Vec<&str> = Vec::new();
    let mut untracked_files: Vec<&str> = Vec::new();
    let mut branch_name = String::new();

    for line in lines.iter().copied() {
        if line.starts_with("##") {
            if let Some(captures) = BRANCH_PATTERN.captures(line) {
                if let Some(name) = captures.get(1) {
                    let name = name.as_str();
                    branch_name = name.split("...").next().unwrap_or(name).to_string();
                }
            }
            continue;
        }

        if line.chars().count() < 3 {
            continue;
        }

        let status: Vec<char> = line.chars().take(2).collect();
        let filename: &str = &line[line
            .char_indices()
            .nth(3)
            .map_or(line.len(), |(index, _)| index)..];
        let index_status = status[0];
        let worktree_status = status[1];

        if matches!(index_status, 'M' | 'A' | 'D' | 'R' | 'C') {
            staged += 1;
            staged_files.push(filename);
        }
        if index_status == 'U' {
            conflicts += 1;
        }
        if matches!(worktree_status, 'M' | 'D') {
            modified += 1;
            modified_files.push(filename);
        }
        if status[0] == '?' && status[1] == '?' {
            untracked += 1;
            untracked_files.push(filename);
        }
    }

    let mut out = ArenaString::new_in(arena);
    let _ = writeln!(out, "Branch: {branch_name}");

    if staged > 0 {
        let _ = writeln!(out, "Staged: {staged} files");
        for file in staged_files.iter().take(5) {
            let _ = writeln!(out, "  {file}");
        }
        if staged > 5 {
            let _ = writeln!(out, "  ... +{} more", staged - 5);
        }
    }
    if modified > 0 {
        let _ = writeln!(out, "Modified: {modified} files");
        for file in modified_files.iter().take(5) {
            let _ = writeln!(out, "  {file}");
        }
        if modified > 5 {
            let _ = writeln!(out, "  ... +{} more", modified - 5);
        }
    }
    if untracked > 0 {
        let _ = writeln!(out, "Untracked: {untracked} files");
        for file in untracked_files.iter().take(3) {
            let _ = writeln!(out, "  {file}");
        }
        if untracked > 3 {
            let _ = writeln!(out, "  ... +{} more", untracked - 3);
        }
    }
    if conflicts > 0 {
        let _ = writeln!(out, "Conflicts: {conflicts} files");
    }

    // pi 版最后做 `.trim()`；这里把裁剪后的内容复制进竞技场，避免额外全局分配。
    let mut trimmed = ArenaString::new_in(arena);
    trimmed.push_str(out.as_str().trim());
    trimmed.into_bump_str().to_string()
}

/// 压缩 `git log`：保留前 `limit` 行，过长行截断到 80 字符。
pub fn compact_log(arena: &Bump, output: &str, limit: usize) -> String {
    let lines = split_lines(arena, output);
    let mut buf = PieceBuf::new(arena);

    for line in lines.iter().take(limit).copied() {
        if line.chars().count() > 80 {
            let end = line
                .char_indices()
                .nth(77)
                .map_or(line.len(), |(index, _)| index);
            let mut piece = ArenaString::new_in(arena);
            piece.push_str(&line[..end]);
            piece.push_str("...");
            buf.push(&piece);
        } else {
            buf.push(line);
        }
    }

    if lines.len() > limit {
        buf.push(&format!("... and {} more commits", lines.len() - limit));
    }

    buf.finish()
}

/// 按子命令分派压缩；非 git 命令或无法识别时返回 `None`。
pub fn compact_git_output(
    arena: &Bump,
    output: &str,
    normalized_command: Option<&str>,
) -> Option<String> {
    if !is_git_command(normalized_command) {
        return None;
    }

    let normalized = normalized_command?;

    if normalized.starts_with("git diff") {
        return RAW_GIT_DIFF_PATTERN
            .is_match(output)
            .then(|| compact_diff(arena, output, 50));
    }
    if normalized.starts_with("git status") {
        return RAW_GIT_STATUS_PATTERN
            .is_match(output)
            .then(|| compact_status(arena, output));
    }
    if normalized.starts_with("git log") {
        return Some(compact_log(arena, output, 20));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::techniques::command_detection::normalize_command_for_detection;
    use phi_ext_common::arena::Scratch;

    fn normalized(command: &str) -> Option<String> {
        normalize_command_for_detection(Some(command))
    }

    #[test]
    fn is_git_command_should_match_common_subcommands() {
        assert!(is_git_command(normalized("git status --short").as_deref()));
        assert!(is_git_command(normalized("git diff HEAD").as_deref()));
        assert!(is_git_command(normalized("git log --oneline").as_deref()));
        assert!(!is_git_command(normalized("git commit -m x").as_deref()));
        assert!(!is_git_command(None));
    }

    #[test]
    fn status_should_group_by_state() {
        let scratch = Scratch::with_capacity(512);
        let output = "## main...origin/main\nM  staged.rs\n M modified.rs\n?? new.txt\nUU conflict.rs";
        let result = compact_status(scratch.arena(), output);
        assert!(result.contains("Branch: main"), "got {result}");
        assert!(result.contains("Staged: 1 files"), "got {result}");
        assert!(result.contains("Modified: 1 files"), "got {result}");
        assert!(result.contains("Untracked: 1 files"), "got {result}");
        assert!(result.contains("Conflicts: 1 files"), "got {result}");
    }

    #[test]
    fn empty_status_should_report_clean_tree() {
        let scratch = Scratch::with_capacity(64);
        assert_eq!(compact_status(scratch.arena(), ""), "Clean working tree");
        // 只有一个换行时 split 出两行，与 pi 版一致地落到「有分支名但无变更」分支。
        assert_eq!(compact_status(scratch.arena(), "\n"), "Branch:");
    }

    #[test]
    fn diff_should_summarise_files_and_hunks() {
        let scratch = Scratch::with_capacity(512);
        let output = "\
diff --git a/src/a.rs b/src/a.rs\n\
index 111..222 100644\n\
--- a/src/a.rs\n\
+++ b/src/a.rs\n\
@@ -1,3 +1,4 @@\n\
 unchanged\n\
-removed\n\
+added\n\
+added2";
        let result = compact_diff(scratch.arena(), output, 50);
        assert!(result.contains("> src/a.rs"), "got {result}");
        assert!(result.contains("@@ -1,3 +1,4 @@"), "got {result}");
        assert!(result.contains("+2 -1"), "got {result}");
    }

    #[test]
    fn diff_should_respect_max_lines() {
        let scratch = Scratch::with_capacity(2048);
        let mut output = String::from("diff --git a/x b/x\n@@ -1 +1 @@\n");
        for index in 0..80 {
            output.push_str(&format!("+line {index}\n"));
        }
        let result = compact_diff(scratch.arena(), &output, 20);
        assert!(result.lines().count() <= 22, "got {}", result.lines().count());
    }

    #[test]
    fn log_should_truncate_and_count_remainder() {
        let scratch = Scratch::with_capacity(1024);
        let output = (0..25)
            .map(|index| format!("commit {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = compact_log(scratch.arena(), &output, 20);
        assert!(result.contains("... and 5 more commits"), "got {result}");
    }

    #[test]
    fn non_git_command_should_return_none() {
        let scratch = Scratch::with_capacity(64);
        assert!(compact_git_output(scratch.arena(), "x", normalized("ls").as_deref()).is_none());
        // git stash 无法识别为三种摘要之一
        assert!(
            compact_git_output(scratch.arena(), "x", normalized("git stash list").as_deref())
                .is_none()
        );
    }
}