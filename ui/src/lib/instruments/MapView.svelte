<script lang="ts">
  // Map: positions from a decoder's text lines (ADS-B aircraft, AIS ships,
  // APRS stations, …), one track per id with a trail, a heading arrow and a
  // label, fading out as it ages. The base map is a built-in Natural Earth
  // world map (loaded with the first Map), or tiles from a URL the user sets.
  // Canvas 2D and our own Web Mercator, no map library. Loaded lazily.
  import { onMount } from 'svelte';
  import type { MapSettings } from '../instruments.svelte';
  import { destination, distance, latToY, lonToX, niceStep, TILE, tileUrl, yToLat } from '../map/mercator';
  import { heading, label, type Track, type TrackKind, type TrackStore } from '../map/tracks';
  import { loadWorld, type World } from '../map/world';

  let {
    settings,
    store,
    version,
    onclear,
    onchange,
  }: {
    settings: MapSettings;
    /** The panel's tracks (it feeds them, so nothing is lost while this loads). */
    store: TrackStore;
    /** Bumped whenever tracks change. */
    version: number;
    onclear: () => void;
    onchange: (patch: Partial<MapSettings>) => void;
  } = $props();

  const MIN_Z = 0;
  const MAX_Z = 18;
  const KIND_COLOURS: Record<TrackKind, string> = {
    aircraft: '#4cc2ff',
    ship: '#8bd450',
    station: '#f59e3b',
    object: '#ffd9a6',
    other: '#c48cff',
  };
  const KIND_NAMES: Record<TrackKind, string> = { aircraft: 'Aircraft', ship: 'Vessel', station: 'Station', object: 'Object', other: 'Track' };

  let wrap = $state<HTMLElement>();
  let canvas = $state<HTMLCanvasElement>();
  let w = 0;
  let h = 0;
  let world: World | null = null;
  let worldError = $state(false);
  /** View: centre in world units and zoom (the world is 256·2^z px wide). */
  let cx = 0.5;
  let cy = 0.5;
  let z = 1;
  /** Fit the view to the tracks as they move, until the user pans or zooms. */
  let follow = $state(true);
  let selected = $state<string | null>(null);
  /** Bumped every second, for ages and fading. */
  let tick = $state(0);
  let count = $state(0);

  const worldPx = () => TILE * 2 ** z;
  const units = $derived(settings.units === 'nm' ? { m: 1852, name: 'nm' } : { m: 1000, name: 'km' });
  const tileTemplate = $derived(settings.tiles ? settings.tileUrl.trim() : '');
  const tilesOn = $derived(!!tileTemplate && tileUrl(tileTemplate, 0, 0, 0) !== null);
  const attribution = $derived.by(() => {
    if (!tilesOn) return 'Natural Earth';
    try {
      const host = new URL(tileTemplate.replace(/\{[a-z]\}/g, '0')).hostname;
      return host.includes('openstreetmap') ? '© OpenStreetMap contributors' : `Tiles: ${host}`;
    } catch {
      return '';
    }
  });
  const rx = $derived(settings.rxLat !== null && settings.rxLon !== null ? { lat: settings.rxLat, lon: settings.rxLon } : null);

  // ---- Drawing --------------------------------------------------------------

  let frameRequested = false;
  function requestDraw() {
    if (frameRequested) return;
    frameRequested = true;
    requestAnimationFrame(() => {
      frameRequested = false;
      draw();
    });
  }

  /** Screen x for world x, using the copy of the world nearest the view's centre. */
  function sx(x: number, W: number): number {
    let d = x - cx;
    d -= Math.round(d);
    return w / 2 + d * W;
  }

  function draw() {
    const ctx = canvas?.getContext('2d');
    if (!ctx || !w || !h) return;
    const dpr = window.devicePixelRatio || 1;
    const W = worldPx();
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.fillStyle = '#0a1018';
    ctx.fillRect(0, 0, w, h);

    if (tilesOn) drawTiles(ctx, W);
    else if (world) drawWorld(ctx, dpr, W);
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);

    if (rx) drawRings(ctx, W);
    const now = Date.now();
    const expiry = Math.max(1, settings.expiry) * 1000;
    const tracks = [...store.positioned()];
    for (const t of tracks) drawTrail(ctx, t, W, alpha(now - t.seen, expiry));
    for (const t of tracks) drawMarker(ctx, t, W, alpha(now - t.seen, expiry));
    drawScale(ctx, W);
  }

  function alpha(age: number, expiry: number): number {
    return 1 - 0.75 * Math.max(0, Math.min(1, age / expiry));
  }

  function drawWorld(ctx: CanvasRenderingContext2D, dpr: number, W: number) {
    if (!world) return;
    const ox = w / 2 - cx * W;
    const oy = h / 2 - cy * W;
    // Copies of the world side by side, for panning across the date line.
    // (A little to the left too: a few islands reach past 180°.)
    const k0 = Math.floor(-ox / W - 0.05);
    const k1 = Math.floor((w - ox) / W);
    for (let k = k0; k <= k1; k++) {
      ctx.setTransform(dpr * W, 0, 0, dpr * W, dpr * (ox + k * W), dpr * oy);
      ctx.fillStyle = '#18212b';
      ctx.fill(world.land, 'evenodd');
      ctx.lineWidth = 1 / W;
      ctx.strokeStyle = '#3a4a5a';
      ctx.stroke(world.coast);
      ctx.setLineDash([3 / W, 3 / W]);
      ctx.strokeStyle = '#33404d';
      ctx.stroke(world.borders);
      ctx.setLineDash([]);
    }
  }

  // Tiles, cached; a missing one falls back to a scaled-up parent.
  const tiles = new Map<string, { img: HTMLImageElement; ok: boolean; failed: boolean }>();
  const MAX_TILES = 400;

  function tile(z: number, x: number, y: number): HTMLImageElement | null {
    const url = tileUrl(tileTemplate, z, x, y);
    if (!url) return null;
    let t = tiles.get(url);
    if (!t) {
      const img = new Image();
      const entry = { img, ok: false, failed: false };
      img.onload = () => {
        entry.ok = true;
        requestDraw();
      };
      img.onerror = () => (entry.failed = true);
      img.decoding = 'async';
      img.src = url;
      t = entry;
      tiles.set(url, t);
      if (tiles.size > MAX_TILES) {
        const first = tiles.keys().next().value;
        if (first !== undefined) tiles.delete(first);
      }
    } else {
      // Most recently used last.
      tiles.delete(url);
      tiles.set(url, t);
    }
    return t.ok ? t.img : null;
  }

  function drawTiles(ctx: CanvasRenderingContext2D, W: number) {
    const tz = Math.max(0, Math.min(19, Math.round(z)));
    const n = 2 ** tz;
    const size = W / n;
    const left = cx * W - w / 2;
    const top = cy * W - h / 2;
    const x0 = Math.floor(left / size);
    const x1 = Math.floor((left + w) / size);
    const y0 = Math.max(0, Math.floor(top / size));
    const y1 = Math.min(n - 1, Math.floor((top + h) / size));
    ctx.imageSmoothingEnabled = true;
    for (let ty = y0; ty <= y1; ty++) {
      for (let tx = x0; tx <= x1; tx++) {
        const px = tx * size - left;
        const py = ty * size - top;
        const wx = ((tx % n) + n) % n;
        const img = tile(tz, wx, ty);
        if (img) {
          ctx.drawImage(img, px, py, size + 0.5, size + 0.5);
          continue;
        }
        // A parent tile, cropped, while this one loads.
        for (let up = 1; up <= 3 && tz - up >= 0; up++) {
          const f = 2 ** up;
          const parent = tiles.get(tileUrl(tileTemplate, tz - up, Math.floor(wx / f), Math.floor(ty / f)) ?? '');
          if (parent?.ok) {
            const s = TILE / f;
            ctx.drawImage(parent.img, (wx % f) * s, (ty % f) * s, s, s, px, py, size + 0.5, size + 0.5);
            break;
          }
        }
      }
    }
  }

  function drawRings(ctx: CanvasRenderingContext2D, W: number) {
    if (!rx) return;
    // Metres across the view at the receiver's latitude.
    const across = (w / W) * 40075016.7 * Math.cos((rx.lat * Math.PI) / 180);
    const step = niceStep(across / 6 / units.m) * units.m;
    const rxX = sx(lonToX(rx.lon), W);
    const rxY = (latToY(rx.lat) - cy) * W + h / 2;
    ctx.lineWidth = 1;
    ctx.font = '10px var(--mono, monospace)';
    ctx.textAlign = 'left';
    ctx.textBaseline = 'bottom';
    for (let r = 1; r <= 5; r++) {
      const d = r * step;
      if (d > 1.9e7) break;
      ctx.beginPath();
      for (let b = 0; b <= 360; b += 4) {
        const p = destination(rx.lat, rx.lon, b, d);
        // Keep the ring on the receiver's copy of the world.
        let x = lonToX(p.lon);
        x += Math.round(lonToX(rx.lon) - x);
        const px = rxX + (x - lonToX(rx.lon)) * W;
        const py = (latToY(p.lat) - cy) * W + h / 2;
        if (b === 0) ctx.moveTo(px, py);
        else ctx.lineTo(px, py);
      }
      ctx.strokeStyle = 'rgba(245, 158, 59, 0.35)';
      ctx.stroke();
      // Labelled where the ring crosses the receiver's latitude to the east (panels are wide).
      const east = destination(rx.lat, rx.lon, 90, d);
      let ex = lonToX(east.lon);
      ex += Math.round(lonToX(rx.lon) - ex);
      ctx.fillStyle = 'rgba(245, 158, 59, 0.8)';
      ctx.fillText(fmtDist(d), rxX + (ex - lonToX(rx.lon)) * W + 3, (latToY(east.lat) - cy) * W + h / 2 - 2);
    }
    ctx.strokeStyle = '#f59e3b';
    ctx.lineWidth = 1.5;
    ctx.beginPath();
    ctx.moveTo(rxX - 6, rxY);
    ctx.lineTo(rxX + 6, rxY);
    ctx.moveTo(rxX, rxY - 6);
    ctx.lineTo(rxX, rxY + 6);
    ctx.stroke();
  }

  function fmtDist(m: number): string {
    const v = m / units.m;
    return `${v >= 10 ? Math.round(v) : Number(v.toPrecision(2))} ${units.name}`;
  }

  /** Screen position of a track's head. */
  function head(t: Track & { pos: { lat: number; lon: number } }, W: number): [number, number] {
    return [sx(lonToX(t.pos.lon), W), (latToY(t.pos.lat) - cy) * W + h / 2];
  }

  function drawTrail(ctx: CanvasRenderingContext2D, t: Track & { pos: { lat: number; lon: number } }, W: number, a: number) {
    if (t.trail.length < 2) return;
    const [hx] = head(t, W);
    const hxw = lonToX(t.pos.lon);
    ctx.beginPath();
    // Walk back from the head, unwrapping across the date line.
    let prevX = hxw;
    for (let i = t.trail.length - 1; i >= 0; i--) {
      const p = t.trail[i];
      let x = lonToX(p.lon);
      x += Math.round(prevX - x);
      prevX = x;
      const px = hx + (x - hxw) * W;
      const py = (latToY(p.lat) - cy) * W + h / 2;
      if (i === t.trail.length - 1) ctx.moveTo(px, py);
      else ctx.lineTo(px, py);
    }
    ctx.globalAlpha = a * 0.6;
    ctx.strokeStyle = KIND_COLOURS[t.kind];
    ctx.lineWidth = 1.5;
    ctx.lineJoin = 'round';
    ctx.stroke();
    ctx.globalAlpha = 1;
  }

  function drawMarker(ctx: CanvasRenderingContext2D, t: Track & { pos: { lat: number; lon: number } }, W: number, a: number) {
    const [x, y] = head(t, W);
    if (x < -50 || x > w + 50 || y < -50 || y > h + 50) return;
    const hdg = heading(t.fields);
    const colour = KIND_COLOURS[t.kind];
    ctx.globalAlpha = a;
    ctx.fillStyle = colour;
    ctx.strokeStyle = '#0a1018';
    ctx.lineWidth = 1.2;
    ctx.save();
    ctx.translate(x, y);
    ctx.beginPath();
    if (hdg === null) {
      ctx.arc(0, 0, 4.5, 0, Math.PI * 2);
    } else {
      ctx.rotate((hdg * Math.PI) / 180);
      if (t.kind === 'aircraft') {
        // A small plane, nose along the track.
        ctx.moveTo(0, -8);
        ctx.lineTo(1.6, -2);
        ctx.lineTo(8, 1.5);
        ctx.lineTo(8, 3);
        ctx.lineTo(1.6, 1.5);
        ctx.lineTo(1.2, 5.5);
        ctx.lineTo(3.5, 7.5);
        ctx.lineTo(-3.5, 7.5);
        ctx.lineTo(-1.2, 5.5);
        ctx.lineTo(-1.6, 1.5);
        ctx.lineTo(-8, 3);
        ctx.lineTo(-8, 1.5);
        ctx.lineTo(-1.6, -2);
      } else if (t.kind === 'ship') {
        // A hull, bow forward.
        ctx.moveTo(0, -8);
        ctx.lineTo(4, -2);
        ctx.lineTo(4, 7);
        ctx.lineTo(-4, 7);
        ctx.lineTo(-4, -2);
      } else {
        ctx.moveTo(0, -8);
        ctx.lineTo(5.5, 6);
        ctx.lineTo(0, 3);
        ctx.lineTo(-5.5, 6);
      }
      ctx.closePath();
    }
    ctx.fill();
    ctx.stroke();
    ctx.restore();
    if (t.key === selected) {
      ctx.strokeStyle = '#ffffff';
      ctx.lineWidth = 1.5;
      ctx.beginPath();
      ctx.arc(x, y, 12, 0, Math.PI * 2);
      ctx.stroke();
    }
    if (settings.labels) {
      ctx.font = '11px var(--mono, monospace)';
      ctx.textAlign = 'left';
      ctx.textBaseline = 'middle';
      ctx.lineWidth = 3;
      ctx.strokeStyle = 'rgba(10, 16, 24, 0.85)';
      const text = label(t);
      ctx.strokeText(text, x + 10, y);
      ctx.fillStyle = '#e6edf3';
      ctx.fillText(text, x + 10, y);
    }
    ctx.globalAlpha = 1;
  }

  function drawScale(ctx: CanvasRenderingContext2D, W: number) {
    const lat = yToLat(cy);
    const mPerPx = (40075016.7 * Math.cos((lat * Math.PI) / 180)) / W;
    const d = niceStep((mPerPx * 100) / units.m) * units.m;
    const len = d / mPerPx;
    const x = 10;
    const y = h - 12;
    ctx.strokeStyle = '#9aa7b4';
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(x, y - 4);
    ctx.lineTo(x, y);
    ctx.lineTo(x + len, y);
    ctx.lineTo(x + len, y - 4);
    ctx.stroke();
    ctx.fillStyle = '#9aa7b4';
    ctx.font = '10px var(--mono, monospace)';
    ctx.textAlign = 'left';
    ctx.textBaseline = 'bottom';
    ctx.fillText(fmtDist(d), x + 4, y - 3);
  }

  // ---- View -----------------------------------------------------------------

  function clampView() {
    z = Math.max(MIN_Z, Math.min(MAX_Z, z));
    cx -= Math.floor(cx);
    cy = Math.max(0, Math.min(1, cy));
  }

  /** Fit the view to every track (and the receiver). */
  function fit() {
    if (!w || !h) return;
    const xs: number[] = [];
    const ys: number[] = [];
    for (const t of store.positioned()) {
      xs.push(lonToX(t.pos.lon));
      ys.push(latToY(t.pos.lat));
    }
    if (rx) {
      xs.push(lonToX(rx.lon));
      ys.push(latToY(rx.lat));
    }
    if (!xs.length) {
      cx = 0.5;
      cy = 0.5;
      z = Math.log2(Math.max(w, h) / TILE);
    } else {
      const [x0, x1] = [Math.min(...xs), Math.max(...xs)];
      const [y0, y1] = [Math.min(...ys), Math.max(...ys)];
      cx = (x0 + x1) / 2;
      cy = (y0 + y1) / 2;
      const pad = 48;
      const span = Math.max((x1 - x0) / Math.max(1, w - 2 * pad), (y1 - y0) / Math.max(1, h - 2 * pad));
      z = span > 0 ? Math.min(11, Math.log2(1 / span / TILE)) : 9;
    }
    clampView();
    requestDraw();
  }

  function zoomAt(px: number, py: number, nz: number) {
    const W = worldPx();
    const wx = cx + (px - w / 2) / W;
    const wy = cy + (py - h / 2) / W;
    z = Math.max(MIN_Z, Math.min(MAX_Z, nz));
    const W2 = worldPx();
    cx = wx - (px - w / 2) / W2;
    cy = wy - (py - h / 2) / W2;
    clampView();
    requestDraw();
  }

  function panBy(dx: number, dy: number) {
    const W = worldPx();
    cx -= dx / W;
    cy -= dy / W;
    clampView();
    requestDraw();
  }

  function userMoved() {
    follow = false;
  }

  // ---- Pointer and keyboard -------------------------------------------------

  const pointers = new Map<number, { x: number; y: number }>();
  let dragged = 0;

  function local(e: PointerEvent | WheelEvent | MouseEvent): [number, number] {
    const r = canvas!.getBoundingClientRect();
    return [e.clientX - r.left, e.clientY - r.top];
  }

  function onpointerdown(e: PointerEvent) {
    canvas?.setPointerCapture(e.pointerId);
    const [x, y] = local(e);
    pointers.set(e.pointerId, { x, y });
    if (pointers.size === 1) dragged = 0;
  }

  function onpointermove(e: PointerEvent) {
    const p = pointers.get(e.pointerId);
    if (!p) return;
    const [x, y] = local(e);
    if (pointers.size === 1) {
      dragged += Math.abs(x - p.x) + Math.abs(y - p.y);
      if (dragged > 3) {
        panBy(x - p.x, y - p.y);
        userMoved();
      }
    } else if (pointers.size === 2) {
      const [a, b] = [...pointers.entries()];
      const other = a[0] === e.pointerId ? b[1] : a[1];
      const before = Math.hypot(p.x - other.x, p.y - other.y);
      const after = Math.hypot(x - other.x, y - other.y);
      const mx = (x + other.x) / 2;
      const my = (y + other.y) / 2;
      panBy((x - p.x) / 2, (y - p.y) / 2);
      if (before > 0 && after > 0) zoomAt(mx, my, z + Math.log2(after / before));
      dragged = Infinity;
      userMoved();
    }
    p.x = x;
    p.y = y;
  }

  function onpointerup(e: PointerEvent) {
    const was = pointers.size;
    pointers.delete(e.pointerId);
    if (was === 1 && dragged <= 3) pick(...local(e));
  }

  function pick(x: number, y: number) {
    const W = worldPx();
    let best: string | null = null;
    let bestD = 14;
    for (const t of store.positioned()) {
      const [tx, ty] = head(t, W);
      const d = Math.hypot(tx - x, ty - y);
      if (d < bestD) {
        bestD = d;
        best = t.key;
      }
    }
    selected = best;
    requestDraw();
  }

  function onwheel(e: WheelEvent) {
    e.preventDefault();
    const [x, y] = local(e);
    const lines = e.deltaMode === 1 ? e.deltaY * 16 : e.deltaMode === 2 ? e.deltaY * 400 : e.deltaY;
    zoomAt(x, y, z - lines / 300);
    userMoved();
  }

  function ondblclick(e: MouseEvent) {
    const [x, y] = local(e);
    zoomAt(x, y, z + (e.shiftKey ? -1 : 1));
    userMoved();
  }

  function onkeydown(e: KeyboardEvent) {
    const step = 60;
    const moves: Record<string, () => void> = {
      ArrowLeft: () => panBy(step, 0),
      ArrowRight: () => panBy(-step, 0),
      ArrowUp: () => panBy(0, step),
      ArrowDown: () => panBy(0, -step),
      '+': () => zoomAt(w / 2, h / 2, z + 1),
      '=': () => zoomAt(w / 2, h / 2, z + 1),
      '-': () => zoomAt(w / 2, h / 2, z - 1),
    };
    const f = moves[e.key];
    if (!f) return;
    e.preventDefault();
    f();
    userMoved();
  }

  // ---- Lifecycle ------------------------------------------------------------

  onMount(() => {
    loadWorld().then(
      (wd) => {
        world = wd;
        requestDraw();
      },
      () => (worldError = true),
    );
    const ro = new ResizeObserver(() => {
      if (!wrap || !canvas) return;
      const dpr = window.devicePixelRatio || 1;
      w = wrap.clientWidth;
      h = wrap.clientHeight;
      canvas.width = Math.max(1, Math.round(w * dpr));
      canvas.height = Math.max(1, Math.round(h * dpr));
      if (follow) fit();
      requestDraw();
    });
    if (wrap) ro.observe(wrap);
    const timer = setInterval(() => {
      if (store.expire(Date.now(), Math.max(1, settings.expiry) * 1000)) count = [...store.positioned()].length;
      tick++;
      requestDraw();
    }, 1000);
    return () => {
      ro.disconnect();
      clearInterval(timer);
    };
  });

  // New positions: refit (when following) and redraw.
  $effect(() => {
    void version;
    count = [...store.positioned()].length;
    if (follow) fit();
    requestDraw();
  });

  // Settings that change the picture.
  $effect(() => {
    void settings.labels;
    void tilesOn;
    void tileTemplate;
    void rx;
    void units;
    store.setTrailLength(Math.max(1, Math.round(settings.trail)));
    requestDraw();
  });

  // The receiver position moved: refit around it when following.
  $effect(() => {
    void rx;
    if (follow) fit();
  });

  // The picked track, copied so the card follows its changes.
  const sel = $derived.by(() => {
    void version;
    void tick;
    const t = selected ? store.tracks.get(selected) : undefined;
    if (!t) return null;
    return {
      kind: t.kind,
      label: label(t),
      pos: t.pos ? { ...t.pos } : null,
      age: Date.now() - t.seen,
      messages: t.messages,
      fields: Object.entries(t.fields).sort(([a], [b]) => a.localeCompare(b)),
    };
  });

  function fmtAge(ms: number): string {
    const s = Math.max(0, Math.round(ms / 1000));
    return s < 60 ? `${s} s` : s < 3600 ? `${Math.floor(s / 60)} min ${s % 60} s` : `${Math.floor(s / 3600)} h ${Math.floor((s % 3600) / 60)} min`;
  }

  function fmtCoord(v: number, pos: string, neg: string): string {
    return `${Math.abs(v).toFixed(5)}° ${v < 0 ? neg : pos}`;
  }
