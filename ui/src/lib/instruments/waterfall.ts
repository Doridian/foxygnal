// WebGL2 waterfall. Rows of dB values go into a float texture used as a ring,
// so a new row costs one small upload, and changing the dB range re-colours
// the whole history without re-sending anything.

const VERT = `#version 300 es
in vec2 pos;
out vec2 uv;
void main() {
  uv = pos * 0.5 + 0.5;
  gl_Position = vec4(pos, 0.0, 1.0);
}`;

const FRAG = `#version 300 es
precision highp float;
in vec2 uv;
out vec4 colour;
uniform sampler2D rows;
uniform sampler2D lut;
uniform float newest;   // texture row of the newest line, normalized
uniform float filled;   // fraction of rows that hold data
uniform float ref;      // dB at the top of the colour scale
uniform float range;    // dB span of the colour scale
void main() {
  float age = 1.0 - uv.y;  // newest at the top
  if (age > filled) { colour = vec4(0.04, 0.05, 0.07, 1.0); return; }
  float row = fract(newest - age + 1.0);
  float db = texture(rows, vec2(uv.x, row)).r;
  float t = clamp((db - (ref - range)) / range, 0.0, 1.0);
  colour = texture(lut, vec2(t, 0.5));
}`;

/** Dark blue → purple → orange → pale yellow; monotonic in lightness. */
const STOPS: [number, number, number, number][] = [
  [0.0, 10, 13, 17],
  [0.2, 32, 22, 84],
  [0.45, 120, 28, 109],
  [0.65, 210, 70, 50],
  [0.85, 245, 158, 59],
  [1.0, 255, 246, 200],
];

function lutData(): Uint8Array {
  const out = new Uint8Array(256 * 4);
  for (let i = 0; i < 256; i++) {
    const t = i / 255;
    let k = 0;
    while (k < STOPS.length - 2 && t > STOPS[k + 1][0]) k++;
    const [t0, ...a] = STOPS[k];
    const [t1, ...b] = STOPS[k + 1];
    const f = (t - t0) / (t1 - t0);
    for (let c = 0; c < 3; c++) out[i * 4 + c] = Math.round(a[c] + (b[c] - a[c]) * f);
    out[i * 4 + 3] = 255;
  }
  return out;
}

export class Waterfall {
  #gl: WebGL2RenderingContext;
  #prog: WebGLProgram;
  #rows: WebGLTexture;
  #lut: WebGLTexture;
  #width = 0;
  #height: number;
  #next = 0;
  #count = 0;
  #u: Record<string, WebGLUniformLocation | null> = {};

  /** Throws if WebGL2 isn't available. */
  constructor(canvas: HTMLCanvasElement, history = 512) {
    const gl = canvas.getContext('webgl2', { antialias: false, premultipliedAlpha: false });
    if (!gl) throw new Error('WebGL2 is not available');
    this.#gl = gl;
    this.#height = history;
    this.#prog = program(gl, VERT, FRAG);
    gl.useProgram(this.#prog);
    for (const n of ['rows', 'lut', 'newest', 'filled', 'ref', 'range']) this.#u[n] = gl.getUniformLocation(this.#prog, n);

    const buf = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, buf);
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 1, -1, -1, 1, 1, 1]), gl.STATIC_DRAW);
    const loc = gl.getAttribLocation(this.#prog, 'pos');
    gl.enableVertexAttribArray(loc);
    gl.vertexAttribPointer(loc, 2, gl.FLOAT, false, 0, 0);

    this.#lut = gl.createTexture()!;
    gl.activeTexture(gl.TEXTURE1);
    gl.bindTexture(gl.TEXTURE_2D, this.#lut);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA8, 256, 1, 0, gl.RGBA, gl.UNSIGNED_BYTE, lutData());
    texParams(gl, gl.LINEAR);
    gl.uniform1i(this.#u.lut, 1);

    this.#rows = gl.createTexture()!;
    gl.uniform1i(this.#u.rows, 0);
  }

  /** Add the newest row (dB per bin). The bin count may change; history then restarts. */
  push(row: Float32Array) {
    const gl = this.#gl;
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, this.#rows);
    if (row.length !== this.#width) {
      this.#width = row.length;
      this.#next = 0;
      this.#count = 0;
      // R16F is filterable in WebGL2 and plenty for dB values.
      // Unwritten rows read as "nothing there", not 0 dB, or filtering would
      // draw a bright line where the history ends.
      const empty = new Float32Array(this.#width * this.#height).fill(-300);
      gl.pixelStorei(gl.UNPACK_ALIGNMENT, 4);
      gl.texImage2D(gl.TEXTURE_2D, 0, gl.R16F, this.#width, this.#height, 0, gl.RED, gl.FLOAT, empty);
      texParams(gl, gl.LINEAR);
    }
    gl.pixelStorei(gl.UNPACK_ALIGNMENT, 4);
    gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, this.#next, this.#width, 1, gl.RED, gl.FLOAT, row);
    this.#next = (this.#next + 1) % this.#height;
    this.#count = Math.min(this.#height, this.#count + 1);
  }

  draw(ref: number, range: number) {
    const gl = this.#gl;
    gl.viewport(0, 0, gl.drawingBufferWidth, gl.drawingBufferHeight);
    gl.clearColor(0.04, 0.05, 0.07, 1);
    gl.clear(gl.COLOR_BUFFER_BIT);
    if (!this.#width) return;
    gl.useProgram(this.#prog);
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, this.#rows);
    // Sample row centres: the newest row is `next - 1`.
    gl.uniform1f(this.#u.newest, (this.#next - 0.5) / this.#height);
    gl.uniform1f(this.#u.filled, this.#count / this.#height);
    gl.uniform1f(this.#u.ref, ref);
    gl.uniform1f(this.#u.range, Math.max(1, range));
    gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
  }

  destroy() {
    const gl = this.#gl;
    gl.deleteTexture(this.#rows);
    gl.deleteTexture(this.#lut);
    gl.deleteProgram(this.#prog);
  }
}

function texParams(gl: WebGL2RenderingContext, filter: number) {
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, filter);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, filter);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
  // Rows wrap around: the ring's oldest row sits next to its newest.
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.REPEAT);
}

function program(gl: WebGL2RenderingContext, vs: string, fs: string): WebGLProgram {
  const p = gl.createProgram()!;
  for (const [type, src] of [
    [gl.VERTEX_SHADER, vs],
    [gl.FRAGMENT_SHADER, fs],
  ] as const) {
    const s = gl.createShader(type)!;
    gl.shaderSource(s, src);
    gl.compileShader(s);
    if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(s) ?? 'shader error');
    gl.attachShader(p, s);
  }
  gl.linkProgram(p);
  if (!gl.getProgramParameter(p, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(p) ?? 'link error');
  return p;
}
