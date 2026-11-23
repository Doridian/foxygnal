#!/bin/sh
# Build the MPEG-2 video decoder (crates/foxygnal-mpeg2) for the browser and
# refresh the prebuilt copy the UI bundles (ui/src/lib/video/mpeg2.wasm), so
# building the UI needs no Rust toolchain.
#
#   scripts/build-mpeg2-wasm.sh
#
# Needs the wasm32-unknown-unknown target (`rustup target add
# wasm32-unknown-unknown`). No other tools: no wasm-bindgen, no wasm-opt.
# The module imports nothing; this script checks that.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
# Reproducible: no paths of this machine in the binary, symbols stripped.
RUSTFLAGS="--remap-path-prefix=$root=. --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=cargo -C strip=symbols -C opt-level=3 -C codegen-units=1 -C panic=abort" \
  cargo rustc --release --target wasm32-unknown-unknown -p foxygnal-mpeg2 --crate-type cdylib \
  --target-dir "$root/target/mpeg2-wasm"
out="$root/target/mpeg2-wasm/wasm32-unknown-unknown/release/foxygnal_mpeg2.wasm"
dest="$root/ui/src/lib/video/mpeg2.wasm"
mkdir -p "$(dirname "$dest")"
cp "$out" "$dest"
chmod 644 "$dest"

# The import section (id 2) must be absent: the UI instantiates the module
# with no imports.
python3 - "$dest" <<'EOF'
import sys
b = open(sys.argv[1], 'rb').read()
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
        sys.exit('mpeg2.wasm imports something; it must be self-contained')
    i += n
print('mpeg2.wasm: no imports')
EOF
ls -l "$dest"
