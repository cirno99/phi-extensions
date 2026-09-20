// gen-prompts.mjs — 从 pi 版 asymptotic-thinking 的 TS 提示词模块生成 Rust 源码。
//
// 用法：node scripts/gen-prompts.mjs <pi 扩展 src 目录> <输出 prompts.rs 路径>
//
// 该脚本是一次性代码生成工具：把 27 个 TS 提示词模块（每个导出 buildPrompt(diff, state)）
// 的返回值在 6 难度 × 6 状态上全量求值，再发射成 Rust 的嵌套 match。
// 生成结果不依赖 Node 运行时，属于纯静态数据。
//
// 除 pi 版的 27 个模块外，还内置一份 ZIG_DEV 提示词（pi 上游没有 Zig 模块），
// 使语言覆盖包含 Zig；内置内容见下方 BUILTIN_PROMPT_MODULES。

import { cp, mkdir, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { tmpdir } from "node:os";

const DIFFICULTIES = ["TRIVIAL", "SIMPLE", "MODERATE", "COMPLEX", "HARD", "EXTREME"];
const STATES = ["START", "DEEP_UNDERSTAND", "DESIGN", "EXECUTE", "VERIFY", "END"];

const DIFF_RUST = {
  TRIVIAL: "Trivial",
  SIMPLE: "Simple",
  MODERATE: "Moderate",
  COMPLEX: "Complex",
  HARD: "Hard",
  EXTREME: "Extreme",
};

const STATE_RUST = {
  START: "Start",
  DEEP_UNDERSTAND: "DeepUnderstand",
  DESIGN: "Design",
  EXECUTE: "Execute",
  VERIFY: "Verify",
  END: "End",
};

const MASTERS = ["CODING", "RETRIEVAL", "ANALYTICS", "DEVOPS", "ENTERTAINMENT", "GENERAL"];

const SUBS = {
  CODING: ["JAVA_DEV", "RUST_DEV", "PYTHON_DEV", "JS_DEV", "GO_DEV", "ZIG_DEV", "CRUD_DEV", "BUG_FIX", "CODE_REFACTOR", "TESTING", "ARCHITECT", "CODE_REVIEW", "PERF_OPTIMIZE"],
  RETRIEVAL: ["PAPER_RETRIEVAL", "DAILY_RETRIEVAL", "DOC_RETRIEVAL", "CODE_RETRIEVAL"],
  ANALYTICS: ["DATA_ANALYSIS", "CODE_ANALYSIS", "LOG_ANALYSIS", "REQUIREMENT_ANALYSIS"],
  DEVOPS: ["DEPLOY", "MONITOR", "CICD", "CONFIG"],
  ENTERTAINMENT: ["FUN_CHAT", "CREATIVE_WRITING"],
  GENERAL: ["GENERAL"],
};

/**
 * pi 上游没有 Zig 提示词模块，这里内置一份与 RUST_DEV 同构的 ZIG_DEV 表，
 * 保证「编程类」的语言覆盖包含 Zig。键为 `MASTER/SUB`。
 */
const ZIG_DOMAIN = {
  DEEP_UNDERSTAND: "明确Zig版本与目标平台 → 识别comptime求值边界与分配器约束 → 评估错误联合(error union)与defer清理路径",
  DESIGN: "确定模块划分与依赖 → 设计分配器传递与错误传播路径 → 列出涉及文件、前置依赖、核心步骤、验收标准 → 考虑defer/errdefer清理与边界条件",
  EXECUTE: "每步完成后验证结果 → 错误分析根因后修正 → 完成目标后 asymptotic-think_transition 进入 VERIFY。",
  VERIFY: "无隐式分配 · 错误联合全部处理 · comptime边界清晰",
};
const ZIG_POINTS = "显式错误联合 · 分配器显式传递 · comptime求值边界";
const ZIG_DOMAIN_VERIFY_EXTRA = "逐项对照需求和方案检查 → 功能完整 → 边界条件覆盖 → 代码规范 → 性能达标。";

function buildZigPrompt(diff, state) {
  const discipline = "\n\n严格遵守《编程与架构准则》";
  const hard = " 需深度分析所有边界条件、隐含约束和潜在风险。";

  if (state === "DEEP_UNDERSTAND") {
    const suffix = diff === "HARD" || diff === "EXTREME" ? hard : "";
    return `当前为编程类Zig开发（${diff === "HARD" ? "困难" : diff === "EXTREME" ? "极难" : diff === "TRIVIAL" ? "微不足道" : diff === "SIMPLE" ? "简单" : diff === "COMPLEX" ? "复杂" : "中等"}难度）。${suffix}\n\n${ZIG_DOMAIN.DEEP_UNDERSTAND}\n\n领域要点：${ZIG_POINTS}${discipline}`;
  }

  if (state === "DESIGN") {
    if (diff === "TRIVIAL") return "";
    if (diff === "SIMPLE") return `确定技术路径和关键步骤 → 列出涉及文件与验收标准。${discipline}`;
    const extras = diff === "HARD" || diff === "EXTREME" ? "\n→ 列出多个备选方案并对比优劣。" : "";
    return `${ZIG_DOMAIN.DESIGN}${extras}${discipline}`;
  }

  if (state === "EXECUTE") {
    if (diff === "TRIVIAL") return `快速完成目标后 asymptotic-think_transition 进入 VERIFY。${discipline}`;
    const strict = diff === "HARD" || diff === "EXTREME";
    const lead = strict
      ? ZIG_DOMAIN.EXECUTE
      : "错误分析根因后修正 → 完成目标后 asymptotic-think_transition 进入 VERIFY。";
    const points = `\n${ZIG_POINTS}`;
    const verify = strict ? "\n每步必须有明确验证，不可跳过" : "";
    return `${lead}${points}${verify}${discipline}`;
  }

  if (state === "VERIFY") {
    if (diff === "HARD" || diff === "EXTREME") {
      return `${ZIG_DOMAIN_VERIFY_EXTRA}\n${ZIG_DOMAIN.VERIFY}${discipline}`;
    }
    return `${ZIG_DOMAIN.VERIFY}${discipline}`;
  }

  return "";
}

const BUILTIN_PROMPT_MODULES = {
  "CODING/ZIG_DEV": { buildPrompt: buildZigPrompt },
};

function pascalFromSnake(name) {
  return name
    .toLowerCase()
    .split("_")
    .map((part) => part.charAt(0).toUpperCase() + part.slice(1))
    .join("");
}

/** 递归重写相对导入，为其补上 .ts 扩展名（Node ESM 要求显式扩展）。 */
async function rewriteImports(dir) {
  const entries = await readdir(dir, { withFileTypes: true });
  for (const entry of entries) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      await rewriteImports(full);
      continue;
    }
    if (!entry.name.endsWith(".ts")) continue;
    const src = await readFile(full, "utf8");
    const out = src.replace(/(from\s+")(\.\.?\/[^"]*)(")/g, (all, a, spec, c) => {
      if (/\.(ts|js|json|md)$/.test(spec)) return all;
      return a + spec + ".ts" + c;
    });
    if (out !== src) await writeFile(full, out, "utf8");
  }
}

