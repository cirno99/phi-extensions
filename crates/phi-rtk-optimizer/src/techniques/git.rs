// git.rs — git 输出压缩：diff / status / log 三种摘要。
//
// 由 pi 版 pi-rtk-optimizer 的 src/techniques/git.ts 移植。

use std::sync::LazyLock;

use regex::Regex;

use super::command_detection::{matches_command_patterns, normalize_command_for_detection};

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
static DIFF_HEADER_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"diff --git a/(.+) b/(.+)").expect("diff 头正则应可编译")
});

/// `@@ ... @@` 段头。
static HUNK_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"@@ .+ @@").expect("段头正则应可编译"));

/// `## branch...remote` 行。
static BRANCH_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"## (.+)").expect("分支正则应可编译"));

/// 命令是否属于 git 类。
pub fn is_git_command(command: Option<&str>) -> bool {
    matches_command_patterns(command, &GIT_COMMAND_PATTERNS)
}

/// 压缩 diff：按文件汇总增删行数，每个 hunk 最多保留 10 行。
pub fn compact_diff(output: &str, max_lines: usize) -> String {
    let mut result: Vec<String> = Vec::new();
    let mut current_file = String::new();
    let mut added = 0usize;
    let mut removed = 0usize;
    let mut in_hunk = false;
    let mut hunk_lines = 0usize;
    let max_hunk_lines = 10usize;

    for line in output.split('\n') {
        if result.len() >= max_lines {
            result.push("\n... (more changes truncated)".to_string());
            break;
        }

        if line.starts_with("diff --git") {
            if !current_file.is_empty() && (added > 0 || removed > 0) {
                result.push(format!("  +{added} -{removed}"));
            }
            current_file = DIFF_HEADER_PATTERN
                .captures(line)
                .and_then(|caps| caps.get(2))
                .map_or_else(|| "unknown".to_string(), |m| m.as_str().to_string());
            result.push(format!("\n> {current_file}"));
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
            result.push(format!("  {hunk_info}"));
            continue;
        }

        if !in_hunk {
            continue;
        }

        if line.starts_with('+') && !line.starts_with("+++") {
            added += 1;
            if hunk_lines < max_hunk_lines {
                result.push(format!("  {line}"));
                hunk_lines += 1;
            }
        } else if line.starts_with('-') && !line.starts_with("---") {
            removed += 1;
            if hunk_lines < max_hunk_lines {
                result.push(format!("  {line}"));
                hunk_lines += 1;
            }
        } else if hunk_lines > 0 && hunk_lines < max_hunk_lines && !line.starts_with('\\') {
            result.push(format!("  {line}"));
            hunk_lines += 1;
        }

        if hunk_lines == max_hunk_lines {
            result.push("  ... (truncated)".to_string());
            hunk_lines += 1;
        }
    }

    if !current_file.is_empty() && (added > 0 || removed > 0) {
        result.push(format!("  +{added} -{removed}"));
    }

    result.join("\n")
}

/// 压缩 `git status`：按暂存/修改/未跟踪/冲突分组计数。
pub fn compact_status(output: &str) -> String {
    let lines: Vec<&str> = output.split('\n').collect();

    if lines.is_empty() || (lines.len() == 1 && lines[0].trim().is_empty()) {
        return "Clean working tree".to_string();
    }

    let mut staged = 0usize;
    let mut modified = 0usize;
    let mut untracked = 0usize;
    let mut conflicts = 0usize;
    let mut staged_files: Vec<String> = Vec::new();
    let mut modified_files: Vec<String> = Vec::new();
    let mut untracked_files: Vec<String> = Vec::new();
    let mut branch_name = String::new();

    for line in &lines {
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

        let status: String = line.chars().take(2).collect();
        let filename: String = line.chars().skip(3).collect();
        let mut status_chars = status.chars();
        let index_status = status_chars.next().unwrap_or(' ');
        let worktree_status = status_chars.next().unwrap_or(' ');

        if matches!(index_status, 'M' | 'A' | 'D' | 'R' | 'C') {
            staged += 1;
            staged_files.push(filename.clone());
        }
        if index_status == 'U' {
            conflicts += 1;
        }
        if matches!(worktree_status, 'M' | 'D') {
            modified += 1;
            modified_files.push(filename.clone());
        }
        if status == "??" {
            untracked += 1;
            untracked_files.push(filename);
        }
    }

    let mut result = format!("Branch: {branch_name}\n");

    if staged > 0 {
        result.push_str(&format!("Staged: {staged} files\n"));
        for file in staged_files.iter().take(5) {
            result.push_str(&format!("  {file}\n"));
        }
        if staged > 5 {
            result.push_str(&format!("  ... +{} more\n", staged - 5));
        }
    }
    if modified > 0 {
        result.push_str(&format!("Modified: {modified} files\n"));
        for file in modified_files.iter().take(5) {
            result.push_str(&format!("  {file}\n"));
        }
        if modified > 5 {
            result.push_str(&format!("  ... +{} more\n", modified - 5));
        }
    }
    if untracked > 0 {
        result.push_str(&format!("Untracked: {untracked} files\n"));
        for file in untracked_files.iter().take(3) {
            result.push_str(&format!("  {file}\n"));
        }
        if untracked > 3 {
            result.push_str(&format!("  ... +{} more\n", untracked - 3));
        }
    }
    if conflicts > 0 {
        result.push_str(&format!("Conflicts: {conflicts} files\n"));
    }

    result.trim_end().to_string()
}

