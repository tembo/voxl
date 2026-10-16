// Usage: node measure.mjs <url> <seconds> [label] [markerX] [markerY]
// Counts unique loadgen frames displayed (decoded from pixel marker) and e2e latency.
import { createRequire } from 'module';
const require = createRequire(process.env.PLAYWRIGHT_NODE_MODULES || process.cwd() + '/node_modules/');
const { chromium } = require('playwright');
const [url, secs = '20', label = '', mx = '300', my = '150'] = process.argv.slice(2);
const browser = await chromium.launch({
  executablePath: process.env.CHROMIUM || 'chromium',
  args: ['--autoplay-policy=no-user-gesture-required', '--no-sandbox', ...(process.env.EXTRA_ARGS ? process.env.EXTRA_ARGS.split(' ') : [])],
  headless: process.env.HEADFUL ? false : true,
});
const page = await browser.newPage({ viewport: { width: 1707, height: 1067 } });
page.on('console', m => { if (process.env.VERBOSE) console.log('[page]', m.text()); });
await page.goto(url);
await page.waitForTimeout(+(process.env.WARMUP || 6000));
await page.evaluate(([mx, my]) => {
  const c = document.createElement('canvas'); c.width = 480; c.height = 48;
  const ctx = c.getContext('2d', { willReadFrequently: true });
  const st = window.__m = { frames: new Set(), lat: [], ticks: 0, perSec: [], cur: new Set(), last: -1, errs: 0 };
  let secStart = performance.now();
  function pick() {
    let best = null, area = 0;
    for (const el of document.querySelectorAll('canvas,video')) {
      if (el === c) continue;
      const w = el.videoWidth || el.width, h = el.videoHeight || el.height;
      if (w * h > area && el.getBoundingClientRect().width > 0) { area = w * h; best = el; }
    }
    return best;
  }
  function tick() {
    st.ticks++;
    const el = pick();
    if (el) {
      try {
        ctx.drawImage(el, mx, my, 480, 48, 0, 0, 480, 48);
        const d = ctx.getImageData(0, 0, 480, 48).data;
        let v = 0n;
        for (let b = 0; b < 40; b++) {
          const x = (b % 20) * 24 + 12, y = Math.floor(b / 20) * 24 + 12;
          const i = (y * 480 + x) * 4;
          if (d[i] + d[i + 1] + d[i + 2] > 382) v |= (1n << BigInt(b));
        }
        const fr = Number(v >> 24n), ms = Number(v & 0xffffffn);
        if (fr !== st.last) {
          st.last = fr; st.cur.add(fr);
          let lat = (Date.now() & 0xffffff) - ms; if (lat < -1e6) lat += 0x1000000;
          if (lat >= 0 && lat < 5000) st.lat.push(lat);
        }
      } catch (e) { st.errs++; }
    }
    const now = performance.now();
    if (now - secStart >= 1000) { st.perSec.push(st.cur.size * 1000 / (now - secStart)); st.cur = new Set(); secStart = now; }
    requestAnimationFrame(tick);
  }
  requestAnimationFrame(tick);
}, [+mx, +my]);
await page.waitForTimeout(+secs * 1000);
const r = await page.evaluate(() => ({ st: (window.voxlStats || window.selkiesStats), perSec: window.__m.perSec, lat: window.__m.lat, ticks: window.__m.ticks, errs: window.__m.errs, selkiesFps: window.fps }));
const s = [...r.perSec].sort((a, b) => a - b), l = [...r.lat].sort((a, b) => a - b);
const q = (a, p) => a.length ? +a[Math.min(a.length - 1, Math.floor(a.length * p))].toFixed(1) : null;
console.log(JSON.stringify({ label, uniqueFps: { avg: +(s.reduce((a, b) => a + b, 0) / s.length).toFixed(1), min: q(s, 0), p10: q(s, 0.1), med: q(s, 0.5) }, latencyMs: { p50: q(l, 0.5), p90: q(l, 0.9), p99: q(l, 0.99) }, client: r.st, rafHz: +(r.ticks / (+secs + 0)).toFixed(1), errs: r.errs, reportedFps: r.selkiesFps }));
await browser.close();
