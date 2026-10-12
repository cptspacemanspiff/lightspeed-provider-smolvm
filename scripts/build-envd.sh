#!/usr/bin/env bash
# Build a static lightspeed-envd for the provider to inject, and write a
# provider config that uses it with the example template's stock image.
#
# Environment:
#   LIGHTSPEED_DIR  Lightspeed checkout to build envd from (default: ../lightspeed)
#   PROFILE         release | debug (default: release)
#   OUT_DIR         where the binary and config go (default: out)
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
lightspeed="${LIGHTSPEED_DIR:-$repo/../lightspeed}"
profile="${PROFILE:-release}"
out_dir="${OUT_DIR:-$repo/out}"

die() { echo "build-envd.sh: $*" >&2; exit 1; }

[[ -f "$lightspeed/Cargo.toml" ]] || die "no Lightspeed checkout at $lightspeed (set LIGHTSPEED_DIR)"
# The guest is Linux on the host's architecture. Only x86_64 is supported.
arch="$(uname -m)"
[[ "$arch" == x86_64 ]] || die "only x86_64 guests are supported (host is $arch)"
target="$arch-unknown-linux-musl"

case "$profile" in
  release) cargo_profile=(--release); target_dir=release ;;
  debug) cargo_profile=(); target_dir=debug ;;
  *) die "PROFILE must be release or debug" ;;
esac

# aws-lc-sys compiles C, which must target musl too.
command -v musl-gcc >/dev/null \
  || die "needs musl-gcc to build C dependencies (Debian/Ubuntu: sudo apt install musl-tools)"

# Run from the checkout so its rust-toolchain.toml picks the toolchain.
cd "$lightspeed"
rustup target list --installed | grep -qx "$target" \
  || die "missing Rust target; run: (cd $lightspeed && rustup target add $target)"

# Explicit, so cc-rs never falls back to the host's glibc compiler.
export CC_x86_64_unknown_linux_musl=musl-gcc

echo "==> building static lightspeed-envd ($profile, $target) from $lightspeed"
cargo build --locked "${cargo_profile[@]}" --target "$target" -p environment-daemon
binary="$lightspeed/target/$target/$target_dir/lightspeed-envd"
[[ -x "$binary" ]] || die "expected envd at $binary"

mkdir -p "$out_dir"
sha="$(sha256sum "$binary" | cut -d' ' -f1)"
# Name by content, so a rebuild never changes a binary a config pins.
staged="$out_dir/lightspeed-envd-$arch-${sha:0:12}"
cp "$binary" "$staged.tmp"
chmod 0755 "$staged.tmp"
mv "$staged.tmp" "$staged"

config="$out_dir/provider.json"
python3 - "$repo/config.example.json" "$config" "$arch" "$staged" "$sha" <<'PY'
import json, sys
example, config, arch, path, sha = sys.argv[1:]
data = json.load(open(example))
data["envd"] = {"binaries": {arch: {"path": path, "sha256": sha}}}
# No credentials, so the machine gets no network.
for template in data["templates"]:
    template["credentials"] = []
json.dump(data, open(config, "w"), indent=2)
PY

cat <<EOF

Built $staged
sha256 $sha

Wrote $config. Run the provider with:

  cargo run -- --config $config
EOF
