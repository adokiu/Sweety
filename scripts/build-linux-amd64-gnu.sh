#!/usr/bin/env bash
# 在 macOS 上交叉编译 Linux x86_64（GNU/glibc），不依赖 Docker / cross。
#
# 依赖（一次性）:
#   brew install zig
#   cargo install cargo-zigbuild
#   rustup target add x86_64-unknown-linux-gnu
#
# 用法:
#   ./scripts/build-linux-amd64-gnu.sh
#   ./scripts/build-linux-amd64-gnu.sh --no-default-features   # 跳过 jemalloc，交叉更省事
set -euo pipefail

export PATH="${HOME}/.cargo/bin:${PATH}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

if ! command -v zig >/dev/null 2>&1; then
  echo "未找到 zig，请执行: brew install zig" >&2
  exit 1
fi

if ! command -v cargo-zigbuild >/dev/null 2>&1; then
  echo "未找到 cargo-zigbuild（需 ${HOME}/.cargo/bin 在 PATH 中）" >&2
  echo "请执行: cargo install cargo-zigbuild" >&2
  exit 1
fi

rustup target add x86_64-unknown-linux-gnu

extra=()
if [[ "${1:-}" == "--no-default-features" ]]; then
  extra+=(--no-default-features)
  shift
fi

cargo zigbuild --release --target x86_64-unknown-linux-gnu "${extra[@]}" "$@"

OUT="$ROOT/target/x86_64-unknown-linux-gnu/release/sweety"
echo ""
echo "完成: $OUT"
echo "校验: file \"$OUT\""
