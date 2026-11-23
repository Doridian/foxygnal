#!/bin/sh
# Export every example to GNU Radio and compile each .grc with grcc.
# Skips (exit 0) when grcc isn't installed. With GR_BLOCKS=<dir of *.block.yml>,
# also runs scripts/grc-check.py against those definitions.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT

cargo build -q -p foxygnal --manifest-path "$root/Cargo.toml"
fox="$root/target/debug/foxygnal"

for g in "$root"/examples/*.fox.toml; do
    name=$(basename "$g" .fox.toml)
    "$fox" export "$g" -o "$out/$name.grc" 2>/dev/null
done

if [ -n "${GR_BLOCKS:-}" ]; then
    python3 "$root/scripts/grc-check.py" "$GR_BLOCKS" "$out"/*.grc
fi

if ! command -v grcc >/dev/null 2>&1; then
    echo "grcc not found: skipping the GNU Radio compile check"
    exit 0
fi

status=0
for f in "$out"/*.grc; do
    if grcc -o "$out/py" "$f" >"$out/log" 2>&1; then
        echo "ok    $(basename "$f")"
    else
        echo "FAIL  $(basename "$f")"
        cat "$out/log"
        status=1
    fi
done
exit $status
