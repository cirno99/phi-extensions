//! ref 分配与解析 —— 对应 acp-kernel `src/refs.ts`。
//!
//! ref 是 `mNNNNN`（5 位补零，1..=99999）形式的会话内稳定锚点。模型在
//! `compress` 调用里引用它们，因此分配规则必须与 TS 版逐字一致：
//! 首次渲染时分配、此后永不重分配。

use std::collections::BTreeMap;

use crate::types::{CoreMessage, MessageRefMap};

/// ref 数字宽度。
const REF_WIDTH: usize = 5;
/// 最小 ref 序号。
pub const MIN_INDEX: u32 = 1;
/// 最大 ref 序号。
pub const MAX_INDEX: u32 = 99_999;
/// 受保护消息占位 ref。
pub const BLOCKED_REF: &str = "BLOCKED";

/// 空 ref 映射。
pub fn empty_ref_map() -> MessageRefMap {
    MessageRefMap::default()
}

/// 序号 → ref 文本。越界返回 `None`（对应 TS 的抛异常，Rust 侧改为可恢复错误）。
pub fn index_to_ref(index: u32) -> Option<String> {
    if !(MIN_INDEX..=MAX_INDEX).contains(&index) {
        return None;
    }
    Some(format!("m{index:0REF_WIDTH$}"))
}

/// ref 文本 → 序号。非法返回 `None`。
///
/// 接受 `m` 开头、后跟 1..=5 位数字的写法（容忍前导零）。
pub fn ref_to_index(reference: &str) -> Option<u32> {
    let trimmed = reference.trim();
    let rest = trimmed
        .strip_prefix('m')
        .or_else(|| trimmed.strip_prefix('M'))?;
    if rest.is_empty() || rest.len() > 5 || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let index: u32 = rest.parse().ok()?;
    if !(MIN_INDEX..=MAX_INDEX).contains(&index) {
        return None;
    }
    Some(index)
}

/// 原始 id → ref。
pub fn ref_for_raw<'a>(map: &'a MessageRefMap, raw_id: &str) -> Option<&'a str> {
    map.by_raw.get(raw_id).map(String::as_str)
}

/// ref → 原始 id。
pub fn raw_for_ref<'a>(map: &'a MessageRefMap, reference: &str) -> Option<&'a str> {
    map.by_ref.get(reference).map(String::as_str)
}

/// 分配结果。
#[derive(Debug, Clone)]
pub struct AssignRefsResult {
    /// 新映射。
    pub map: MessageRefMap,
    /// 下一个可用序号。
    pub next_index: u32,
    /// 本次新分配数量。
    pub newly_assigned: usize,
}

/// `assignRefs` 选项。
pub struct AssignRefsOptions<'a> {
    /// 既有映射。
    pub existing: &'a MessageRefMap,
    /// 起始序号。
    pub next_index: u32,
    /// 受保护判定：为真时写入 `BLOCKED` 占位。
    pub is_protected: Option<&'a dyn Fn(&CoreMessage) -> bool>,
}

/// 为消息列表分配 ref（不修改输入，返回新映射）。
///
/// 既有映射中的条目被保留；只有从未见过的原始 id 才分配新 ref。
/// 已存在但值为 `BLOCKED` 的条目也视为「已处理」，不再分配。
pub fn assign_refs(messages: &[CoreMessage], options: AssignRefsOptions<'_>) -> AssignRefsResult {
    let mut map = MessageRefMap {
        by_raw: options.existing.by_raw.clone(),
        by_ref: options.existing.by_ref.clone(),
    };
    let mut cursor = if options.next_index >= MIN_INDEX {
        options.next_index
    } else {
        MIN_INDEX
    };
    let mut newly_assigned = 0usize;

    for message in messages {
        if message.id.is_empty() {
            continue;
        }
        if map.by_raw.contains_key(&message.id) {
            continue;
        }
        if let Some(is_protected) = options.is_protected {
            if is_protected(message) {
                map.by_raw
                    .insert(message.id.clone(), BLOCKED_REF.to_string());
                continue;
            }
        }
        let Some((reference, index)) = allocate_free_ref(&map, cursor) else {
            // 容量耗尽：停止分配，已分配的部分仍然有效。
            break;
        };
        cursor = index + 1;
        map.by_raw.insert(message.id.clone(), reference.clone());
        map.by_ref.insert(reference, message.id.clone());
        newly_assigned += 1;
    }

    AssignRefsResult {
        map,
        next_index: cursor,
        newly_assigned,
    }
}

/// 从 `start` 起寻找未被占用的 ref。
fn allocate_free_ref(map: &MessageRefMap, start: u32) -> Option<(String, u32)> {
    let mut candidate = start.max(MIN_INDEX);
    while candidate <= MAX_INDEX {
        let reference = index_to_ref(candidate)?;
        if !map.by_ref.contains_key(&reference) {
            return Some((reference, candidate));
        }
        candidate += 1;
    }
    None
}