</script>

<div class="map">
  <div class="bar">
    <span class="count mono">{count} {count === 1 ? 'track' : 'tracks'}</span>
    {#if worldError}<span class="warn">World map failed to load</span>{/if}
    <span class="spacer"></span>
    <button type="button" class:on={follow} aria-pressed={follow} title="Keep every track in view" onclick={() => ((follow = true), fit())}>Fit</button>
    <button type="button" aria-label="Zoom in" onclick={() => (zoomAt(w / 2, h / 2, z + 1), userMoved())}>+</button>
    <button type="button" aria-label="Zoom out" onclick={() => (zoomAt(w / 2, h / 2, z - 1), userMoved())}>−</button>
    <button
      type="button"
      onclick={() => {
        selected = null;
        onclear();
      }}
      disabled={!count}>Clear</button
    >
  </div>
  <div class="area" bind:this={wrap}>
    <canvas
      bind:this={canvas}
      tabindex="0"
      aria-label="Map of decoded positions. Drag to pan, scroll or pinch to zoom, click a track for its details."
      {onpointerdown}
      {onpointermove}
      {onpointerup}
      onpointercancel={(e) => pointers.delete(e.pointerId)}
      {onwheel}
      {ondblclick}
      {onkeydown}
    ></canvas>
    <span class="attrib">{attribution}</span>
    {#if sel}
      <div class="card" role="dialog" aria-label="Track details">
        <div class="card-head">
          <span class="dot" style:background={KIND_COLOURS[sel.kind]}></span>
          <strong>{sel.label}</strong>
          <span class="kind">{KIND_NAMES[sel.kind]}</span>
          <span class="spacer"></span>
          <button type="button" class="x" aria-label="Close details" onclick={() => ((selected = null), requestDraw())}>×</button>
        </div>
        <div class="meta mono">
          {#if sel.pos}{fmtCoord(sel.pos.lat, 'N', 'S')} {fmtCoord(sel.pos.lon, 'E', 'W')}<br />{/if}
          {#if rx && sel.pos}{fmtDist(distance(rx.lat, rx.lon, sel.pos.lat, sel.pos.lon))} from the receiver<br />{/if}
          Last heard {fmtAge(sel.age)} ago · {sel.messages} msgs
        </div>
        <table class="mono">
          <tbody>
            {#each sel.fields as [k, v] (k)}
              <tr><th>{k}</th><td>{v}</td></tr>
            {/each}
          </tbody>
        </table>
      </div>
    {/if}
  </div>
</div>

<style>
  .map {
    flex: 1;
    min-width: 0;
    min-height: 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .bar {
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .spacer {
    flex: 1;
  }
  .count {
    font-size: 11px;
    color: var(--muted);
    white-space: nowrap;
  }
  .warn {
    font-size: 11px;
    color: var(--err);
  }
  button {
    padding: 3px 10px;
    border: 1px solid var(--line);
    border-radius: 6px;
    background: transparent;
    color: var(--text);
    font-size: 11px;
  }
  button.on {
    border-color: var(--accent);
    color: var(--accent);
  }
  button:disabled {
    opacity: 0.45;
  }
  .area {
    position: relative;
    flex: 1;
    min-height: 120px;
    border: 1px solid var(--line);
    border-radius: 4px;
    overflow: hidden;
  }
  canvas {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
    touch-action: none;
    cursor: grab;
    outline: none;
  }
  canvas:focus-visible {
    box-shadow: inset 0 0 0 2px var(--accent);
  }
  .attrib {
    position: absolute;
    right: 4px;
    bottom: 2px;
    font-size: 10px;
    color: var(--faint);
    background: rgba(10, 16, 24, 0.6);
    padding: 0 4px;
    border-radius: 3px;
    pointer-events: none;
  }
  .card {
    position: absolute;
    top: 8px;
    right: 8px;
    width: min(280px, calc(100% - 16px));
    max-height: calc(100% - 16px);
    overflow: auto;
    box-sizing: border-box;
    padding: 8px 10px;
    background: var(--panel-2);
    border: 1px solid var(--line);
    border-radius: 8px;
    box-shadow: 0 8px 24px rgba(0, 0, 0, 0.5);
    font-size: 12px;
  }
  .card-head {
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
  }
  .kind {
    font-size: 11px;
    color: var(--muted);
  }
  .x {
    padding: 0 6px;
    font-size: 14px;
    line-height: 1.2;
  }
  .meta {
    margin: 6px 0;
    font-size: 11px;
    color: var(--muted);
    line-height: 1.5;
  }
  table {
    width: 100%;
    border-collapse: collapse;
    font-size: 11px;
  }
  th {
    padding: 1px 8px 1px 0;
    text-align: left;
    font-weight: normal;
    color: var(--muted);
    vertical-align: top;
    white-space: nowrap;
  }
  td {
    padding: 1px 0;
    word-break: break-all;
  }
</style>
