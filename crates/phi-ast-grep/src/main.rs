// main.rs — phi-ast-grep 扩展入口。
//
// 由 pi 版 pi-ast-grep 的 `src/index.ts` 移植。
//
// 注册：
//   - 工具 `ast_grep_search`（AST 结构搜索，readable / 并行安全）
//   - 工具 `ast_grep_replace`（AST 结构改写，顺序执行，dry-run 默认 true）
//   - 命令 `/ast-grep`（显示二进制路径与版本）
//
// 与 pi 版的**唯一有意差异**：不做自动下载。要求宿主 PATH 上已有 ast-grep
// 二进制（`sg` 或 `ast-grep`），缺失时工具返回安装提示。

mod binary;
mod cli;
mod commands;
mod cwd;
mod errors;
mod json_output;
mod pattern_hints;
mod process;
mod render;
mod result_formatter;
mod tools;
mod types;

use phi_ext::phi;

fn main() -> Result<(), phi::Error> {
    let mut ext = phi::Extension::new("phi-ast-grep", env!("CARGO_PKG_VERSION"));
    tools::register(&mut ext);
    commands::register(&mut ext);
    ext.run()
}