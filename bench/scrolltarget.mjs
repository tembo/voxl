// Opens /tmp/scroll.html in a headful Chromium on :1 and smooth-scrolls it for N seconds.
import { createRequire } from 'module';
const require = createRequire(process.env.PLAYWRIGHT_NODE_MODULES || process.cwd() + '/node_modules/');
const { chromium } = require('playwright');
const secs = +(process.argv[2] || 20);
const b = await chromium.launch({ executablePath: process.env.CHROMIUM || 'chromium', headless: false,
  env: { ...process.env, DISPLAY: ':1' }, args: ['--no-sandbox', '--window-position=60,90', '--window-size=1600,950', '--no-first-run', ...(process.env.CFLAGS !== undefined ? process.env.CFLAGS.split(' ').filter(Boolean) : ['--disable-gpu'])] });
const p = await b.newPage();
await p.goto('file:///tmp/scroll.html'); await p.bringToFront(); await p.waitForTimeout(1500);
// Smooth scroll driven by the page itself: one step per animation frame.
await p.evaluate(() => { let d = 1; const step = () => { window.scrollBy(0, 6 * d); if (scrollY + innerHeight >= document.body.scrollHeight - 2 || scrollY <= 0) d = -d; requestAnimationFrame(step); }; requestAnimationFrame(step); });
await p.waitForTimeout(secs * 1000);
await b.close();
