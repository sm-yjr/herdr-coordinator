#!/usr/bin/env bash
set -euo pipefail

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
manifest="$root/herdr-plugin.toml"
version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$manifest" | head -1)

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) target="aarch64-apple-darwin" ;;
  Darwin-x86_64) target="x86_64-apple-darwin" ;;
  Linux-aarch64|Linux-arm64) target="aarch64-unknown-linux-gnu" ;;
  Linux-x86_64) target="x86_64-unknown-linux-gnu" ;;
  *) echo "不支持的平台: $(uname -s) $(uname -m)" >&2; exit 1 ;;
esac

asset="herdr-coordinator-${target}.tar.gz"
base="https://github.com/sm-yjr/herdr-coordinator/releases/download/v${version}"
tmp=$(mktemp -d)
cleanup() { rm -rf "$tmp"; }
trap cleanup EXIT INT TERM

verify_checksum() {
  expected=$(awk -v name="$asset" '$2 == name { print $1 }' "$tmp/checksums.txt")
  [ -n "$expected" ] || return 1
  if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$tmp/$asset" | awk '{print $1}')
  else
    actual=$(shasum -a 256 "$tmp/$asset" | awk '{print $1}')
  fi
  [ "$actual" = "$expected" ]
}

mkdir -p "$root/bin"
if command -v curl >/dev/null 2>&1 \
  && curl -fsSL "$base/$asset" -o "$tmp/$asset" \
  && curl -fsSL "$base/checksums.txt" -o "$tmp/checksums.txt" \
  && verify_checksum
then
  tar -xzf "$tmp/$asset" -C "$tmp"
  install -m 0755 "$tmp/herdr-coordinator" "$root/bin/herdr-coordinator"
  echo "installed prebuilt herdr-coordinator v${version} (${target})"
  exit 0
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "没有匹配的预编译产物，且本机未安装 cargo" >&2
  exit 1
fi

cargo build --locked --release --manifest-path "$root/tower/Cargo.toml"
install -m 0755 "$root/tower/target/release/herdr-coordinator" "$root/bin/herdr-coordinator"
echo "built herdr-coordinator v${version} from source"
