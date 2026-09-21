//! 持久规则（`acp_rule`）—— 对应 acp-kernel `src/rules.ts`。
//!
//! 模型记录的原则性提醒，会随系统提示词注入，并在压缩中硬保护
//! （[`crate::protected::ALWAYS_PROTECTED_TOOLS`] 含 `acp_rule`），因此记录过的
//! 规则能跨上下文压缩存活。
//!
//! 与上游一致的三条硬性约束（缺一不可，否则模型会被自己写坏的输入困住）：
//! - 单条规则有字符上限（默认 300），超长直接拒绝并给出可执行的修复提示；
//! - 完全相同的规则不重复记录；
//! - 规则条数有上限（默认 50），满了提示先删再写。
//!
//! id 分配是**单调**的：计数器只增不减，且会用现存规则里的最大编号回填
//! （上游注释：手工构造的状态也要能正确续号），因此删除后不会重发旧 id。

use crate::types::{CompressionState, RuleRecord};

/// 规则工具名。
pub const RULE_TOOL_NAME: &str = "acp_rule";

/// 规则条数上限（对应上游 `DEFAULT_RULE_LIMITS.maxRules`）。
pub const DEFAULT_MAX_RULES: usize = 50;

/// 单条规则字符上限（对应上游 `DEFAULT_RULE_LIMITS.maxRuleChars`）。
pub const DEFAULT_MAX_RULE_CHARS: usize = 300;

/// 解析 `ruleN` 里的 N（非 `ruleN` 形状返回 `None`）。
fn rule_number(id: &str) -> Option<u64> {
    id.strip_prefix("rule")?.parse().ok()
}

/// 现存规则里的最大编号（对应上游 `highestRuleNumber`）。
fn highest_rule_number(state: &CompressionState) -> u64 {
    state
        .rules
        .iter()
        .filter_map(|rule| rule_number(&rule.id))
        .max()
        .unwrap_or(0)
}

/// 下一次 [`add_rule`] 会发出的 id（纯函数，不改状态）。
///
/// 计数器单调递增，永不重发已发出的 id；手工构造的状态用现存最大编号回填。
pub fn allocate_rule_id(state: &CompressionState) -> String {
    let next = state
        .next_rule_id
        .unwrap_or(1)
        .max(highest_rule_number(state) + 1);
    format!("rule{next}")
}

/// 添加一条规则（文本先去首尾空白）。
///
/// 校验顺序对齐上游 `addRule`：空文本 → 超长 → 重复 → 条数上限。
pub fn add_rule(state: &mut CompressionState, text: &str) -> Result<RuleRecord, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("rule text is empty — provide the reminder to record.".to_string());
    }
    let len = trimmed.chars().count();
    if len > DEFAULT_MAX_RULE_CHARS {
        return Err(format!(
            "{len} chars exceeds the {DEFAULT_MAX_RULE_CHARS}-char limit — keep rules short and principle-level."
        ));
    }
    if let Some(duplicate) = state.rules.iter().find(|rule| rule.text == trimmed) {
        return Err(format!(
            "identical rule already exists ({}) — no change.",
            duplicate.id
        ));
    }
    if state.rules.len() >= DEFAULT_MAX_RULES {
        return Err(format!(
            "rule limit reached ({DEFAULT_MAX_RULES}) — remove or clear outdated rules first."
        ));
    }
    let next = state
        .next_rule_id
        .unwrap_or(1)
        .max(highest_rule_number(state) + 1);
    let rule = RuleRecord {
        id: format!("rule{next}"),
        text: trimmed.to_string(),
    };
    state.rules.push(rule.clone());
    state.next_rule_id = Some(next + 1);
    Ok(rule)
}

/// 删除一条规则；id 不存在时报错并给出下一步（对应上游 `removeRule`）。
pub fn remove_rule(state: &mut CompressionState, id: &str) -> Result<RuleRecord, String> {
    let target = id.trim();
    let Some(index) = state.rules.iter().position(|rule| rule.id == target) else {
        return Err(format!(
            "no rule with id \"{target}\" — list current rules first (omit the text argument)."
        ));
    };
    Ok(state.rules.remove(index))
}

/// 清空全部规则，返回清掉的条数。
pub fn clear_rules(state: &mut CompressionState) -> usize {
    let count = state.rules.len();
    state.rules.clear();
    count
}

