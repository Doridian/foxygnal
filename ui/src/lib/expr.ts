// Graph variables and parameter expressions, mirroring foxygnal-core's
// `expr` and `resolve::eval_vars` so the inspector can show resolved values
// as you type. The engine is the authority: it evaluates again when it builds.
//
// Numbers (with SI prefixes and unit words: `2.4M`, `100 kHz`), `$name`
// references, + - * /, unary minus and parentheses. Nothing else.

export type Scope = ReadonlyMap<string, number>;
export type Result = { value: number; error?: undefined } | { value?: undefined; error: string };

const PREFIX: Record<string, number> = { p: 1e-12, n: 1e-9, u: 1e-6, µ: 1e-6, m: 1e-3, k: 1e3, K: 1e3, M: 1e6, G: 1e9, T: 1e12 };
const LETTER = /\p{L}/u;
const IDENT = /^[A-Za-z0-9_]+/;
export const VAR_NAME = /^[A-Za-z_][A-Za-z0-9_]*$/;

type Expr = { num: number } | { ref: string } | { neg: Expr } | { op: '+' | '-' | '*' | '/'; a: Expr; b: Expr };

class Parser {
  pos = 0;
  depth = 0;
  constructor(readonly src: string) {}

  peek(): string | undefined {
    while (this.pos < this.src.length && /\s/.test(this.src[this.pos])) this.pos++;
    return this.src[this.pos];
  }

  expr(): Expr {
    let a = this.term();
    for (let c = this.peek(); c === '+' || c === '-'; c = this.peek()) {
      this.pos++;
      a = { op: c, a, b: this.term() };
    }
    return a;
  }

  term(): Expr {
    let a = this.unary();
    for (let c = this.peek(); c === '*' || c === '/'; c = this.peek()) {
      this.pos++;
      a = { op: c, a, b: this.unary() };
    }
    return a;
  }

  unary(): Expr {
    if (++this.depth > 64) throw new Error('expression is nested too deeply');
    const c = this.peek();
    let e: Expr;
    if (c === '-') {
      this.pos++;
      e = { neg: this.unary() };
    } else if (c === '+') {
      this.pos++;
      e = this.unary();
    } else e = this.atom();
    this.depth--;
    return e;
  }

  atom(): Expr {
    const c = this.peek();
    if (c === undefined) throw new Error('expected a value, found the end');
    if (c === '(') {
      this.pos++;
      const e = this.expr();
      if (this.peek() !== ')') throw new Error('missing `)`');
      this.pos++;
      return e;
    }
    if (c === '$') {
      this.pos++;
      const name = IDENT.exec(this.src.slice(this.pos))?.[0] ?? '';
      if (!name || /^\d/.test(name)) throw new Error('`$` must be followed by a variable name');
      this.pos += name.length;
      return { ref: name };
    }
    if (/[\d.]/.test(c)) return this.number();
    if (LETTER.test(c) || c === '_') {
      const word = /^[\p{L}\d_]+/u.exec(this.src.slice(this.pos))?.[0] ?? c;
      throw new Error(`\`${word}\` isn't a number; variables are written \`$${word}\``);
    }
    throw new Error(`unexpected \`${c}\``);
  }

  number(): Expr {
    const m = /^(?:[\d_]*(?:\.[\d_]*)?)(?:[eE][+-]?\d+)?/.exec(this.src.slice(this.pos))!;
    const text = m[0];
    const v0 = Number(text.replaceAll('_', ''));
    if (!text || Number.isNaN(v0) || text === '.') throw new Error(`\`${text || this.src[this.pos]}\` isn't a number`);
    this.pos += text.length;
    let v = v0;
    this.peek();
    // A unit word: letters, and `/` between letters (`kS/s`, not `k/2`).
    let end = this.pos;
    while (end < this.src.length) {
      const ch = this.src[end];
      if (LETTER.test(ch) || (ch === '/' && end > this.pos && LETTER.test(this.src[end + 1] ?? ''))) end++;
      else break;
    }
    if (end > this.pos) {
      const p = PREFIX[this.src[this.pos]];
      if (p !== undefined) v *= p;
      this.pos = end;
    }
    return { num: v };
  }
}

