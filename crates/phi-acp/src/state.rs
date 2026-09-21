//! 压缩状态管理 —— 对应 acp-kernel `src/state.ts`。

use crate::types::{CompressionBlock, CompressionState, CompressionTier};

/// 创建初始状态。
pub fn create_initial_state() -> CompressionState {
    CompressionState {
        blocks: Vec::new(),
        message_refs: Default::default(),
        token_snapshot: Default::default(),
        nudge: Default::default(),
        stats: Default::default(),
        absorbed: Vec::new(),
        absorbed_outputs: Vec::new(),
        next_absorb_id: 1,
        terminal_streak: None,
        rules: Vec::new(),
        next_rule_id: Some(1),
        next_block_id: 1,
        next_run_id: 1,
    }
}

/// 分配块 id（`bN`）。
pub fn allocate_block_id(state: &mut CompressionState) -> String {
    let id = state.next_block_id.max(1);
    state.next_block_id = id + 1;
    format!("b{id}")
}

/// 分配批次 id（`rN`）。
pub fn allocate_run_id(state: &mut CompressionState) -> String {
    let id = state.next_run_id.max(1);
    state.next_run_id = id + 1;
    format!("r{id}")
}

/// 分配一个可逆吸收句柄（`aN`）。
pub fn allocate_absorb_id(state: &mut CompressionState) -> String {
    let id = state.next_absorb_id.max(1);
    state.next_absorb_id = id + 1;
    format!("a{id}")
}

/// 按句柄查找可逆吸收记录。
pub fn absorbed_output_by_handle<'a>(
    state: &'a CompressionState,
    handle: &str,
) -> Option<&'a crate::types::AbsorbedOutput> {
    state
        .absorbed_outputs
        .iter()
        .find(|output| output.handle == handle)
}

/// 按 id 查找块。
pub fn block_by_id<'a>(
    state: &'a CompressionState,
    block_id: &str,
) -> Option<&'a CompressionBlock> {
    state.blocks.iter().find(|block| block.block_id == block_id)
}

/// 全部活跃块。
pub fn active_blocks(state: &CompressionState) -> Vec<&CompressionBlock> {
    state.blocks.iter().filter(|block| block.active).collect()
}

/// 活跃块覆盖的原始消息 id 集合。
pub fn covered_message_ids(state: &CompressionState) -> std::collections::BTreeSet<String> {
    let mut covered = std::collections::BTreeSet::new();
    for block in &state.blocks {
        if !block.active {
            continue;
        }
        for id in &block.effective_message_ids {
            covered.insert(id.clone());
        }
    }
    covered
}

/// 最高活跃层级（0 表示无活跃块）。
pub fn highest_active_tier(state: &CompressionState) -> CompressionTier {
    let mut highest = 0;
    for block in &state.blocks {
        if block.active && block.tier > highest {
            highest = block.tier;
        }
    }
    highest
}

/// 推进每个活跃块的存活计数，达到阈值后转 `old`。
pub fn advance_survival(state: &mut CompressionState, promotion_threshold: u32) {
    for block in &mut state.blocks {
        if !block.active {
            continue;
        }
        block.survived_count += 1;
        if block.survived_count >= promotion_threshold {
            block.generation = crate::types::BlockGeneration::Old;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocate_ids_should_increment() {
        let mut state = create_initial_state();
        assert_eq!(allocate_block_id(&mut state), "b1");
        assert_eq!(allocate_block_id(&mut state), "b2");
        assert_eq!(allocate_run_id(&mut state), "r1");
        assert_eq!(state.next_block_id, 3);
        assert_eq!(state.next_run_id, 2);
    }

    #[test]
    fn active_and_covered_should_filter_inactive() {
        let mut state = create_initial_state();
        state.blocks.push(CompressionBlock {
            block_id: "b1".into(),
            active: true,
            tier: 1,
            effective_message_ids: vec!["m1".into()],
            ..Default::default()
        });
        state.blocks.push(CompressionBlock {
            block_id: "b2".into(),
            active: false,
            effective_message_ids: vec!["m2".into()],
            ..Default::default()
        });
        assert_eq!(active_blocks(&state).len(), 1);
        assert_eq!(covered_message_ids(&state).len(), 1);
        assert_eq!(highest_active_tier(&state), 1);
    }

    #[test]
    fn advance_survival_should_promote_after_threshold() {
        let mut state = create_initial_state();
        state.blocks.push(CompressionBlock {
            block_id: "b1".into(),
            active: true,
            ..Default::default()
        });
        advance_survival(&mut state, 2);
        assert_eq!(
            state.blocks[0].generation,
            crate::types::BlockGeneration::Young
        );
        advance_survival(&mut state, 2);
        assert_eq!(
            state.blocks[0].generation,
            crate::types::BlockGeneration::Old
        );
    }
}
