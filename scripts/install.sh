#!/usr/bin/env bash
# install.sh — 构建全部 phi 扩展并安装到 ~/.phi/extensions/<name>/。
#
# 用法：
#   scripts/install.sh            # 构建 release 并安装
#   scripts/install.sh --debug    # 构建 debug 并安装
#
# phi 的扩展发现规则（见 doc/extensions.md）：
#   ~/.phi/extensions/<name>/phi.yaml   —— 全局
#   <cwd>/.phi/extensions/<name>/phi.yaml —— 项目内（同名时项目优先）
# 安装后需在 TUI 里 Ctrl+K → extensions → reload，或重启 phi。

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROFILE="release"
CARGO_FLAGS=("--release")

if [[ "${1:-}" == "--debug" ]]; then
  PROFILE="debug"
  CARGO_FLAGS=()
fi

EXTENSIONS=(
  phi-asymptotic-thinking
  phi-rtk-optimizer
  phi-sleep-continue
  phi-cache-optimizer
)

PHI_HOME="${PHI_HOME:-$HOME/.phi}"
TARGET_ROOT="$PHI_HOME/extensions"

echo "==> 构建（profile: $PROFILE）"
cargo build --manifest-path "$ROOT/Cargo.toml" --workspace "${CARGO_FLAGS[@]}"

for name in "${EXTENSIONS[@]}"; do
  src="$ROOT/target/$PROFILE/$name"
  dest="$TARGET_ROOT/$name"
  if [[ ! -x "$src" ]]; then
    echo "!! 未找到可执行文件：$src" >&2
    exit 1
  fi
  mkdir -p "$dest"
  install -m 0755 "$src" "$dest/$name"
  install -m 0644 "$ROOT/crates/$name/phi.yaml" "$dest/phi.yaml"
  echo "==> 已安装 $name -> $dest"
done

echo
echo "完成。请在 phi 中执行 Ctrl+K → extensions → reload（或重启 phi）以加载新扩展。"
echo "提示：扩展自身的数据目录即 $TARGET_ROOT/<name>/，配置文件与状态文件都写在那里。"