function parse(src: string): Expr {
  const p = new Parser(src);
  const e = p.expr();
  const c = p.peek();
  if (c === ')') throw new Error('unmatched `)`');
  if (c !== undefined) throw new Error(`unexpected \`${c}\``);
  return e;
}

function run(e: Expr, lookup: (name: string) => number): number {
  let v: number;
  if ('num' in e) v = e.num;
  else if ('ref' in e) v = lookup(e.ref);
  else if ('neg' in e) v = -run(e.neg, lookup);
  else {
    const a = run(e.a, lookup);
    const b = run(e.b, lookup);
    if (e.op === '/' && b === 0) throw new Error('division by zero');
    v = e.op === '+' ? a + b : e.op === '-' ? a - b : e.op === '*' ? a * b : a / b;
  }
  if (!Number.isFinite(v)) throw new Error('the result is too large');
  return v;
}

function refsOf(e: Expr, out: string[] = []): string[] {
  if ('ref' in e) out.push(e.ref);
  else if ('neg' in e) refsOf(e.neg, out);
  else if ('op' in e) {
    refsOf(e.a, out);
    refsOf(e.b, out);
  }
  return out;
}

/** Evaluate `text` with variables from `scope`. */
export function evaluate(text: string, scope: Scope): Result {
  try {
    return {
      value: run(parse(text), (name) => {
        const v = scope.get(name);
        if (v === undefined) throw new Error(`unknown variable \`$${name}\``);
        return v;
      }),
    };
  } catch (e) {
    return { error: (e as Error).message };
  }
}

/** A param or variable value as a number: a number, or an expression. */
export function evaluateValue(v: unknown, scope: Scope): Result {
  if (typeof v === 'number') return { value: v };
  if (typeof v === 'string') return evaluate(v, scope);
  return { error: `expected a number or an expression, got ${JSON.stringify(v)}` };
}

/** Whether a variable's value refers to other variables (derived, not a parameter of the graph). */
export function isDerived(v: unknown): boolean {
  if (typeof v !== 'string') return false;
  try {
    return refsOf(parse(v)).length > 0;
  } catch {
    return false;
  }
}

/** Evaluate `[vars]`: values of the good ones, messages for the rest. */
export function resolveVars(vars: Record<string, unknown>) {
  const values = new Map<string, number>();
  const errors = new Map<string, string>();
  const visiting: string[] = [];

  function get(name: string): number {
    const known = values.get(name);
    if (known !== undefined) return known;
    if (errors.has(name)) throw new Error(`\`$${name}\` has an error`);
    const i = visiting.indexOf(name);
    if (i >= 0) throw new Error(`depends on itself: ${[...visiting.slice(i), name].map((n) => `\`${n}\``).join(' → ')}`);
    visiting.push(name);
    try {
      const v = vars[name];
      let x: number;
      if (typeof v === 'number') x = v;
      else if (typeof v === 'string')
        x = run(parse(v), (dep) => {
          if (!(dep in vars)) throw new Error(`unknown variable \`$${dep}\``);
          return get(dep);
        });
      else throw new Error('must be a number or an expression');
      values.set(name, x);
      return x;
    } catch (e) {
      // The variable where it went wrong gets the message; its users a pointer.
      if (!errors.has(name)) errors.set(name, (e as Error).message);
      throw new Error(`\`$${name}\` has an error`);
    } finally {
      visiting.pop();
    }
  }

  for (const name of Object.keys(vars)) {
    try {
      get(name);
    } catch {
      // Recorded in `errors`.
    }
  }
  return { values, errors };
}

/** Replace `$from` with `$to` in an expression (for renaming a variable). */
export function renameRef(text: string, from: string, to: string): string {
  return text.replace(new RegExp(`\\$${from}(?![A-Za-z0-9_])`, 'g'), () => `$${to}`);
}
