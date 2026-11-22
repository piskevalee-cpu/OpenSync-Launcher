// Smooth 60fps HTML5 Canvas throughput waveform for OpenSync Launcher.
// Animates rolling speed_samples with spline interpolation, translucent gradient fill,
// and smooth color/amplitude transitions between active (#7dfa9b) and paused (#ffd166).

const SAMPLE_COUNT = 56;
const GREEN_RGB = [125, 250, 155];
const AMBER_RGB = [255, 209, 102];

function lerp(a, b, t) {
  return a + (b - a) * t;
}

function formatAxisSpeed(bps) {
  const mb = bps / (1024 * 1024);
  if (mb >= 100) return `${Math.round(mb)} MB/s`;
  if (mb >= 10) return `${mb.toFixed(0)} MB/s`;
  if (mb >= 1) return `${mb.toFixed(1)} MB/s`;
  const kb = bps / 1024;
  if (kb >= 1) return `${Math.round(kb)} KB/s`;
  return '0 MB/s';
}

export class WaveformCanvas {
  constructor(canvas, initialSamples = [], paused = false) {
    this.canvas = canvas;
    this.ctx = canvas.getContext('2d');
    this.paused = Boolean(paused);
    this.completed = false;
    this.target = new Array(SAMPLE_COUNT).fill(0);
    this.display = new Array(SAMPLE_COUNT).fill(0);
    this.displayMax = 20 * 1024 * 1024; // 20 MB/s floor so axis stays calm
    this.pauseBlend = this.paused ? 1 : 0;
    this.pulsePhase = 0;
    this.rafId = null;
    this.lastFrame = performance.now();
    this.destroyed = false;

    this.width = 0;
    this.height = 0;
    this.dpr = window.devicePixelRatio || 1;

    this.update(initialSamples, paused, false);
    // Snap initial display so opening Downloads doesn't pop from zero
    for (let i = 0; i < SAMPLE_COUNT; i++) {
      this.display[i] = this.target[i];
    }
    const peak = Math.max(...this.target, 10 * 1024 * 1024);
    this.displayMax = peak * 1.18;

    this._resize();
    this.resizeObserver = new ResizeObserver(() => this._resize());
    this.resizeObserver.observe(this.canvas);
    this._loop(performance.now());
  }

  _resize() {
    if (this.destroyed) return;
    const rect = this.canvas.getBoundingClientRect();
    if (!rect.width || !rect.height) return;
    this.dpr = Math.min(window.devicePixelRatio || 1, 2);
    this.width = rect.width;
    this.height = rect.height;
    this.canvas.width = Math.round(rect.width * this.dpr);
    this.canvas.height = Math.round(rect.height * this.dpr);
  }

  update(samples = [], paused = false, completed = false) {
    this.paused = Boolean(paused);
    this.completed = Boolean(completed);
    const values = Array.isArray(samples)
      ? samples.map((s) => (typeof s === 'number' ? s : Number(s?.bytes_per_sec || 0)))
      : [];

    if (this.paused) {
      // Smoothly flatten the live tail toward zero when paused
      for (let i = 0; i < SAMPLE_COUNT; i++) {
        this.target[i] = 0;
      }
      return;
    }

    const tail = values.slice(-SAMPLE_COUNT);
    const pad = SAMPLE_COUNT - tail.length;
    for (let i = 0; i < SAMPLE_COUNT; i++) {
      this.target[i] = i < pad ? 0 : Math.max(0, tail[i - pad]);
    }
  }

  _loop(now) {
    if (this.destroyed) return;
    const dt = Math.min((now - this.lastFrame) / 1000, 0.08);
    this.lastFrame = now;

    // Smooth exponential interpolation (frame-rate independent)
    const smooth = 1 - Math.exp(-dt * 9.5);
    const colorSmooth = 1 - Math.exp(-dt * 7.0);
    const scaleSmooth = 1 - Math.exp(-dt * 4.5);

    this.pauseBlend = lerp(this.pauseBlend, this.paused ? 1 : 0, colorSmooth);
    this.pulsePhase += dt * 3.8;

    let maxVal = 0;
    for (let i = 0; i < SAMPLE_COUNT; i++) {
      this.display[i] = lerp(this.display[i], this.target[i], smooth);
      if (this.display[i] > maxVal) maxVal = this.display[i];
    }

    const desiredMax = Math.max(maxVal * 1.22, 8 * 1024 * 1024);
    this.displayMax = lerp(this.displayMax, desiredMax, scaleSmooth);

    this._draw();
    this.rafId = requestAnimationFrame((t) => this._loop(t));
  }