/// 由 `by_raw` 重建 `by_ref`（跳过 `BLOCKED`）。
pub fn rebuild_ref_index(map: &MessageRefMap) -> MessageRefMap {
    let mut by_ref = BTreeMap::new();
    for (raw_id, reference) in &map.by_raw {
        if reference != BLOCKED_REF {
            by_ref.insert(reference.clone(), raw_id.clone());
        }
    }
    MessageRefMap {
        by_raw: map.by_raw.clone(),
        by_ref,
    }
}

/// 当前使用的最大 ref 序号。
pub fn highest_used_index(map: &MessageRefMap) -> u32 {
    let mut highest = 0;
    for reference in map.by_raw.values() {
        if reference == BLOCKED_REF {
            continue;
        }
        if let Some(index) = ref_to_index(reference) {
            if index > highest {
                highest = index;
            }
        }
    }
    highest
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ContentType, Role};

    fn msg(id: &str) -> CoreMessage {
        CoreMessage::text(id, Role::User, "hi")
    }

    #[test]
    fn index_to_ref_should_zero_pad_to_five_digits() {
        assert_eq!(index_to_ref(1).as_deref(), Some("m00001"));
        assert_eq!(index_to_ref(99_999).as_deref(), Some("m99999"));
    }

    #[test]
    fn index_to_ref_should_reject_out_of_range() {
        assert_eq!(index_to_ref(0), None);
        assert_eq!(index_to_ref(100_000), None);
    }

    #[test]
    fn ref_to_index_should_round_trip() {
        assert_eq!(ref_to_index("m00005"), Some(5));
        assert_eq!(ref_to_index("m5"), Some(5));
        assert_eq!(ref_to_index(" M00042 "), Some(42));
    }

    #[test]
    fn ref_to_index_should_reject_malformed() {
        assert_eq!(ref_to_index("x00001"), None);
        assert_eq!(ref_to_index("m"), None);
        assert_eq!(ref_to_index("m000000"), None);
        assert_eq!(ref_to_index("mabc"), None);
    }

    #[test]
    fn assign_refs_should_allocate_sequentially() {
        let messages = vec![msg("a"), msg("b")];
        let existing = empty_ref_map();
        let result = assign_refs(
            &messages,
            AssignRefsOptions {
                existing: &existing,
                next_index: 1,
                is_protected: None,
            },
        );
        assert_eq!(result.newly_assigned, 2);
        assert_eq!(result.map.by_raw["a"], "m00001");
        assert_eq!(result.map.by_raw["b"], "m00002");
        assert_eq!(result.next_index, 3);
    }

    #[test]
    fn assign_refs_should_keep_existing_and_mark_protected() {
        let messages = vec![msg("a"), msg("b"), msg("c")];
        let mut existing = empty_ref_map();
        existing.by_raw.insert("a".into(), "m00001".into());
        existing.by_ref.insert("m00001".into(), "a".into());
        let protected = |m: &CoreMessage| m.id == "b";
        let result = assign_refs(
            &messages,
            AssignRefsOptions {
                existing: &existing,
                next_index: 2,
                is_protected: Some(&protected),
            },
        );
        assert_eq!(result.newly_assigned, 1);
        assert_eq!(result.map.by_raw["a"], "m00001");
        assert_eq!(result.map.by_raw["b"], BLOCKED_REF);
        assert_eq!(result.map.by_raw["c"], "m00002");
    }

    #[test]
    fn highest_used_index_ignores_blocked() {
        let mut map = empty_ref_map();
        map.by_raw.insert("a".into(), "m00003".into());
        map.by_raw.insert("b".into(), BLOCKED_REF.into());
        assert_eq!(highest_used_index(&map), 3);
    }

    #[test]
    fn rebuild_ref_index_skips_blocked() {
        let mut map = empty_ref_map();
        map.by_raw.insert("a".into(), "m00001".into());
        map.by_raw.insert("b".into(), BLOCKED_REF.into());
        let rebuilt = rebuild_ref_index(&map);
        assert_eq!(rebuilt.by_ref.get("m00001").map(String::as_str), Some("a"));
        assert_eq!(rebuilt.by_ref.len(), 1);
    }

    #[test]
    fn assign_refs_should_skip_empty_ids_and_duplicates() {
        let mut a = msg("");
        a.content_type = ContentType::Text;
        let messages = vec![a, msg("a"), msg("a")];
        let existing = empty_ref_map();
        let result = assign_refs(
            &messages,
            AssignRefsOptions {
                existing: &existing,
                next_index: 1,
                is_protected: None,
            },
        );
        assert_eq!(result.newly_assigned, 1);
    }
}
