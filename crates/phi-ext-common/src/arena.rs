//! bumpalo 竞技场：为单次工具调用 / 钩子事件提供临时内存。
//!
//! 设计取舍：扩展是长驻进程，一次 `bash` 工具输出可能有几百 KB，压缩流程需要
//! 「剥 ANSI → 切行 → 分类 → 截断」多趟扫描。每趟都用 `String` / `Vec` 会产生
//! 大量短命堆分配。
//!
//! [`Scratch`] 复用一个 [`Bump`]：一次调用结束时 [`Scratch::finish`] 只把分配
//! 指针拨回起点，底层 chunk 全部保留，后续调用几乎不再向全局分配器申请内存。
//!
//! **生命周期约束**：竞技场里的数据在 `finish()` 后立即失效，因此只用于调用
//! 内部的中间量；最终交给宿主的字符串仍以普通 `String` 返回。

use bumpalo::collections::{String as ArenaString, Vec as ArenaVec};
use bumpalo::Bump;

/// 可复用的竞技场句柄。
#[derive(Debug)]
pub struct Scratch {
    arena: Bump,
    resets: u64,
    peak_reserved: usize,
}

impl Scratch {
    /// 创建空竞技场（首次分配时按需扩块）。
    pub fn new() -> Self {
        Self::with_capacity(0)
    }

    /// 创建预置容量的竞技场，适合已知单次处理量级的场景。
    pub fn with_capacity(bytes: usize) -> Self {
        Self {
            arena: Bump::with_capacity(bytes),
            resets: 0,
            peak_reserved: 0,
        }
    }

    /// 底层竞技场，传给 [`lines`] / [`strip_ansi`] / [`join`] 使用。
    pub fn arena(&self) -> &Bump {
        &self.arena
    }

    /// 底层 chunk 当前占用的字节数（含为复用保留的空闲块）。
    ///
    /// 该值在 [`Scratch::finish`] 之后不会归零，因为 chunk 被刻意保留复用。
    pub fn reserved_bytes(&self) -> usize {
        self.arena.allocated_bytes()
    }

    /// 历史峰值 chunk 占用，用于判断预置容量是否合理。
    pub fn peak_reserved_bytes(&self) -> usize {
        self.peak_reserved
    }

    /// 已完成（被复位）的调用次数。
    pub fn resets(&self) -> u64 {
        self.resets
    }

    /// 结束一次调用：释放全部临时分配，保留底层 chunk 供下次复用。
    pub fn finish(&mut self) {
        self.peak_reserved = self.peak_reserved.max(self.arena.allocated_bytes());
        self.arena.reset();
        self.resets += 1;
    }
}

impl Default for Scratch {
    fn default() -> Self {
        Self::new()
    }
}

/// 在竞技场内按行切分，零拷贝借用输入。
///
/// 行索引本身不占堆内存（元素是 `&str`），整个 `Vec` 也只从竞技场取一次内存。
pub fn lines<'a>(arena: &'a Bump, input: &'a str) -> ArenaVec<'a, &'a str> {
    let mut out = ArenaVec::with_capacity_in(estimate_lines(input), arena);
    out.extend(input.lines());
    out
}

/// 在竞技场内按 `\n` 切分，语义与 `str::split('\n')` 完全一致
/// （保留末尾空行，且不处理 `\r`）。
///
/// 与 [`lines`] 的区别：`lines` 走 `str::lines`（丢弃末尾空行、去掉 `\r`），
/// 适合「按行处理」；本函数适合需要与既有 `split('\n')` 行为逐字对齐的迁移。
pub fn split_lines<'a>(arena: &'a Bump, input: &'a str) -> ArenaVec<'a, &'a str> {
    let mut out = ArenaVec::with_capacity_in(estimate_lines(input), arena);
    out.extend(input.split('\n'));
    out
}

/// 在竞技场内剥离 ANSI 转义序列。
pub fn strip_ansi<'a>(arena: &'a Bump, input: &'a str) -> ArenaString<'a> {
    let mut out = ArenaString::with_capacity_in(input.len(), arena);
    crate::ansi::strip_ansi_into(input, &mut out);
    out
}

/// 用分隔符把若干片段连接成一个竞技场字符串。
pub fn join<'a>(arena: &'a Bump, parts: &[&str], sep: &str) -> ArenaString<'a> {
    let total: usize = parts.iter().map(|p| p.len()).sum::<usize>()
        + sep.len() * parts.len().saturating_sub(1);
    let mut out = ArenaString::with_capacity_in(total, arena);
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            out.push_str(sep);
        }
        out.push_str(part);
    }
    out
}

/// 粗略估计行数，避免 `Vec` 反复扩容。
fn estimate_lines(input: &str) -> usize {
    // 平均每行 48 字节，向上取整，至少给 8 个槽位。
    (input.len() / 48 + 1).max(8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finish_keeps_chunk_for_reuse() {
        let mut scratch = Scratch::with_capacity(4096);
        let payload = "x".repeat(20) + "\n";
        let bulk = payload.repeat(500);
        let _ = lines(scratch.arena(), &bulk);
        let after_first = scratch.reserved_bytes();
        assert!(after_first > 0);

        scratch.finish();
        assert_eq!(scratch.resets(), 1);
        assert!(scratch.peak_reserved_bytes() >= after_first);

        // 复位后底层 chunk 保留，同量级再分配不应触发新的块增长。
        let _ = lines(scratch.arena(), &bulk);
        assert!(scratch.reserved_bytes() <= after_first);
        scratch.finish();
        assert_eq!(scratch.resets(), 2);
    }

    #[test]
    fn split_lines_keeps_trailing_empty_like_str_split() {
        let scratch = Scratch::with_capacity(64);
        assert_eq!(split_lines(scratch.arena(), "a\nb\n").as_slice(), &["a", "b", ""]);
        assert_eq!(split_lines(scratch.arena(), "").as_slice(), &[""]);
    }

    #[test]
    fn lines_splits_without_trailing_empty() {
        let scratch = Scratch::with_capacity(64);
        let got = lines(scratch.arena(), "a\nb\n");
        assert_eq!(got.as_slice(), &["a", "b"]);
    }

    #[test]
    fn strip_ansi_removes_escape_sequences() {
        let scratch = Scratch::with_capacity(64);
        let got = strip_ansi(scratch.arena(), "\u{1b}[31mred\u{1b}[0m");
        assert_eq!(got.as_str(), "red");
    }

    #[test]
    fn join_inserts_separator_between_parts_only() {
        let scratch = Scratch::with_capacity(64);
        let got = join(scratch.arena(), &["a", "b", "c"], " -> ");
        assert_eq!(got.as_str(), "a -> b -> c");
    }

    #[test]
    fn join_with_empty_parts_has_no_separator() {
        let scratch = Scratch::with_capacity(16);
        let got = join(scratch.arena(), &[], " -> ");
        assert_eq!(got.as_str(), "");
    }
}