/// 系统提示词里的规则段；无规则时返回空串（空会话零 token 开销）。
///
/// 必须带 `[ruleN]`：模型要靠这个 id 才能删除规则。
pub fn format_rules_for_prompt(state: &CompressionState) -> String {
    if state.rules.is_empty() {
        return String::new();
    }
    let mut lines =
        vec!["# Persistent rules (recorded via acp_rule — kept across compression)".to_string()];
    for rule in &state.rules {
        lines.push(format!("- [{}] {}", rule.id, rule.text));
    }
    lines.join("\n")
}

/// `acp_rule` 列出规则时的编号渲染（对应上游 `formatRulesList`）。
pub fn format_rules_list(state: &CompressionState) -> String {
    state
        .rules
        .iter()
        .enumerate()
        .map(|(index, rule)| format!("{}. [{}] {}", index + 1, rule.id, rule.text))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(rules: &[(&str, &str)]) -> CompressionState {
        CompressionState {
            rules: rules
                .iter()
                .map(|(id, text)| RuleRecord {
                    id: (*id).to_string(),
                    text: (*text).to_string(),
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn add_should_trim_and_echo_the_recorded_rule() {
        let mut state = CompressionState::default();
        let rule = add_rule(&mut state, "  always run tests  ").expect("应记录");
        assert_eq!(rule.id, "rule1");
        assert_eq!(rule.text, "always run tests");
        assert_eq!(state.next_rule_id, Some(2));
        assert_eq!(add_rule(&mut state, "second").expect("应记录").id, "rule2");
    }

    #[test]
    fn add_should_reject_empty_and_oversized() {
        let mut state = CompressionState::default();
        assert!(add_rule(&mut state, "   ").is_err());
        let long = "x".repeat(DEFAULT_MAX_RULE_CHARS + 1);
        let error = add_rule(&mut state, &long).expect_err("超长应被拒绝");
        assert!(
            error.starts_with("301 chars exceeds the 300-char limit"),
            "错误文案应与上游一致：{error}"
        );
        assert!(state.rules.is_empty(), "被拒绝的输入不得改动状态");
    }

    #[test]
    fn add_should_reject_identical_duplicate() {
        let mut state = CompressionState::default();
        add_rule(&mut state, "dup rule").expect("首次应记录");
        let error = add_rule(&mut state, "dup rule").expect_err("重复应被拒绝");
        assert_eq!(error, "identical rule already exists (rule1) — no change.");
        assert_eq!(state.rules.len(), 1);
    }

    #[test]
    fn add_should_cap_at_max_rules() {
        let mut state = CompressionState::default();
        for index in 0..DEFAULT_MAX_RULES {
            add_rule(&mut state, &format!("rule body {index}")).expect("应记录");
        }
        let error = add_rule(&mut state, "one too many").expect_err("应触发上限");
        assert_eq!(
            error,
            "rule limit reached (50) — remove or clear outdated rules first."
        );
    }

    #[test]
    fn allocate_should_backfill_from_highest_surviving_id() {
        // 手工构造的状态没有 nextRuleId：必须从现存最大编号续号，而不是回到 rule1。
        let state = state_with(&[("rule3", "a"), ("rule7", "b")]);
        assert_eq!(allocate_rule_id(&state), "rule8");
        // 计数器更高时以计数器为准（单调，不重发）。
        let state = CompressionState {
            next_rule_id: Some(20),
            ..state_with(&[("rule3", "a")])
        };
        assert_eq!(allocate_rule_id(&state), "rule20");
    }

    #[test]
    fn remove_should_error_on_unknown_id() {
        let mut state = state_with(&[("rule1", "a")]);
        assert_eq!(
            remove_rule(&mut state, "rule1").expect("应删除").id,
            "rule1"
        );
        assert!(state.rules.is_empty());
        let error = remove_rule(&mut state, "rule1").expect_err("已删除的 id 应报错");
        assert_eq!(
            error,
            "no rule with id \"rule1\" — list current rules first (omit the text argument)."
        );
    }

    #[test]
    fn clear_should_report_count() {
        let mut state = state_with(&[("rule1", "a"), ("rule2", "b")]);
        assert_eq!(clear_rules(&mut state), 2);
        assert!(state.rules.is_empty());
        assert_eq!(clear_rules(&mut state), 0);
    }

    #[test]
    fn prompt_rendering_should_include_ids() {
        let empty = CompressionState::default();
        assert_eq!(format_rules_for_prompt(&empty), "");
        let state = state_with(&[("rule1", "a"), ("rule2", "b")]);
        assert_eq!(
            format_rules_for_prompt(&state),
            "# Persistent rules (recorded via acp_rule — kept across compression)\n- [rule1] a\n- [rule2] b"
        );
        assert_eq!(format_rules_list(&state), "1. [rule1] a\n2. [rule2] b");
    }
}
