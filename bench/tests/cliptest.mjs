import { createRequire } from 'module';
import { execSync, spawn } from 'child_process';
import { readFileSync } from 'fs';
const require = createRequire(process.env.PLAYWRIGHT_NODE_MODULES || process.cwd() + '/node_modules/');
const { chromium } = require('playwright');
const exe = process.env.CHROMIUM || 'chromium';
const HERE = new URL('.', import.meta.url).pathname;
const PORT = process.env.VOXL_PORT || '6903';
const DISPLAY = process.env.BENCH_DISPLAY || ':3';
const b = await chromium.launch({ executablePath: exe, args: ['--no-sandbox'] });
const ctx = await b.newContext({ viewport: { width: 1280, height: 800 } });
await ctx.grantPermissions(['clipboard-read', 'clipboard-write'], { origin: 'http://localhost:' + (process.env.VOXL_PORT || '6903') + '' });
const p = await ctx.newPage();
p.on('console', m => m.type() !== 'log' && !m.text().includes('AudioContext') && console.log('[page]', m.text()));
await p.goto('http://localhost:' + (process.env.VOXL_PORT || '6903') + '/'); await p.waitForTimeout(2500);
await p.mouse.click(1200, 700); await p.waitForTimeout(300);
// remote -> local: an X app (xclip) owns CLIPBOARD with a PNG
const xclip = spawn('nix-shell', ['-p', 'xclip', '--run', `DISPLAY=${process.env.BENCH_DISPLAY || ':3'} xclip -selection clipboard -t image/png -i ${process.env.TEST_PNG || '/tmp/test.png'} -loops 3`], { stdio: 'ignore' });
await p.waitForTimeout(6000);
const got = await p.evaluate(async () => { const items = await navigator.clipboard.read(); const out = []; for (const it of items) for (const t of it.types) out.push([t, (await it.getType(t)).size]); return out; });
console.log('remote image -> local clipboard:', JSON.stringify(got), '(source', readFileSync((process.env.TEST_PNG || '/tmp/test.png')).length, 'bytes)');
// local -> remote: put a PNG on the local clipboard and paste into the desktop
const b64 = readFileSync((process.env.TEST_PNG || '/tmp/test.png')).toString('base64');
await p.evaluate(async (b64) => { const bin = Uint8Array.from(atob(b64), c => c.charCodeAt(0)); await navigator.clipboard.write([new ClipboardItem({ 'image/png': new Blob([bin], { type: 'image/png' }) })]); }, b64);
await p.mouse.click(1200, 700); await p.waitForTimeout(200);
await p.keyboard.press('Control+v'); await p.waitForTimeout(1500);
const remote = execSync(`nix-shell -p xclip --run 'DISPLAY=${process.env.BENCH_DISPLAY || ':3'} xclip -selection clipboard -o -t TARGETS; DISPLAY=${process.env.BENCH_DISPLAY || ':3'} xclip -selection clipboard -o -t image/png | wc -c'`).toString().trim().split('\n');
console.log('local image -> remote clipboard: targets', JSON.stringify(remote.slice(0, -1)), 'image bytes', remote.at(-1));
await b.close();