  _draw() {
    const { ctx, width: w, height: h, dpr } = this;
    if (!w || !h) return;

    ctx.save();
    ctx.scale(dpr, dpr);
    ctx.clearRect(0, 0, w, h);

    const padTop = 14;
    const padBottom = 10;
    const padLeft = 2;
    const padRight = 10;
    const plotW = Math.max(1, w - padLeft - padRight);
    const plotH = Math.max(1, h - padTop - padBottom);

    // Subtle horizontal gridlines
    ctx.strokeStyle = 'rgba(255, 255, 255, 0.05)';
    ctx.lineWidth = 1;
    ctx.setLineDash([3, 5]);
    for (let g = 0; g < 3; g++) {
      const gy = Math.round(padTop + (plotH * g) / 2) + 0.5;
      ctx.beginPath();
      ctx.moveTo(padLeft, gy);
      ctx.lineTo(w - padRight, gy);
      ctx.stroke();
    }
    ctx.setLineDash([]);

    // Blended active/paused stroke & fill color
    const r = Math.round(lerp(GREEN_RGB[0], AMBER_RGB[0], this.pauseBlend));
    const g = Math.round(lerp(GREEN_RGB[1], AMBER_RGB[1], this.pauseBlend));
    const b = Math.round(lerp(GREEN_RGB[2], AMBER_RGB[2], this.pauseBlend));

    // Compute canvas points
    const pts = new Array(SAMPLE_COUNT);
    const baseY = padTop + plotH;
    for (let i = 0; i < SAMPLE_COUNT; i++) {
      const x = padLeft + (i / (SAMPLE_COUNT - 1)) * plotW;
      const ratio = Math.min(1, Math.max(0, this.display[i] / this.displayMax));
      // Keep a tiny 1.5px baseline lift when paused so the amber line stays crisp
      const y = baseY - ratio * plotH - (this.pauseBlend * 1.5);
      pts[i] = { x, y };
    }

    // Build smooth midpoint quadratic spline
    const traceSpline = () => {
      ctx.beginPath();
      ctx.moveTo(pts[0].x, pts[0].y);
      for (let i = 1; i < pts.length - 1; i++) {
        const xc = (pts[i].x + pts[i + 1].x) * 0.5;
        const yc = (pts[i].y + pts[i + 1].y) * 0.5;
        ctx.quadraticCurveTo(pts[i].x, pts[i].y, xc, yc);
      }
      const last = pts[pts.length - 1];
      ctx.lineTo(last.x, last.y);
    };

    // Translucent gradient fill beneath curve
    const grad = ctx.createLinearGradient(0, padTop, 0, baseY);
    grad.addColorStop(0, `rgba(${r}, ${g}, ${b}, ${lerp(0.28, 0.14, this.pauseBlend)})`);
    grad.addColorStop(0.65, `rgba(${r}, ${g}, ${b}, ${lerp(0.08, 0.03, this.pauseBlend)})`);
    grad.addColorStop(1, `rgba(${r}, ${g}, ${b}, 0)`);

    traceSpline();
    ctx.lineTo(pts[pts.length - 1].x, baseY);
    ctx.lineTo(pts[0].x, baseY);
    ctx.closePath();
    ctx.fillStyle = grad;
    ctx.fill();

    // Glowing two-pass spline stroke (avoids slow per-frame canvas shadowBlur in WebKitGTK)
    ctx.save();
    ctx.lineJoin = 'round';
    ctx.lineCap = 'round';
    traceSpline();
    ctx.strokeStyle = `rgba(${r}, ${g}, ${b}, ${lerp(0.22, 0.12, this.pauseBlend)})`;
    ctx.lineWidth = 6;
    ctx.stroke();

    traceSpline();
    ctx.strokeStyle = `rgb(${r}, ${g}, ${b})`;
    ctx.lineWidth = 2;
    ctx.stroke();
    ctx.restore();

    // Leading telemetry dot
    const tip = pts[pts.length - 1];
    if (!this.completed) {
      const pulse = 0.5 + 0.5 * Math.sin(this.pulsePhase);
      if (!this.paused) {
        ctx.beginPath();
        ctx.arc(tip.x, tip.y, 4.5 + pulse * 3.5, 0, Math.PI * 2);
        ctx.fillStyle = `rgba(${r}, ${g}, ${b}, ${(0.28 - pulse * 0.18).toFixed(3)})`;
        ctx.fill();
      }
      ctx.beginPath();
      ctx.arc(tip.x, tip.y, 3.2, 0, Math.PI * 2);
      ctx.fillStyle = `rgb(${r}, ${g}, ${b})`;
      ctx.fill();
    }

    // Subtle peak scale label in top-right corner
    ctx.font = '600 10px Inter, -apple-system, sans-serif';
    ctx.fillStyle = 'rgba(143, 143, 154, 0.72)';
    ctx.textAlign = 'right';
    ctx.textBaseline = 'top';
    ctx.fillText(
      this.paused ? 'Paused' : `Peak ${formatAxisSpeed(this.displayMax)}`,
      w - padRight,
      2,
    );

    ctx.restore();
  }

  destroy() {
    this.destroyed = true;
    if (this.rafId !== null) {
      cancelAnimationFrame(this.rafId);
      this.rafId = null;
    }
    if (this.resizeObserver) {
      this.resizeObserver.disconnect();
      this.resizeObserver = null;
    }
  }
}
