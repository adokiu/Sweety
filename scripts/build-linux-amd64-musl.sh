#!/usr/bin/env bash
# Linux x86_64 静态链接（musl），默认保留 jemalloc。不用 Docker / cross-rs。
#
# 一次性依赖:
#   brew install zig
#   cargo install cargo-zigbuild
#   rustup target add x86_64-unknown-linux-musl
#
# 用法:
#   ./scripts/build-linux-amd64-musl.sh
#   ./scripts/build-linux-amd64-musl.sh --features http3   # 追加 feature
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

rustup target add x86_64-unknown-linux-musl

# default = ["jemalloc"]；显式带上，避免误用 --no-default-features
cargo zigbuild --release --target x86_64-unknown-linux-musl --features jemalloc "$@"

OUT="$ROOT/target/x86_64-unknown-linux-musl/release/sweety"
echo ""
echo "完成（musl + jemalloc）: $OUT"
echo "校验: file \"$OUT\""
