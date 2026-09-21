//! 轻量英文词干还原 —— 对应 acp-kernel `src/search/stemmer.ts`。
//!
//! 后缀剥离（Porter 风格，刻意更简单更快，零依赖），用于 IR 形态归一：
//! `tokens → token`、`running → runn`、`compressed → compress`、
//! `authentication → authenticat`、`subagents → subagent`。
//!
//! 上游注释把最后一个例子写成 `authentication → authentic`，那是笔误：`ation`
//! 分支先命中并砍掉末尾 3 字符，实际是 `authenticat`（已用上游 TS 源码在 node
//! 上逐字复算确认）。这里按**代码**而非注释移植。
//!
//! 不是完整 Porter，CJK 不经过这里（由 bigram 分词处理，不做词干）。

/// 对单个词做后缀剥离。
///
/// 只对 ASCII 词有意义（调用方 [`crate::search::tokenizer::tokenize`] 只会把
/// `latin_words` 产出的 ASCII 词喂进来）；非 ASCII 原样返回，避免按字节截断
/// 切坏 UTF-8 边界。
pub fn stem(word: &str) -> String {
    if !word.is_ascii() {
        return word.to_string();
    }
    let mut w = word.to_string();
    // 上游用 UTF-16 `.length`；ASCII 词下与字节长度一致。
    if w.len() <= 3 {
        return w;
    }
    if w.ends_with("ies") {
        w.truncate(w.len() - 3);
        w.push('y');
    } else if w.ends_with("ses")
        || w.ends_with("xes")
        || w.ends_with("zes")
        || w.ends_with("ches")
        || w.ends_with("shes")
    {
        // 上游把这两组写成两个分支（纯可读性），剥离长度完全相同，这里合并
        // 以免 `clippy::if_same_then_else` 告警。
        w.truncate(w.len() - 2);
    } else if w.ends_with('s') && !w.ends_with("ss") {
        w.truncate(w.len() - 1);
    }
    if w.ends_with("ing") && w.len() > 5 {
        w.truncate(w.len() - 3);
    }
    if w.ends_with("ed") && w.len() > 4 {
        w.truncate(w.len() - 2);
    }
    if w.ends_with("ation") && w.len() > 6 {
        w.truncate(w.len() - 3);
    } else if w.ends_with("tion") && w.len() > 5 {
        w.truncate(w.len() - 4);
        w.push('t');
    } else if w.ends_with("ion") && w.len() > 4 {
        w.truncate(w.len() - 3);
    }
    if w.ends_with("ment") && w.len() > 6 {
        w.truncate(w.len() - 4);
    }
    if w.ends_with("ness") && w.len() > 6 {
        w.truncate(w.len() - 4);
    }
    if w.ends_with("ly") && w.len() > 4 {
        w.truncate(w.len() - 2);
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stem_should_match_upstream_examples() {
        // 上游文档里逐字列出的例子。注意 `authentication → authentic` 是上游
        // 注释的**笔误**：`ation` 分支先命中并砍掉末尾 3 字符，实际得到
        // `authenticat`（已用上游 TS 源码在 node 上逐字复算确认）。
        assert_eq!(stem("tokens"), "token");
        assert_eq!(stem("running"), "runn");
        assert_eq!(stem("compressed"), "compress");
        assert_eq!(stem("authentication"), "authenticat");
        assert_eq!(stem("handling"), "handl");
        assert_eq!(stem("subagents"), "subagent");
    }

    #[test]
    fn stem_should_collapse_morphology() {
        // 同一词根的多种形态必须收敛到同一 token（BM25 精度来源）。
        assert_eq!(stem("compress"), stem("compressed"));
        assert_eq!(stem("compress"), stem("compression"));
        assert_eq!(stem("compression"), "compress");
    }

    #[test]
    fn stem_should_strip_sibilant_plurals() {
        // `ses` / `xes` / `zes` / `ches` / `shes` 分支（上游拆成两个，剥离长度相同）。
        assert_eq!(stem("boxes"), "box");
        assert_eq!(stem("churches"), "church");
        assert_eq!(stem("dishes"), "dish");
        assert_eq!(stem("quizzes"), "quizz");
        // `ss` 结尾不能剥（"class" 不是复数）。
        assert_eq!(stem("class"), "class");
        // `ies` → `y`。
        assert_eq!(stem("policies"), "policy");
    }

    #[test]
    fn stem_should_leave_short_and_non_ascii_alone() {
        assert_eq!(stem("to"), "to");
        assert_eq!(stem("of"), "of");
        assert_eq!(stem("ss"), "ss");
        // 非 ASCII 原样返回（不做词干，也不按字节截断）。
        assert_eq!(stem("压缩"), "压缩");
        assert_eq!(stem("café"), "café");
    }
}
