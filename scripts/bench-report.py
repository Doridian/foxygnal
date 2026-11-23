#!/usr/bin/env python3
"""Summaries and comparisons of criterion results (stdlib only).

    scripts/bench-report.py table [--baseline NAME]   current results (vs a saved baseline)
    scripts/bench-report.py export FILE.json [--baseline NAME]
                                                       save current (or saved) results as JSON
    scripts/bench-report.py diff OLD.json [NEW.json]   compare two exports (NEW: current)

Results are read from criterion's output directory: $CRITERION_HOME, else
$CARGO_TARGET_DIR/criterion, else target/criterion. Throughput is items per
second (criterion `Throughput::Elements`). Changes are reported, never
enforced: shared machines are too noisy for a hard threshold.
"""

import json
import os
import sys

# Changes smaller than this are shown without a marker.
NOISE = 0.05


def criterion_dir():
    if os.environ.get("CRITERION_HOME"):
        return os.environ["CRITERION_HOME"]
    return os.path.join(os.environ.get("CARGO_TARGET_DIR", "target"), "criterion")


def load(baseline="new"):
    """{full_id: {"ns": mean ns per iteration, "elements": items or None}}."""
    out = {}
    root = criterion_dir()
    for dirpath, _dirs, files in os.walk(root):
        if os.path.basename(dirpath) != baseline or "benchmark.json" not in files:
            continue
        try:
            with open(os.path.join(dirpath, "benchmark.json")) as f:
                bench = json.load(f)
            with open(os.path.join(dirpath, "estimates.json")) as f:
                est = json.load(f)
        except (OSError, ValueError):
            continue
        tp = bench.get("throughput") or {}
        out[bench["full_id"]] = {
            "ns": est["mean"]["point_estimate"],
            "elements": tp.get("Elements") or tp.get("Bytes"),
        }
    return out


def rate(r):
    """Items per second, or None without a throughput."""
    if not r or not r.get("elements"):
        return None
    return r["elements"] / (r["ns"] * 1e-9)


def fmt_rate(v):
    if v is None:
        return "–"
    for unit, k in (("G", 1e9), ("M", 1e6), ("k", 1e3)):
        if v >= k:
            return f"{v / k:.4g} {unit}/s"
    return f"{v:.3g} /s"


def fmt_time(ns):
    for unit, k in (("s", 1e9), ("ms", 1e6), ("µs", 1e3)):
        if ns >= k:
            return f"{ns / k:.3g} {unit}"
    return f"{ns:.3g} ns"


def change(old, new):
    """Relative speed change (+ is faster), from time per iteration."""
    if not old or not new:
        return None
    return old["ns"] / new["ns"] - 1.0


def fmt_change(c):
    if c is None:
        return "–"
    mark = ""
    if c > NOISE:
        mark = " ▲"
    elif c < -NOISE:
        mark = " ▼"
    return f"{c * 100:+.1f}%{mark}"


def table(new, old=None, old_label="baseline"):
    cols = ["benchmark", "throughput", "time"]
    if old is not None:
        cols += [f"{old_label}", "change"]
    rows = []
    for key in sorted(new):
        r = new[key]
        row = [key, fmt_rate(rate(r)), fmt_time(r["ns"])]
        if old is not None:
            o = old.get(key)
            row += [fmt_rate(rate(o)) if o else "–", fmt_change(change(o, r)) if o else "new"]
        rows.append(row)
    lines = ["| " + " | ".join(cols) + " |", "|" + "|".join("---" for _ in cols) + "|"]
    lines += ["| " + " | ".join(row) + " |" for row in rows]
    if old is not None:
        gone = sorted(set(old) - set(new))
        if gone:
            lines.append("")
            lines.append("Not run: " + ", ".join(gone))
        lines.append("")
        lines.append(
            f"Change is speed relative to {old_label} (+ faster). ▲/▼ mark moves beyond "
            f"±{NOISE * 100:.0f}%; on shared CI runners even those can be noise."
        )
    return "\n".join(lines)


def main(argv):
    if len(argv) < 2 or argv[1] in ("-h", "--help"):
        print(__doc__.strip())
        return 0
    cmd = argv[1]
    if cmd == "table":
        new = load()
        if not new:
            print(f"no criterion results under {criterion_dir()}", file=sys.stderr)
            return 1
        old = None
        label = "baseline"
        if "--baseline" in argv:
            label = argv[argv.index("--baseline") + 1]
            old = load(label)
        print(table(new, old, label))
    elif cmd == "export":
        new = load(argv[argv.index("--baseline") + 1] if "--baseline" in argv else "new")
        with open(argv[2], "w") as f:
            json.dump(new, f, indent=1, sort_keys=True)
            f.write("\n")
        print(f"wrote {len(new)} results to {argv[2]}")
    elif cmd == "diff":
        with open(argv[2]) as f:
            old = json.load(f)
        if len(argv) > 3:
            with open(argv[3]) as f:
                new = json.load(f)
        else:
            new = load()
        print(table(new, old, os.path.basename(argv[2])))
    else:
        print(f"unknown command `{cmd}`\n\n{__doc__.strip()}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
