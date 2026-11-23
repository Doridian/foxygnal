#!/usr/bin/env python3
"""Check exported .grc files against GNU Radio block definitions.

    scripts/grc-check.py BLOCKS_DIR file.grc...

BLOCKS_DIR holds GNU Radio's `*.block.yml` files (an installed GNU Radio has
them in /usr/share/gnuradio/grc/blocks; the gnuradio repository's
`gr-*/grc/` and `grc/blocks/` directories, maint-3.10 branch, work too).
The definitions are read as data (PyYAML), nothing is executed. For every
block: the id must be defined, every parameter must exist, enum values must
be among the options, and connections must use existing stream ports of
matching types. Exits 1 on any problem. `grcc` (when installed) checks more:
see scripts/grcc-check.sh.
"""
import glob
import os
import re
import sys

import yaml

BUILTIN = {
    # grc/core/blocks: defined in Python, not YAML.
    'epy_block': None,
    'options': None,
}
COMMON = {'alias', 'comment', 'affinity', 'minoutbuf', 'maxoutbuf'}


def load_defs(d):
    idx = {}
    for p in glob.glob(os.path.join(d, '**', '*.block.yml'), recursive=True):
        try:
            with open(p) as f:
                y = yaml.safe_load(f)
        except Exception:
            continue
        if isinstance(y, dict) and 'id' in y:
            idx[y['id']] = y
    return idx


def stream_ports(defn, params, side):
    """Expand a block's stream ports: [(dtype or None)]."""
    out = []
    for p in defn.get(side, []):
        if p.get('domain', 'stream') != 'stream':
            continue
        dtype = str(p.get('dtype', ''))
        m = re.fullmatch(r'\$\{\s*(\w+)(?:\.(\w+))?\s*\}', dtype)
        if m:
            val = params.get(m.group(1))
            dtype = None
            if m.group(2) is None:
                dtype = val
            else:
                for q in defn.get('parameters', []):
                    if q['id'] == m.group(1) and 'option_attributes' in q:
                        opts = [str(o) for o in q.get('options', [])]
                        attrs = q['option_attributes'].get(m.group(2))
                        if val in opts and attrs:
                            dtype = str(attrs[opts.index(val)])
        mult = p.get('multiplicity', 1)
        if isinstance(mult, str):
            m = re.fullmatch(r'\$\{\s*(\w+)\s*\}', mult)
            try:
                mult = int(params.get(m.group(1), 1)) if m else 1
            except ValueError:
                mult = 1
        out.extend([dtype] * int(mult))
    return out


def check(path, defs):
    errors = []
    with open(path) as f:
        doc = yaml.safe_load(f)
    if doc.get('metadata', {}).get('file_format') != 1:
        errors.append('metadata.file_format must be 1')
    names = {}
    for b in doc.get('blocks', []):
        name, bid = b.get('name'), b.get('id')
        if not re.fullmatch(r'[A-Za-z]\w*', str(name)):
            errors.append(f'{name}: not a valid id')
        if name in names:
            errors.append(f'{name}: duplicate name')
        params = {str(k): str(v) for k, v in (b.get('parameters') or {}).items()}
        st = b.get('states', {})
        if not (isinstance(st.get('coordinate'), list) and len(st['coordinate']) == 2):
            errors.append(f'{name}: states.coordinate missing')
        if bid in BUILTIN:
            names[name] = (bid, None, params)
            continue
        defn = defs.get(bid)
        if defn is None:
            errors.append(f'{name}: unknown block id `{bid}`')
            continue
        names[name] = (bid, defn, params)
        known = {p['id']: p for p in defn.get('parameters', [])}
        for k, v in params.items():
            if k in COMMON:
                continue
            if k not in known:
                errors.append(f'{name} ({bid}): unknown parameter `{k}`')
                continue
            q = known[k]
            if q.get('dtype') == 'enum' and 'options' in q:
                opts = [str(o) for o in q['options']]
                if v not in opts:
                    errors.append(f'{name} ({bid}): `{k}` = {v!r} not in {opts}')
        for k, q in known.items():
            if k not in params and 'default' not in q and q.get('dtype') not in ('enum',):
                if not str(q.get('hide', '')).startswith('$'):
                    errors.append(f'{name} ({bid}): required parameter `{k}` missing')
    for c in doc.get('connections', []):
        if len(c) != 4:
            errors.append(f'connection {c}: not [src, port, dst, port]')
            continue
        src, sp, dst, dp = map(str, c)
        if src not in names or dst not in names:
            errors.append(f'connection {c}: unknown block')
            continue
        sb, db = names[src], names[dst]
        if sb[1] is None or db[1] is None:
            continue  # epy blocks: ports come from the source code
        outs = stream_ports(sb[1], sb[2], 'outputs')
        ins = stream_ports(db[1], db[2], 'inputs')
        if not sp.isdigit() or int(sp) >= len(outs):
            errors.append(f'connection {c}: {src} has no output {sp} ({len(outs)})')
            continue
        if not dp.isdigit() or int(dp) >= len(ins):
            errors.append(f'connection {c}: {dst} has no input {dp} ({len(ins)})')
            continue
        to, ti = outs[int(sp)], ins[int(dp)]
        if to and ti and to != ti and {to, ti} != {'fc32', 'complex'}:
            errors.append(f'connection {c}: type {to} → {ti}')
    return errors


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        return 2
    defs = load_defs(sys.argv[1])
    bad = 0
    for path in sys.argv[2:]:
        errs = check(path, defs)
        for e in errs:
            print(f'{path}: {e}')
        bad += bool(errs)
        if not errs:
            print(f'{path}: ok')
    return 1 if bad else 0


if __name__ == '__main__':
    sys.exit(main())