function rustString(value) {
  return JSON.stringify(value);
}

async function main() {
  const [srcArg, outArg] = process.argv.slice(2);
  if (!srcArg || !outArg) {
    console.error("用法: node scripts/gen-prompts.mjs <src 目录> <输出 prompts.rs>");
    process.exit(1);
  }
  const srcDir = resolve(srcArg);
  const outFile = resolve(outArg);

  const staging = join(tmpdir(), "phi-asym-gen-" + process.pid);
  if (existsSync(staging)) await rm(staging, { recursive: true, force: true });
  await cp(srcDir, staging, { recursive: true });
  await rewriteImports(staging);

  const registry = await import("file://" + join(staging, "prompt-registry.ts"));

  const lines = [];
  lines.push("// 本文件由 scripts/gen-prompts.mjs 自动生成，请勿手工修改。");
  lines.push("// 数据来源：pi 版 asymptotic-thinking 扩展的 src/prompts/**（27 个模块）");
  lines.push("//  + 本项目内置的 ZIG_DEV 提示词（pi 上游无 Zig 模块），共 28 个。");
  lines.push("");
  lines.push("use crate::types::{Difficulty, MasterTaskType, State, SubTaskType};");
  lines.push("");
  lines.push("/// 按任务画像与状态返回领域提示词片段。");
  lines.push("pub(crate) fn build_prompt(");
  lines.push("    master: MasterTaskType,");
  lines.push("    sub: SubTaskType,");
  lines.push("    diff: Difficulty,");
  lines.push("    state: State,");
  lines.push(") -> &'static str {");
  lines.push("    match (master, sub) {");

  let armCount = 0;
  for (const master of MASTERS) {
    for (const sub of SUBS[master]) {
      // 内置表优先：prompt-registry 对未知子类型会回落到 GENERAL，
      // 直接查 registry 会把 ZIG_DEV 生成成通用提示词。
      const module =
        BUILTIN_PROMPT_MODULES[master + "/" + sub] ?? registry.loadPromptModule(master, sub);
      if (!module) {
        throw new Error("找不到提示词模块: " + master + "/" + sub);
      }
      lines.push("        (MasterTaskType::" + pascalFromSnake(master) + ", SubTaskType::" + pascalFromSnake(sub) + ") => {");
      lines.push("            match (diff, state) {");
      for (const state of STATES) {
        const byDiff = {};
        for (const diff of DIFFICULTIES) {
          byDiff[diff] = module.buildPrompt(diff, state);
        }
        const fallback = byDiff.MODERATE;
        for (const diff of DIFFICULTIES) {
          if (byDiff[diff] === fallback) continue;
          lines.push(
            "                (Difficulty::" + DIFF_RUST[diff] + ", State::" + STATE_RUST[state] + ") => " + rustString(byDiff[diff]) + ",",
          );
          armCount += 1;
        }
        if (fallback !== "") {
          lines.push("                (_, State::" + STATE_RUST[state] + ") => " + rustString(fallback) + ",");
          armCount += 1;
        }
      }
      lines.push("                _ => \"\",");
      lines.push("            }");
      lines.push("        }");
    }
  }

  lines.push('        _ => "",');
  lines.push("    }");
  lines.push("}");
  lines.push("");

  await mkdir(dirname(outFile), { recursive: true });
  await writeFile(outFile, lines.join("\n"), "utf8");
  await rm(staging, { recursive: true, force: true });
  console.log("已生成 " + outFile + "（" + armCount + " 条 match 分支）");
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
