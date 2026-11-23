#!/bin/sh
# Build the browser's own decoders to WebAssembly and refresh the prebuilt
# copies the UI bundles, so building the UI needs no Rust toolchain:
#
#   crates/foxygnal-mpeg2 (MPEG-1/2 video)      → ui/src/lib/video/mpeg2.wasm
#   crates/foxygnal-mp2   (MPEG audio Layer I/II) → ui/src/lib/video/mp2.wasm
#   crates/foxygnal-ac3   (AC-3)                  → ui/src/lib/video/ac3.wasm
#
#   scripts/build-mpeg2-wasm.sh           rebuild and copy
#   scripts/build-mpeg2-wasm.sh --check   rebuild elsewhere and fail if the
#                                         committed modules differ (CI)
#
# The build is reproducible byte for byte: no paths of this machine in the
# binary, symbols stripped, fixed codegen flags, and the toolchain pinned in
# rust-toolchain.toml (official builds through rustup; a distro's rustc of
# the same version makes working modules that differ in a few bytes). With
# rustup, the pinned toolchain and its wasm32-unknown-unknown target are
# installed if missing. No other tools: no wasm-bindgen, no wasm-opt. The
# modules import nothing; this script checks that.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

check=false
if [ "${1:-}" = "--check" ]; then
  check=true
fi

channel=$(sed -n 's/^channel *= *"\(.*\)"/\1/p' rust-toolchain.toml)
if command -v rustup >/dev/null 2>&1; then
  rustup toolchain list | grep -q "^$channel" || rustup toolchain install --profile minimal "$channel"
  rustup target list --installed --toolchain "$channel" | grep -qx wasm32-unknown-unknown ||
    rustup target add --toolchain "$channel" wasm32-unknown-unknown
  cargo="rustup run $channel cargo"
else
  cargo=cargo
  echo "note: no rustup, so building with $(rustc --version), not the official $channel" >&2
  echo "      toolchain: the modules will work but may not match CI's --check" >&2
fi

out_dir="$root/ui/src/lib/video"
if $check; then
  out_dir=$(mktemp -d)
  trap 'rm -rf "$out_dir"' EXIT
fi

build() {
  crate=$1
  dest="$out_dir/$2"
  # Only these flags: none from the environment (CARGO_ENCODED_RUSTFLAGS
  # would override RUSTFLAGS) or a config file.
  env -u CARGO_ENCODED_RUSTFLAGS -u CARGO_BUILD_RUSTFLAGS \
    RUSTFLAGS="--remap-path-prefix=$root=. --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=cargo -C strip=symbols -C opt-level=3 -C codegen-units=1 -C panic=abort" \
    $cargo rustc --release --locked --target wasm32-unknown-unknown -p "$crate" --crate-type cdylib \
    --target-dir "$root/target/mpeg2-wasm"
  out="$root/target/mpeg2-wasm/wasm32-unknown-unknown/release/$(echo "$crate" | tr - _).wasm"
  mkdir -p "$(dirname "$dest")"
  cp "$out" "$dest"
  chmod 644 "$dest"

  # The import section (id 2) must be absent: the UI instantiates the
  # module with no imports.
  python3 - "$dest" <<'EOF'
import os, sys
b = open(sys.argv[1], 'rb').read()
name = os.path.basename(sys.argv[1])
assert b[:4] == b'\0asm', 'not a wasm module'
i = 8
def leb(i):
    v = s = 0
    while True:
        c = b[i]; i += 1
        v |= (c & 0x7f) << s; s += 7
        if c < 0x80:
            return v, i
while i < len(b):
    sid = b[i]
    n, i = leb(i + 1)
    if sid == 2:
        sys.exit(name + ' imports something; it must be self-contained')
    i += n
print(name + ': no imports')
EOF
  if $check; then
    if cmp -s "$dest" "$root/ui/src/lib/video/$2"; then
      echo "$2: matches the committed module"
    else
      echo "$2: differs from the committed module; run scripts/build-mpeg2-wasm.sh (with rustup) and commit it" >&2
      failed=true
    fi
  else
    ls -l "$dest"
  fi
}

failed=false
build foxygnal-mpeg2 mpeg2.wasm
build foxygnal-mp2 mp2.wasm
build foxygnal-ac3 ac3.wasm
if $failed; then
  exit 1
fi
