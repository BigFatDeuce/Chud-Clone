// Chud Clone's visual universe: starry sky, celestial bodies (one per disk) and transfer beam.
// Everything is drawn locally (2D canvas and SVG): no external resource.
"use strict";

(function () {
  const TAU = Math.PI * 2;

  function hash(str) {
    let h = 2166136261;
    for (let i = 0; i < str.length; i++) {
      h ^= str.charCodeAt(i);
      h = Math.imul(h, 16777619);
    }
    return h >>> 0;
  }

  // ------------------------------------------------------------------ Ciel
  const sky = { canvas: null, ctx: null, stars: [], raf: 0, quality: "full", last: 0, shoot: null, nextShoot: 6 };

  function resizeSky() {
    const c = sky.canvas;
    const dpr = Math.min(window.devicePixelRatio || 1, 2);
    c.width = Math.floor(innerWidth * dpr);
    c.height = Math.floor(innerHeight * dpr);
    sky.ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    const n = Math.round((innerWidth * innerHeight) / 5200);
    sky.stars = Array.from({ length: n }, () => ({
      x: Math.random() * innerWidth,
      y: Math.random() * innerHeight,
      z: 0.2 + Math.random() * 0.8,
      r: 0.3 + Math.random() * 1.1,
      p: Math.random() * TAU,
      s: 0.4 + Math.random() * 1.6,
      c: Math.random() < 0.12 ? "190,170,255" : Math.random() < 0.2 ? "150,225,255" : "232,236,255",
    }));
    drawSky(0, 0);
  }

  function drawSky(t, dt) {
    const { ctx } = sky;
    ctx.clearRect(0, 0, innerWidth, innerHeight);
    for (const s of sky.stars) {
      s.x -= s.z * 2.2 * dt;
      if (s.x < -2) s.x = innerWidth + 2;
      const a = 0.25 + 0.55 * s.z * (0.65 + 0.35 * Math.sin(t * s.s + s.p));
      ctx.fillStyle = `rgba(${s.c},${a.toFixed(3)})`;
      ctx.beginPath();
      ctx.arc(s.x, s.y, s.r * s.z + 0.2, 0, TAU);
      ctx.fill();
    }
    if (sky.shoot) {
      const m = sky.shoot;
      m.life += dt;
      const k = m.life / m.dur;
      if (k >= 1) sky.shoot = null;
      else {
        const x = m.x + m.vx * m.life, y = m.y + m.vy * m.life;
        const g = ctx.createLinearGradient(x, y, x - m.vx * 0.18, y - m.vy * 0.18);
        g.addColorStop(0, `rgba(220,245,255,${(1 - k) * 0.9})`);
        g.addColorStop(1, "rgba(79,227,255,0)");
        ctx.strokeStyle = g;
        ctx.lineWidth = 1.4;
        ctx.beginPath();
        ctx.moveTo(x, y);
        ctx.lineTo(x - m.vx * 0.18, y - m.vy * 0.18);
        ctx.stroke();
      }
    }
  }

  function skyLoop(now) {
    sky.raf = requestAnimationFrame(skyLoop);
    const t = now / 1000;
    const dt = Math.min(0.1, t - sky.last);
    if (dt < 1 / 32) return; // ~30 frames/s is enough for a background
    sky.last = t;
    sky.nextShoot -= dt;
    if (sky.nextShoot <= 0 && !sky.shoot) {
      sky.nextShoot = 7 + Math.random() * 12;
      sky.shoot = { x: Math.random() * innerWidth * 0.8 + innerWidth * 0.2, y: Math.random() * innerHeight * 0.4, vx: -420 - Math.random() * 260, vy: 160 + Math.random() * 120, life: 0, dur: 0.9 };
    }
    drawSky(t, dt);
  }

  function applyQuality() {
    cancelAnimationFrame(sky.raf);
    sky.raf = 0;
    if (!sky.canvas) return;
    document.documentElement.classList.toggle("no-motion", sky.quality === "off");
    document.documentElement.classList.toggle("reduced-motion", sky.quality === "reduced");
    if (sky.quality === "off") {
      sky.ctx.clearRect(0, 0, innerWidth, innerHeight);
      return;
    }
    drawSky(0, 0);
    if (sky.quality === "full" && !document.hidden) sky.raf = requestAnimationFrame(skyLoop);
  }

  function startSky(canvas, quality) {
    sky.canvas = canvas;
    sky.ctx = canvas.getContext("2d");
    sky.quality = quality || "full";
    addEventListener("resize", resizeSky);
    document.addEventListener("visibilitychange", applyQuality);
    resizeSky();
    applyQuality();
  }

  function setQuality(q) {
    sky.quality = q;
    applyQuality();
  }

  // ---------------------------------------------------------------- Astres
  let uid = 0;

  /** Kind of celestial body according to the disk type. */
  function bodyKind(d) {
    if (d.kind === "image" || d.media === "virtual" || d.media === "image") return "holo";
    if (d.bus === "USB" || d.removable) return "moon";
    if (d.bus === "NVMe") return "star";
    if (d.media === "ssd") return "crystal";
    if (d.media === "hdd") return "ringed";
    return "rock";
  }

  /** Draws a disk's celestial body. The hue derives from its identity: a disk keeps its body. */
  function planet(d, opts) {
    opts = opts || {};
    const h = hash(d.fingerprint || d.id || d.model || "x");
    const hue = opts.hue != null ? opts.hue : h % 360;
    const id = "p" + ++uid;
    const kind = opts.kind || bodyKind(d);
    const light = `hsl(${hue} 85% 78%)`, mid = `hsl(${hue} 72% 55%)`, dark = `hsl(${(hue + 28) % 360} 60% 18%)`;
    const glow = `hsl(${hue} 90% 62%)`;
    let body = "";
    const defs = `
      <radialGradient id="${id}g" cx="36%" cy="30%" r="80%"><stop offset="0" stop-color="${light}"/><stop offset=".5" stop-color="${mid}"/><stop offset="1" stop-color="${dark}"/></radialGradient>
      <radialGradient id="${id}h" cx="50%" cy="50%" r="50%"><stop offset=".55" stop-color="${glow}" stop-opacity=".45"/><stop offset="1" stop-color="${glow}" stop-opacity="0"/></radialGradient>
      <clipPath id="${id}c"><circle cx="50" cy="50" r="32"/></clipPath>
      <linearGradient id="${id}s" x1="0" x2="1"><stop offset="0" stop-color="#000" stop-opacity="0"/><stop offset="1" stop-color="#02030a" stop-opacity=".6"/></linearGradient>`;
    const halo = `<circle cx="50" cy="50" r="48" fill="url(#${id}h)"/>`;
    const sphere = `<circle cx="50" cy="50" r="32" fill="url(#${id}g)"/>`;
    const shade = `<circle cx="50" cy="50" r="32" fill="url(#${id}s)"/>`;
    if (kind === "ringed") {
      // Ringed gas giant: the hard disk and its platters.
      const bands = [0, 1, 2, 3, 4].map((i) => `<rect x="14" y="${24 + i * 11 + (h >> i) % 4}" width="72" height="${3 + ((h >> (i + 3)) % 4)}" fill="#fff" opacity="${0.07 + ((h >> (i * 2)) % 5) * 0.03}"/>`).join("");
      body = `${halo}<g transform="rotate(-18 50 50)"><ellipse cx="50" cy="50" rx="47" ry="11" fill="none" stroke="${light}" stroke-opacity=".28" stroke-width="5"/></g>
        ${sphere}<g clip-path="url(#${id}c)">${bands}</g>${shade}
        <g transform="rotate(-18 50 50)"><path d="M3 50a47 11 0 0 0 94 0" fill="none" stroke="${light}" stroke-opacity=".75" stroke-width="2.4"/><path d="M8 50a42 8 0 0 0 84 0" fill="none" stroke="${light}" stroke-opacity=".3" stroke-width="1.6"/></g>`;
    } else if (kind === "crystal") {
      // Smooth, bright world: the SSD, no moving parts.
      body = `${halo}${sphere}<g clip-path="url(#${id}c)" opacity=".5"><path d="M22 60 50 18 78 58 50 84z" fill="#fff" opacity=".07"/><path d="M50 18 62 52 50 84 36 50z" fill="#fff" opacity=".09"/></g>${shade}
        <ellipse cx="40" cy="36" rx="9" ry="5" fill="#fff" opacity=".35" transform="rotate(-28 40 36)"/>`;
    } else if (kind === "star") {
      // Star: NVMe, the fastest.
      body = `<circle cx="50" cy="50" r="48" fill="url(#${id}h)"/><circle class="spin" cx="50" cy="50" r="40" fill="none" stroke="${light}" stroke-opacity=".5" stroke-width="1.2" stroke-dasharray="2 9"/>
        <circle class="spin" style="animation-direction:reverse;animation-duration:38s" cx="50" cy="50" r="44.5" fill="none" stroke="${glow}" stroke-opacity=".35" stroke-width=".9" stroke-dasharray="1 14"/>
        <circle cx="50" cy="50" r="27" fill="url(#${id}g)"/><circle cx="50" cy="50" r="16" fill="#fff" opacity=".55"/><circle cx="50" cy="50" r="27" fill="none" stroke="#fff" stroke-opacity=".5" stroke-width=".8"/>`;
    } else if (kind === "moon") {
      // Moon: the external disk, passing through.
      body = `<circle cx="50" cy="50" r="42" fill="url(#${id}h)" opacity=".6"/><circle cx="50" cy="50" r="29" fill="hsl(${hue} 14% 62%)"/><circle cx="50" cy="50" r="29" fill="url(#${id}g)" opacity=".28"/>
        <g fill="#0a0d20" opacity=".28"><circle cx="40" cy="42" r="6"/><circle cx="60" cy="58" r="4.5"/><circle cx="56" cy="36" r="2.8"/><circle cx="42" cy="62" r="2.2"/></g>
        <circle cx="50" cy="50" r="29" fill="url(#${id}s)"/>`;
    } else if (kind === "holo") {
      // Hologram: the image file, a disk without matter.
      body = `<circle cx="50" cy="50" r="46" fill="url(#${id}h)" opacity=".7"/><g fill="none" stroke="${light}" stroke-width="1.1">
        <circle cx="50" cy="50" r="32" stroke-opacity=".9"/><ellipse cx="50" cy="50" rx="32" ry="11" stroke-opacity=".55"/><ellipse cx="50" cy="50" rx="32" ry="22" stroke-opacity=".35"/>
        <g class="spin" style="animation-duration:24s"><ellipse cx="50" cy="50" rx="12" ry="32" stroke-opacity=".6" stroke-dasharray="3 3"/><ellipse cx="50" cy="50" rx="24" ry="32" stroke-opacity=".35" stroke-dasharray="3 3"/></g></g>
        <circle cx="50" cy="50" r="32" fill="${glow}" opacity=".08"/>`;
    } else {
      body = `${halo}${sphere}<g clip-path="url(#${id}c)" fill="#0a0d20" opacity=".22"><path d="M20 44q14-10 26 0t30-4v14q-18 8-30 0t-26 6z"/><path d="M28 66q12-6 22 0t24-2v10H28z"/></g>${shade}`;
    }
    return `<svg viewBox="0 0 100 100" aria-hidden="true"><defs>${defs}</defs>${body}</svg>`;
  }

  // ---------------------------------------------------------------------- Beam
  /** Particle stream between the source body and the destination body. */
  class Beam {
    constructor(canvas) {
      this.c = canvas;
      this.ctx = canvas.getContext("2d");
      this.parts = [];
      this.rings = [];
      this.mode = "idle";
      this.intensity = 0.3;
      this.errors = false;
      this.raf = 0;
      this.last = 0;
      this.acc = 0;
      this.t = 0;
      this.onResize = () => this.resize();
      addEventListener("resize", this.onResize);
      this.resize();
      this.loop = this.loop.bind(this);
      this.raf = requestAnimationFrame(this.loop);
    }
    destroy() {
      cancelAnimationFrame(this.raf);
      removeEventListener("resize", this.onResize);
    }
    resize() {
      const r = this.c.getBoundingClientRect();
      const dpr = Math.min(window.devicePixelRatio || 1, 2);
      this.w = r.width;
      this.h = r.height;
      this.c.width = Math.max(1, Math.floor(r.width * dpr));
      this.c.height = Math.max(1, Math.floor(r.height * dpr));
      this.ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      const cy = this.h * 0.5 - this.h * 0.08 - 18;
      this.a = { x: this.w * 0.07 + 75, y: cy };
      this.b = { x: this.w * 0.93 - 75, y: cy };
    }
    /** Point on the path (two arcs passing above and below the central ring). */
    point(t, lane) {
      const a = this.a, b = this.b;
      const mx = (a.x + b.x) / 2, bend = 118 * lane;
      const x = (1 - t) * (1 - t) * a.x + 2 * (1 - t) * t * mx + t * t * b.x;
      const y = (1 - t) * (1 - t) * a.y + 2 * (1 - t) * t * (a.y - bend) + t * t * b.y;
      return [x, y];
    }
    set(mode, intensity, errors) {
      if (mode !== this.mode && mode === "table") this.rings.push({ r: 20, a: 1 }, { r: 2, a: 1 });
      if (mode !== this.mode && (mode === "ok" || mode === "warn")) for (let i = 0; i < 4; i++) this.rings.push({ r: 10 - i * 26, a: 1 });
      this.mode = mode;
      if (intensity != null) this.intensity = intensity;
      this.errors = !!errors;
    }
    spawn(back) {
      const lane = (Math.random() < 0.5 ? 1 : -1) * (0.75 + Math.random() * 0.5);
      this.parts.push({ t: 0, v: 0.32 + Math.random() * 0.3 + this.intensity * 0.5, lane, r: 0.9 + Math.random() * 1.9, back, amber: this.errors && Math.random() < 0.3 });
    }
    loop(now) {
      this.raf = requestAnimationFrame(this.loop);
      const t = now / 1000;
      const dt = Math.min(0.05, t - this.last || 0.016);
      this.last = t;
      this.t = t;
      const { ctx, mode } = this;
      if (document.hidden || !this.w) return;
      ctx.clearRect(0, 0, this.w, this.h);
      const still = document.documentElement.classList.contains("no-motion");

      // Particle rate according to the phase.
      let rate = 0, back = false;
      if (mode === "copy") rate = 26 + 150 * this.intensity;
      else if (mode === "recover") rate = 10;
      else if (mode === "verify") { rate = 46; back = true; }
      else if (mode === "prepare" || mode === "table" || mode === "check") rate = 6;
      else if (mode === "ok") rate = 12;
      if (still) rate = 0;
      this.acc += rate * dt;
      while (this.acc >= 1) { this.acc -= 1; this.spawn(back && Math.random() < 0.8); }

      // Trajectoires de fond.
      const dead = mode === "fail" || mode === "incomplete";
      for (const lane of [1, -1]) {
        ctx.beginPath();
        for (let i = 0; i <= 40; i++) {
          const k = i / 40;
          if (dead && k > 0.44 && k < 0.56) { ctx.moveTo(...this.point(0.56, lane)); continue; }
          const [x, y] = this.point(k, lane);
          i === 0 ? ctx.moveTo(x, y) : ctx.lineTo(x, y);
        }
        const g = ctx.createLinearGradient(this.a.x, 0, this.b.x, 0);
        if (dead) { g.addColorStop(0, "rgba(255,79,130,.5)"); g.addColorStop(1, "rgba(255,79,130,.15)"); }
        else if (mode === "warn") { g.addColorStop(0, "rgba(255,197,107,.55)"); g.addColorStop(1, "rgba(255,197,107,.35)"); }
        else if (mode === "ok") { g.addColorStop(0, "rgba(84,240,182,.6)"); g.addColorStop(1, "rgba(79,227,255,.6)"); }
        else { g.addColorStop(0, "rgba(79,227,255,.22)"); g.addColorStop(1, "rgba(154,107,255,.22)"); }
        ctx.strokeStyle = g;
        ctx.lineWidth = mode === "ok" || mode === "warn" ? 1.8 : 1;
        ctx.setLineDash(dead ? [4, 7] : []);
        ctx.stroke();
        ctx.setLineDash([]);
      }

      // Particules.
      ctx.globalCompositeOperation = "lighter";
      const keep = [];
      for (const p of this.parts) {
        p.t += p.v * dt;
        if (p.t >= 1) continue;
        keep.push(p);
        const k = p.back ? 1 - p.t : p.t;
        const [x, y] = this.point(k, p.lane);
        const fade = Math.sin(Math.min(1, p.t) * Math.PI);
        let col;
        if (p.amber || mode === "recover") col = `255,197,107`;
        else if (p.back) col = `210,245,255`;
        else if (mode === "ok") col = `84,240,182`;
        else col = `${Math.round(79 + (154 - 79) * k)},${Math.round(227 + (107 - 227) * k)},255`;
        const r = p.r * (1 + this.intensity * 0.6);
        const g = ctx.createRadialGradient(x, y, 0, x, y, r * 4.5);
        g.addColorStop(0, `rgba(${col},${0.85 * fade})`);
        g.addColorStop(1, `rgba(${col},0)`);
        ctx.fillStyle = g;
        ctx.beginPath();
        ctx.arc(x, y, r * 4.5, 0, TAU);
        ctx.fill();
      }
      this.parts = keep;

      // Waves on the destination (table writing, end of the operation).
      const rk = [];
      for (const r of this.rings) {
        r.r += 60 * dt;
        r.a -= 0.55 * dt;
        if (r.a <= 0) continue;
        rk.push(r);
        if (r.r < 0) continue;
        ctx.strokeStyle = mode === "warn" ? `rgba(255,197,107,${r.a})` : `rgba(120,235,255,${r.a})`;
        ctx.lineWidth = 1.6;
        ctx.beginPath();
        ctx.arc(this.b.x, this.b.y, 58 + r.r, 0, TAU);
        ctx.stroke();
      }
      this.rings = rk;

      // Sweep over the destination during verification.
      if ((mode === "verify" || mode === "check") && !still) {
        const ang = t * 2.4;
        ctx.strokeStyle = "rgba(160,240,255,.85)";
        ctx.lineWidth = 2.2;
        ctx.beginPath();
        ctx.arc(this.b.x, this.b.y, 66, ang, ang + 1.25);
        ctx.stroke();
        ctx.strokeStyle = "rgba(160,240,255,.3)";
        ctx.beginPath();
        ctx.arc(this.b.x, this.b.y, 73, -ang * 0.7, -ang * 0.7 + 0.8);
        ctx.stroke();
      }
      // Pulse of the source while re-reading difficult areas.
      if (mode === "recover" && !still) {
        const k = (t * 1.4) % 1;
        ctx.strokeStyle = `rgba(255,197,107,${1 - k})`;
        ctx.lineWidth = 1.6;
        ctx.beginPath();
        ctx.arc(this.a.x, this.a.y, 58 + k * 30, 0, TAU);
        ctx.stroke();
      }
      ctx.globalCompositeOperation = "source-over";
    }
  }

  // ---------------------------------------------------------------------- Star
  const clamp01 = (x) => Math.max(0, Math.min(1, x));
  const easeOut = (x) => 1 - Math.pow(1 - clamp01(x), 3);
  const easeInOut = (x) => { x = clamp01(x); return x * x * (3 - 2 * x); };
  /** Colour along a list of [position, [r, g, b]] stops. */
  function ramp(stops, k) {
    k = clamp01(k);
    for (let i = 1; i < stops.length; i++) {
      if (k <= stops[i][0]) {
        const [a, ca] = stops[i - 1], [b, cb] = stops[i];
        const u = (k - a) / (b - a || 1);
        return ca.map((v, j) => Math.round(v + (cb[j] - v) * u));
      }
    }
    return stops[stops.length - 1][1];
  }
  // A star that burns its fuel: blue-white, then yellow, orange, and a swollen red giant.
  const BURN = [[0, [190, 225, 255]], [0.35, [255, 244, 214]], [0.6, [255, 214, 120]], [0.85, [255, 150, 70]], [1, [255, 92, 62]]];
  // What is left after the explosion cools down.
  const COOL = [[0, [235, 245, 255]], [0.4, [255, 230, 160]], [0.75, [255, 140, 80]], [1, [176, 78, 62]]];

  /**
   * Life of a star around the percentage of a clone. Stages, set by the interface:
   *   "proto"    preparation: a small young star;
   *   "grow"     copy: it grows and reddens with the progress `k`, 0 → 1;
   *   "remnant"  copy complete: it explodes, then the core cools with `k` (verification);
   *   "dwarf"    good result: the core settles into a brown dwarf;
   *   "dead"     failed or incomplete: it goes out.
   */
  class Star {
    constructor(canvas) {
      this.c = canvas;
      this.ctx = canvas.getContext("2d");
      const dpr = Math.min(window.devicePixelRatio || 1, 2);
      this.size = canvas.getBoundingClientRect().width || 340;
      canvas.width = canvas.height = Math.max(1, Math.floor(this.size * dpr));
      this.ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      this.stage = "proto";
      this.k = 0;
      this.errors = false;
      this.r = 14;
      this.novaAt = -1;
      this.novaR = 0;
      this.endAt = -1;
      this.ejecta = [];
      this.last = 0;
      this.loop = this.loop.bind(this);
      this.raf = requestAnimationFrame(this.loop);
    }
    destroy() {
      cancelAnimationFrame(this.raf);
    }
    set(stage, k, errors) {
      const now = performance.now() / 1000;
      if (stage !== this.stage) {
        const alive = this.stage === "proto" || this.stage === "grow";
        if (alive && (stage === "remnant" || stage === "dwarf")) {
          this.novaAt = now;
          this.novaR = this.r;
          for (let i = 0; i < 70; i++) this.ejecta.push({ a: Math.random() * TAU, v: 60 + Math.random() * 190, r: 0.8 + Math.random() * 2.2, hue: Math.random() });
        }
        if (stage === "dwarf" || stage === "dead") this.endAt = now;
        this.stage = stage;
      }
      if (k != null) this.k = clamp01(k);
      this.errors = !!errors;
    }
    loop(nowMs) {
      this.raf = requestAnimationFrame(this.loop);
      if (document.hidden) return;
      const t = nowMs / 1000;
      const dt = Math.min(0.05, t - this.last || 0.016);
      this.last = t;
      this.draw(t, dt, document.documentElement.classList.contains("no-motion"));
    }
    glow(x, y, r, col, a) {
      if (a <= 0 || r <= 0) return;
      const { ctx } = this;
      const g = ctx.createRadialGradient(x, y, 0, x, y, r);
      g.addColorStop(0, `rgba(${col},${a})`);
      g.addColorStop(1, `rgba(${col},0)`);
      ctx.fillStyle = g;
      ctx.beginPath();
      ctx.arc(x, y, r, 0, TAU);
      ctx.fill();
    }
    draw(t, dt, still) {
      const { ctx, stage, k } = this;
      const c = this.size / 2;
      ctx.clearRect(0, 0, this.size, this.size);
      // Without motion every transition is shown in its final state.
      const na = this.novaAt < 0 ? -1 : still ? 99 : t - this.novaAt;
      const ea = this.endAt < 0 ? -1 : still ? 99 : t - this.endAt;
      const wave = still ? 0 : t;
      ctx.globalCompositeOperation = "lighter";

      // ---- The living star (and its last light when the clone fails during the copy).
      if (this.novaAt < 0) {
        const target = stage === "proto" ? 15 : stage === "grow" ? 20 + 50 * easeInOut(k) + 7 * clamp01((k - 0.94) / 0.06) : this.r;
        this.r = still ? target : this.r + (target - this.r) * Math.min(1, dt * 3.5);
        const alpha = stage === "dead" ? clamp01(1 - ea / 1.4) : 1;
        if (alpha > 0) {
          const heat = stage === "proto" ? 0 : k;
          let col = ramp(BURN, heat);
          if (this.errors) {
            const m = 0.35 + 0.25 * Math.sin(wave * 6);
            col = col.map((v, i) => Math.round(v + ([255, 197, 107][i] - v) * m));
          }
          const rr = this.r * (1 + (0.012 + 0.035 * heat * heat) * Math.sin(wave * (1.6 + 3.2 * heat)));
          // Corona.
          const cg = ctx.createRadialGradient(c, c, rr * 0.85, c, c, rr * 1.95);
          cg.addColorStop(0, `rgba(${col},${0.4 * alpha})`);
          cg.addColorStop(1, `rgba(${col},0)`);
          ctx.fillStyle = cg;
          ctx.beginPath();
          ctx.arc(c, c, rr * 1.95, 0, TAU);
          ctx.fill();
          // Body: a bright limb, a translucent heart so the percentage stays readable.
          const bg = ctx.createRadialGradient(c, c, 0, c, c, rr);
          bg.addColorStop(0, `rgba(${col},${0.1 * alpha})`);
          bg.addColorStop(0.55, `rgba(${col},${0.17 * alpha})`);
          bg.addColorStop(0.86, `rgba(${col},${0.55 * alpha})`);
          bg.addColorStop(0.97, `rgba(${col},${0.95 * alpha})`);
          bg.addColorStop(1, `rgba(${col},0)`);
          ctx.fillStyle = bg;
          ctx.beginPath();
          ctx.arc(c, c, rr, 0, TAU);
          ctx.fill();
          // Prominences on the limb, larger as the star ages.
          for (let i = 0; i < 5; i++) {
            const ang = (i * TAU) / 5 + wave * 0.15 + Math.sin(wave * 0.7 + i) * 0.2;
            const len = rr * (0.1 + 0.1 * (0.5 + 0.5 * Math.sin(wave * 1.3 + i * 2.1))) * (0.6 + heat);
            this.glow(c + Math.cos(ang) * (rr + len * 0.4), c + Math.sin(ang) * (rr + len * 0.4), len * 1.3, col, 0.5 * alpha);
          }
        }
      }

      // ---- The explosion: flash, shock waves, ejected matter.
      if (na >= 0 && na < 1.9) {
        const fr = this.novaR + (165 - this.novaR) * easeOut(na / 0.5);
        this.glow(c, c, fr, "255,240,215", clamp01(1 - na / 0.75));
        for (const [delay, col] of [[0, "190,235,255"], [0.16, "170,130,255"]]) {
          const u = na - delay;
          if (u <= 0) continue;
          const a = clamp01(1 - u / 1.5);
          ctx.strokeStyle = `rgba(${col},${a})`;
          ctx.lineWidth = 1 + 3 * a;
          ctx.beginPath();
          ctx.arc(c, c, this.novaR + 150 * easeOut(u / 1.5), 0, TAU);
          ctx.stroke();
        }
        const fade = clamp01(1 - na / 1.9);
        for (const p of this.ejecta) {
          const d = this.novaR * 0.6 + p.v * easeOut(na / 1.9);
          const col = p.hue < 0.4 ? "120,235,255" : p.hue < 0.75 ? "170,130,255" : "255,197,107";
          this.glow(c + Math.cos(p.a) * d, c + Math.sin(p.a) * d, p.r * 3.2, col, 0.8 * fade);
        }
      } else if (na >= 1.9 && this.ejecta.length) this.ejecta = [];

      if (this.novaAt >= 0) {
        // ---- The nebula left behind: it thins out while the clone is verified.
        const born = clamp01((na - 0.25) / 1.0);
        let neb = born * (0.5 - 0.28 * (stage === "remnant" ? k : 1));
        if (stage === "dwarf") neb *= 1 - 0.55 * easeOut(ea / 1.6);
        if (stage === "dead") neb *= clamp01(1 - ea / 1.2);
        if (neb > 0.01) {
          const shells = [["154,107,255", 62], ["79,227,255", 72], ["255,120,170", 82]];
          shells.forEach(([col, base], i) => {
            const R = base + 3 * Math.sin(wave * 0.4 + i * 1.7);
            const x = c + Math.cos(wave * 0.1 + i * 2) * 5, y = c + Math.sin(wave * 0.13 + i * 2) * 5;
            const g = ctx.createRadialGradient(x, y, R * 0.5, x, y, R * 1.25);
            g.addColorStop(0, `rgba(${col},0)`);
            g.addColorStop(0.55, `rgba(${col},${neb})`);
            g.addColorStop(1, `rgba(${col},0)`);
            ctx.fillStyle = g;
            ctx.beginPath();
            ctx.arc(x, y, R * 1.25, 0, TAU);
            ctx.fill();
          });
        }
        // ---- The core: white-hot after the explosion, then cooling.
        if (stage === "remnant" || (stage === "dwarf" && ea < 1.6)) {
          const heat = stage === "remnant" ? k : 1;
          const col = ramp(COOL, heat);
          const rc = 12 - 2 * heat;
          const a = clamp01(na / 0.5) * (stage === "dwarf" ? clamp01(1 - ea / 1.6) : 1);
          this.glow(c, c, rc * 3.2, col, 0.5 * (1 - 0.5 * heat) * a);
          this.glow(c, c, rc * 1.25, col, 0.95 * a);
        }
      }

      ctx.globalCompositeOperation = "source-over";
      // ---- The brown dwarf: a dim, banded body that no longer shines by itself.
      if (stage === "dwarf") {
        const b = easeOut(ea / 1.6);
        const r = 10 + 20 * b;
        const tint = this.errors ? "255,197,107" : "205,98,72";
        ctx.globalCompositeOperation = "lighter";
        const hg = ctx.createRadialGradient(c, c, r * 0.9, c, c, r * 2.2);
        hg.addColorStop(0, `rgba(${tint},${0.3 * b})`);
        hg.addColorStop(1, `rgba(${tint},0)`);
        ctx.fillStyle = hg;
        ctx.beginPath();
        ctx.arc(c, c, r * 2.2, 0, TAU);
        ctx.fill();
        ctx.globalCompositeOperation = "source-over";
        ctx.save();
        ctx.globalAlpha = b;
        ctx.beginPath();
        ctx.arc(c, c, r, 0, TAU);
        ctx.clip();
        const sg = ctx.createRadialGradient(c - r * 0.3, c - r * 0.35, r * 0.1, c, c, r * 1.05);
        sg.addColorStop(0, "#bd664c");
        sg.addColorStop(0.5, "#7a3a32");
        sg.addColorStop(1, "#2a1420");
        ctx.fillStyle = sg;
        ctx.fillRect(c - r, c - r, 2 * r, 2 * r);
        // Cloud bands, drifting slowly.
        [-0.58, -0.22, 0.1, 0.42, 0.7].forEach((y, i) => {
          const dy = Math.sin(wave * 0.25 + i * 1.9) * r * 0.015;
          ctx.fillStyle = i % 2 ? "rgba(236,160,118,.2)" : "rgba(38,14,24,.36)";
          ctx.fillRect(c - r, c + y * r + dy - r * 0.07, 2 * r, r * (i % 2 ? 0.1 : 0.15));
        });
        const shade = ctx.createRadialGradient(c - r * 0.35, c - r * 0.4, r * 0.4, c, c, r * 1.1);
        shade.addColorStop(0, "rgba(5,6,13,0)");
        shade.addColorStop(1, "rgba(5,6,13,.62)");
        ctx.fillStyle = shade;
        ctx.fillRect(c - r, c - r, 2 * r, 2 * r);
        ctx.restore();
      }
      // ---- Failure: nothing left but a cold cinder.
      if (stage === "dead") {
        const a = clamp01(ea / 1.4);
        ctx.fillStyle = `rgba(13,11,20,${a})`;
        ctx.beginPath();
        ctx.arc(c, c, 16, 0, TAU);
        ctx.fill();
        ctx.strokeStyle = `rgba(255,79,130,${0.6 * a})`;
        ctx.lineWidth = 1.4;
        ctx.setLineDash([3, 5]);
        ctx.stroke();
        ctx.setLineDash([]);
      }
    }
  }

  window.Cosmos = { startSky, setQuality, planet, bodyKind, Beam, Star, hash };
})();
