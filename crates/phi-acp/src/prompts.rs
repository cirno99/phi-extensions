//! 压缩提示词 —— 对应 acp-kernel `src/compression-rules.ts` / `src/prompts.ts`。
//!
//! 这些规则是「载重」文本：它们经过数月生产调优，直接决定摘要质量。逐字保留
//! 英文原文（提示词稳定性也关系到上游前缀缓存命中）。

/// 核心压缩哲学。
pub const COMPRESS_PHILOSOPHY: &str = "Compression Philosophy:
- All compression serves the primary task, but be frugal.
- Context capacity is precious. Save context by compressing consumed outputs, not by avoiding tools.
- Compress by need, not by percentage.
- Work from summaries, not raw tool outputs. All listed ranges (user prompts, tool outputs, code, logs, exploration, intermediate steps) should be compressed to summary format — the ONLY exceptions are protected content, content the current step is actively using, or critical content you cannot reconstruct.";

/// 如何写 tier-1 摘要。
pub const HOW_TO_COMPRESS_RULES: &str = "HOW TO COMPRESS

When you call `compress`, the summary you write becomes the only record of the replaced conversation. Make it self-contained and complete: every user request, experiment purpose, and work task in the range must be accurately captured. A later reader (or you, after decompressing) should be able to continue the task WITHOUT needing the original. The summary records the PAST as of this block's creation: label recorded task state as history (\"TASK AS OF THIS BLOCK: ...\") — never as a live instruction, so a later reader treats it as settled context, not something to re-execute. Write plain text with real unicode characters; never copy \\uXXXX escape sequences or JSON-escaped fragments out of tool output.

KEEP VERBATIM — never paraphrase or abbreviate these:
- Full file paths with line numbers, directory prefix on every mention (`lib/hooks.ts:347`, `src/index.ts:12-18`). Never abbreviate to a bare filename — they are ambiguous and cannot be grepped or decompressed-to later.
- Function, class, and type signatures (exact names, params, return types) AND critical code lines that encode logic.
- Error messages and stack traces (exact text — you need the literal string to grep for it later).
- Key details from reports and analyses — not just the conclusion.
- Decisions and their rationale (\"chose X over Y because Z\" — the \"because\" is load-bearing).
- Constraints discovered (\"must support Node 22\", \"no new dependencies\").
- Exact values: versions, config keys, thresholds, magic numbers.
- User intent — quote short user messages verbatim ONLY WITH their message ref, e.g. `User said (m00132): \"ship it tonight\"`. Without a verifiable ref, paraphrase. Quotes are historical records, never current directives.
- The user's overall goal and any changes to it.
- Purpose behind each significant action.
- Open questions and unresolved TODOs.
- Message refs of key anchors (`m00420`, `m00510–m00520`).

DROP — extract the signal, discard the vessel:
- Verbose logs once you have captured the error line or the result.
- Duplicate file reads once the needed content is recorded.
- Consumed exploration once you have extracted the facts you need.
- Dead-end exploration — but PRESERVE the lesson in one line: \"tried X, failed because Y\".
- Back-and-forth discussion and self-corrections once the final position is captured.
- Repeated status checks once state is known.

For each significant item you DROP, add a one-line CONTENT description of what it covers — not where it lives.

PRIORITY — when the summary must be compact, preserve in this order:
1. User's overall goal, goal evolution, intent, and hard constraints.
2. Decisions and rationale.
3. Exact technical artifacts: paths, signatures, errors, values.
4. Conclusions and key findings.
5. Lessons learned: what failed and why.

Write dense, scannable bullets — not narrative prose. Every line must earn its place. Do not mimic the style of existing summaries in context; follow these rules.";

/// tier-2 蒸馏规则。
pub const TIER2_DISTILL_RULES: &str = "TIER 2 COMPRESSION — DISTILLATION

You are compressing historical summaries (not raw conversation). Your job is to DISTILL them: extract only what matters for future work, discard the process.

KEEP — these are the only things that survive distillation:
- Decisions and their rationale (\"chose X over Y because Z\").
- Final outcomes: version numbers shipped, PR numbers merged/closed, bugs fixed or deferred.
- Key lessons: what failed and why.
- Critical constraints discovered.
- Design decisions with architectural impact.
- User quotes and task state only as attributed history: keep the source ref with any user quote.
- Whether content is OBSOLETE or SUPERSEDED — mark with one line.
- Function/class/type names and module paths that are the SUBJECT of the work.
- Exploration findings: keep the CONCLUSION in one line.

DROP:
- Exact line numbers, diffs, verbose function signatures, full code listings.
- Build/deploy process details, test execution steps.
- Review process details.
- Verbose logs, command output, intermediate debugging steps.

FORMAT:
- Start each distilled block with a source header line: `Source: bN+bM+... (XK→YK tok, Zx). [original topic]`
- 3-5 bullet points per source block, each a self-contained fact.
- Dense, scannable — no narrative prose.
- Start with the outcome, not the process.
- Cross-block synthesis: MERGE same-topic blocks into one group.

SIZE TARGET: 50-150 tokens per source block (excluding the header).";

/// tier-3 超浓缩规则。
pub const TIER3_CONDENSE_RULES: &str = "TIER 3 COMPRESSION — ULTRA-CONDENSATION

You are compressing distilled summaries (Tier 2) into ultra-condensed facts (Tier 3). Your job is to reduce them to bare factual references.

PRIORITY:
1. Shipped outcomes (versions released, PRs merged) — permanent record.
2. Open work (PRs/issues still pending).
3. Key decisions with architectural impact.
4. Critical constraints.
Drop everything else. Tier 3 is a lookup index, not a knowledge base.

FORMAT:
- Start with a source header line: `Source: bN+bM+... (XK→YK tok, Zx). [original topic]`
- Output 1-3 facts per source block. Each fact is a single line: subject + outcome.
- Format: \"[PR/Issue/Version] — [outcome in ≤8 words]\"
- Merge related facts from different source blocks.

DROP:
- Multi-sentence context.
- Lessons learned unless the failure is likely to recur.
- Design rationale details.
- Anything marked [OBSOLETE] or [SUPERSEDED] — drop entirely.

SIZE TARGET: 30-60 tokens per source block (including header).";

/// 提示词集合。
#[derive(Debug, Clone)]
pub struct Prompts {
    /// 压缩哲学。
    pub compress_philosophy: String,
    /// tier-1 规则。
    pub how_to_compress_rules: String,
    /// tier-2 规则。
    pub tier2_distill_rules: String,
    /// tier-3 规则。
    pub tier3_condense_rules: String,
}

impl Default for Prompts {
    fn default() -> Self {
        Self {
            compress_philosophy: COMPRESS_PHILOSOPHY.to_string(),
            how_to_compress_rules: HOW_TO_COMPRESS_RULES.to_string(),
            tier2_distill_rules: TIER2_DISTILL_RULES.to_string(),
            tier3_condense_rules: TIER3_CONDENSE_RULES.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_prompts_should_be_non_empty() {
        let p = Prompts::default();
        assert!(p.compress_philosophy.starts_with("Compression Philosophy:"));
        assert!(p.how_to_compress_rules.starts_with("HOW TO COMPRESS"));
        assert!(p.tier2_distill_rules.contains("TIER 2"));
        assert!(p.tier3_condense_rules.contains("TIER 3"));
    }
}
