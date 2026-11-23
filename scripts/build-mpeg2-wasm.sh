#!/bin/sh
# Build the browser's own decoders to WebAssembly and refresh the prebuilt
# copies the UI bundles, so building the UI needs no Rust toolchain:
#
#   crates/foxygnal-mpeg2 (MPEG-1/2 video)      → ui/src/lib/video/mpeg2.wasm
#   crates/foxygnal-mp2   (MPEG audio Layer I/II) → ui/src/lib/video/mp2.wasm
#
#   scripts/build-mpeg2-wasm.sh
#
# Needs the wasm32-unknown-unknown target (`rustup target add
# wasm32-unknown-unknown`). No other tools: no wasm-bindgen, no wasm-opt.
# The modules import nothing; this script checks that.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

build() {
  crate=$1
  dest="$root/ui/src/lib/video/$2"
  # Reproducible: no paths of this machine in the binary, symbols stripped.
  RUSTFLAGS="--remap-path-prefix=$root=. --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=cargo -C strip=symbols -C opt-level=3 -C codegen-units=1 -C panic=abort" \
    cargo rustc --release --target wasm32-unknown-unknown -p "$crate" --crate-type cdylib \
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
  ls -l "$dest"
}

build foxygnal-mpeg2 mpeg2.wasm
build foxygnal-mp2 mp2.wasm
