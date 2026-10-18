// Dynamic resolution end to end: starts a patched-or-stock Xvfb (XVFB) at 2560x1600
// with its own voxl --enable-resize, then checks that the desktop follows the
// browser window: size, clamping, decoded video size, pointer mapping, and that
// capture stays on the direct framebuffer path after each resize.
// Usage: node resizedyn.mjs   (XVFB, XRANDR = xrandr binary, VOXL_BIN, CHROMIUM,
// PLAYWRIGHT_NODE_MODULES)
import { createRequire } from 'module';
import { spawn, execFileSync } from 'child_process';
import fs from 'fs';
import path from 'path';
const require = createRequire(process.env.PLAYWRIGHT_NODE_MODULES || process.cwd() + '/node_modules/');
const { chromium } = require('playwright');

const HERE = new URL('.', import.meta.url).pathname;
const bin = process.env.VOXL_BIN || path.join(HERE, '../../target/release/voxl');
const DISPLAY = process.env.RESIZE_DISPLAY || ':8';
const port = process.env.VOXL_PORT || '6908';
const log = '/tmp/voxl-resizedyn.log';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let failed = 0;
const check = (ok, what) => { console.log(`${ok ? 'PASS' : 'FAIL'} ${what}`); if (!ok) failed++; };
const xsize = () => execFileSync(process.env.XRANDR || 'xrandr', [], { env: { ...process.env, DISPLAY } }).toString().match(/current (\d+) x (\d+)/).slice(1).map(Number);

const xvfb = spawn(process.env.XVFB || 'Xvfb', [DISPLAY, '-screen', '0', '2560x1600x24', '-dpi', '144', '-shmem', '-ac', '-nolisten', 'tcp'], { stdio: 'ignore' });
await sleep(1000);
const voxl = spawn(bin, [`--port=${port}`, '--audio-enabled=false', '--enable-resize=true'], { env: { ...process.env, DISPLAY }, stdio: ['ignore', fs.openSync(log, 'w'), fs.openSync(log, 'a')] });
let b;
try {
	for (let i = 0; i < 50; i++) { try { if ((await fetch(`http://127.0.0.1:${port}/healthz`)).ok) break; } catch {} await sleep(100); }
	b = await chromium.launch({ executablePath: process.env.CHROMIUM || 'chromium', args: ['--no-sandbox'] });
	const p = await b.newPage({ viewport: { width: 1280, height: 800 } });
	await p.goto(`http://127.0.0.1:${port}/`);
	const videoSize = () => p.evaluate(() => { const v = document.querySelector('video.stripe'); return v ? [v.videoWidth, v.videoHeight] : null; });
	const settle = async (want) => {
		for (let i = 0; i < 40; i++) { const [w, h] = xsize(); const v = await videoSize(); if (w === want[0] && h === want[1] && v && v[0] === want[0]) return; await sleep(150); }
	};

	await settle([1920, 1200]);
	check(JSON.stringify(xsize()) === '[1920,1200]', `1280x800 window -> 1920x1200 desktop (X says ${xsize().join('x')})`);
	check(JSON.stringify(await videoSize()) === '[1920,1200]', `video decodes at 1920x1200 (${(await videoSize())?.join('x')})`);

	// Pointer mapping: the middle of the page is the middle of the desktop.
	await p.mouse.move(640, 400);
	await sleep(300);
	const loc = execFileSync('xdotool', ['getmouselocation'], { env: { ...process.env, DISPLAY } }).toString();
	const [mx, my] = loc.match(/x:(\d+) y:(\d+)/).slice(1).map(Number);
	check(Math.abs(mx - 960) <= 2 && Math.abs(my - 600) <= 2, `pointer maps 1:1 at the new size (${mx},${my})`);

	await p.setViewportSize({ width: 1000, height: 700 });
	await settle([1496, 1050]);
	check(JSON.stringify(xsize()) === '[1496,1050]', `1000x700 window -> 1496x1050 (width rounded to 8): ${xsize().join('x')}`);

	await p.setViewportSize({ width: 2000, height: 1300 });
	await settle([2560, 1600]);
	check(JSON.stringify(xsize()) === '[2560,1600]', `2000x1300 window is clamped to the 2560x1600 maximum: ${xsize().join('x')}`);

	await p.setViewportSize({ width: 300, height: 200 });
	await settle([640, 480]);
	check(JSON.stringify(xsize()) === '[640,480]', `tiny window is clamped to the 640x480 minimum: ${xsize().join('x')}`);

	const l = fs.readFileSync(log, 'utf8');
	const direct = (l.match(/capturing directly from the Xvfb framebuffer/g) || []).length;
	const fallback = /falling back to GetImage/.test(l);
	check(direct >= 5 && !fallback, `capture stays zero-copy after every resize (${direct} direct setups, fallback: ${fallback})`);
	// The framebuffer must be this Xvfb's own, not another display's that happens to match.
	const ids = [...new Set([...l.matchAll(/shmid (\d+)/g)].map((m) => m[1]))];
	const owner = (id) => fs.readFileSync('/proc/sysvipc/shm', 'utf8').split('\n').map((r) => r.trim().split(/\s+/)).find((f) => f[1] === id)?.[4];
	check(ids.length === 1 && Number(owner(ids[0])) === xvfb.pid, `framebuffer belongs to this Xvfb (shmid ${ids.join(',')}, creator ${owner(ids[0])}, Xvfb ${xvfb.pid})`);
} finally {
	if (b) await b.close();
	voxl.kill();
	xvfb.kill();
}
console.log(failed ? `${failed} failed` : 'all passed');
process.exit(failed ? 1 : 0);