/// 压缩 `git log`：保留前 `limit` 行，过长行截断到 80 字符。
pub fn compact_log(output: &str, limit: usize) -> String {
    let lines: Vec<&str> = output.split('\n').collect();
    let mut result: Vec<String> = Vec::new();

    for line in lines.iter().take(limit) {
        if line.chars().count() > 80 {
            let kept: String = line.chars().take(77).collect();
            result.push(format!("{kept}..."));
        } else {
            result.push((*line).to_string());
        }
    }

    if lines.len() > limit {
        result.push(format!("... and {} more commits", lines.len() - limit));
    }

    result.join("\n")
}

/// 按子命令分派压缩；非 git 命令或无法识别时返回 `None`。
pub fn compact_git_output(output: &str, command: Option<&str>) -> Option<String> {
    if !is_git_command(command) {
        return None;
    }

    let normalized = normalize_command_for_detection(command)?;

    if normalized.starts_with("git diff") {
        return RAW_GIT_DIFF_PATTERN
            .is_match(output)
            .then(|| compact_diff(output, 50));
    }
    if normalized.starts_with("git status") {
        return RAW_GIT_STATUS_PATTERN
            .is_match(output)
            .then(|| compact_status(output));
    }
    if normalized.starts_with("git log") {
        return Some(compact_log(output, 20));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_git_command_should_match_common_subcommands() {
        assert!(is_git_command(Some("git status --short")));
        assert!(is_git_command(Some("git diff HEAD")));
        assert!(is_git_command(Some("git log --oneline")));
        assert!(!is_git_command(Some("git commit -m x")));
        assert!(!is_git_command(None));
    }

    #[test]
    fn status_should_group_by_state() {
        let output = "## main...origin/main\nM  staged.rs\n M modified.rs\n?? new.txt\nUU conflict.rs";
        let result = compact_status(output);
        assert!(result.contains("Branch: main"), "got {result}");
        assert!(result.contains("Staged: 1 files"), "got {result}");
        assert!(result.contains("Modified: 1 files"), "got {result}");
        assert!(result.contains("Untracked: 1 files"), "got {result}");
        assert!(result.contains("Conflicts: 1 files"), "got {result}");
    }

    #[test]
    fn empty_status_should_report_clean_tree() {
        assert_eq!(compact_status(""), "Clean working tree");
        // 只有一个换行时 split 出两行，与 pi 版一致地落到「有分支名但无变更」分支。
        assert_eq!(compact_status("\n"), "Branch:");
    }

    #[test]
    fn diff_should_summarise_files_and_hunks() {
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
        let result = compact_diff(output, 50);
        assert!(result.contains("> src/a.rs"), "got {result}");
        assert!(result.contains("@@ -1,3 +1,4 @@"), "got {result}");
        assert!(result.contains("+2 -1"), "got {result}");
    }

    #[test]
    fn diff_should_respect_max_lines() {
        let mut output = String::from("diff --git a/x b/x\n@@ -1 +1 @@\n");
        for index in 0..80 {
            output.push_str(&format!("+line {index}\n"));
        }
        let result = compact_diff(&output, 20);
        assert!(result.lines().count() <= 22, "got {}", result.lines().count());
    }

    #[test]
    fn log_should_truncate_and_count_remainder() {
        let output = (0..25)
            .map(|index| format!("commit {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = compact_log(&output, 20);
        assert!(result.contains("... and 5 more commits"), "got {result}");
    }

    #[test]
    fn non_git_command_should_return_none() {
        assert!(compact_git_output("x", Some("ls")).is_none());
        // git stash 无法识别为三种摘要之一
        assert!(compact_git_output("x", Some("git stash list")).is_none());
    }
}