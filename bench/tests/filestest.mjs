// File transfer end to end: starts its own voxl on BENCH_DISPLAY with a temporary
// --files-root, then drives the Files panel in headless Chromium (upload button,
// drag and drop, folders, download, auto-refresh) and checks the HTTP API's guards.
// Usage: node filestest.mjs   (VOXL_BIN, BENCH_DISPLAY, CHROMIUM, PLAYWRIGHT_NODE_MODULES)
import { createRequire } from 'module';
import { spawn } from 'child_process';
import fs from 'fs';
import os from 'os';
import path from 'path';
const require = createRequire(process.env.PLAYWRIGHT_NODE_MODULES || process.cwd() + '/node_modules/');
const { chromium } = require('playwright');

const HERE = new URL('.', import.meta.url).pathname;
const bin = process.env.VOXL_BIN || path.join(HERE, '../../target/release/voxl');
const port = process.env.VOXL_PORT || '6905';
const base = `http://127.0.0.1:${port}`;
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'voxl-files-'));
const fixtures = fs.mkdtempSync(path.join(os.tmpdir(), 'voxl-fixtures-'));
fs.mkdirSync(path.join(root, 'docs'));
fs.writeFileSync(path.join(root, 'docs/a.txt'), 'a');
fs.writeFileSync(path.join(root, 'résumé.txt'), 'unicode');
fs.symlinkSync('/etc', path.join(root, 'etc-link'));
fs.writeFileSync(path.join(fixtures, 'report.csv'), 'quarterly numbers\n');
fs.writeFileSync(path.join(fixtures, 'photo.jpg'), Buffer.alloc(3 << 20, 7));

let failed = 0;
const check = (ok, what) => { console.log(`${ok ? 'PASS' : 'FAIL'} ${what}`); if (!ok) failed++; };

const voxl = spawn(bin, [`--port=${port}`, '--audio-enabled=false', `--files-root=${root}`],
  { env: { ...process.env, DISPLAY: process.env.BENCH_DISPLAY || ':2' }, stdio: 'ignore' });
let b;
try {
  for (let i = 0; i < 50; i++) { try { if ((await fetch(`${base}/healthz`)).ok) break; } catch {} await new Promise((r) => setTimeout(r, 100)); }

  // HTTP guards
  const st = async (u, o) => (await fetch(base + u, o)).status;
  check(await st('/files/list?path=..') === 400, 'list rejects ..');
  check(await st('/files/list?path=etc-link') === 403, 'list refuses symlinks leading outside the root');
  check(await st('/files/download?path=etc-link/passwd') === 403, 'download refuses symlinks leading outside the root');
  check(await st('/files/upload?name=x.txt', { method: 'PUT', body: 'x' }) === 403, 'upload requires the X-Voxl-Upload header');
  check(await st('/files/upload?name=..%2Fx', { method: 'PUT', body: 'x', headers: { 'X-Voxl-Upload': '1' } }) === 400, 'upload rejects path-like names');
  const listed = await (await fetch(`${base}/files/list`)).json();
  check(!listed.entries.some((e) => e.name === 'etc-link'), 'outside symlinks are hidden from listings');

  b = await chromium.launch({ executablePath: process.env.CHROMIUM || 'chromium', args: ['--no-sandbox'] });
  const ctx = await b.newContext({ viewport: { width: 1400, height: 900 }, acceptDownloads: true });
  const p = await ctx.newPage();
  const names = () => p.$$eval('#files-list .f-row .f-name', (e) => e.map((x) => x.textContent));
  await p.goto(base + '/');
  await p.evaluate(() => { window.crumbs = () => [...document.querySelectorAll('#files-crumbs a')].map((a) => a.textContent).join('/'); });
  await p.waitForFunction(() => document.getElementById('tools').classList.contains('files'), null, { timeout: 10000 });
  await p.mouse.move(1380, 20); await p.waitForTimeout(300);
  await p.click('#files-btn');
  await p.waitForSelector('#files.open');
  await p.waitForFunction(() => crumbs() === 'Home/Downloads');
  check(true, 'panel opens at ~/Downloads before it exists');

  await p.setInputFiles('#files-input', [path.join(fixtures, 'report.csv'), path.join(fixtures, 'photo.jpg')]);
  await p.waitForFunction(() => document.querySelectorAll('.f-up.done').length === 2, null, { timeout: 15000 });
  check(fs.readFileSync(path.join(root, 'Downloads/photo.jpg')).equals(fs.readFileSync(path.join(fixtures, 'photo.jpg'))), 'upload button: files arrive intact');
  await p.setInputFiles('#files-input', [path.join(fixtures, 'report.csv')]);
  await p.waitForFunction(() => document.querySelectorAll('.f-up.done').length === 3, null, { timeout: 15000 });
  check(fs.existsSync(path.join(root, 'Downloads/report (1).csv')), 'name collisions get a (1) suffix');
  check(!fs.readdirSync(path.join(root, 'Downloads')).some((n) => n.endsWith('.part')), 'no temporary files left behind');

  const dt = await p.evaluateHandle(() => { const d = new DataTransfer(); d.items.add(new File(['dropped!'], 'dropped.txt')); return d; });
  await p.dispatchEvent('#surface', 'dragenter', { dataTransfer: dt });
  check(await p.isVisible('#drop'), 'drop overlay appears while dragging files');
  await p.dispatchEvent('#surface', 'drop', { dataTransfer: dt });
  await p.waitForFunction(() => [...document.querySelectorAll('#files-list .f-name')].some((e) => e.textContent === 'dropped.txt'), null, { timeout: 10000 });
  check(fs.readFileSync(path.join(root, 'Downloads/dropped.txt'), 'utf8') === 'dropped!' && !(await p.isVisible('#drop')), 'drag and drop uploads into the open folder');

  fs.writeFileSync(path.join(root, 'Downloads/from-vm.pdf'), 'x');
  await p.waitForFunction(() => [...document.querySelectorAll('#files-list .f-name')].some((e) => e.textContent === 'from-vm.pdf'), null, { timeout: 8000 });
  check((await names())[0] === 'from-vm.pdf', 'files that appear in the VM show up, newest first');

  await p.click('#files-crumbs a:first-child');
  await p.waitForFunction(() => crumbs() === 'Home');
  await p.click('.f-row.dir >> text=docs');
  await p.waitForFunction(() => crumbs() === 'Home/docs');
  check(JSON.stringify(await names()) === '["a.txt"]', 'folders open from the list and the breadcrumb');
  await p.click('#files-crumbs a:first-child');
  await p.waitForFunction(() => crumbs() === 'Home');
  const [dl] = await Promise.all([p.waitForEvent('download'), p.click('.f-row >> text=résumé.txt')]);
  check(dl.suggestedFilename() === 'résumé.txt' && fs.readFileSync(await dl.path(), 'utf8') === 'unicode', 'clicking a file downloads it with its name');

  await p.keyboard.press('Escape');
  check(!(await p.isVisible('#files')), 'Escape closes the panel');
} finally {
  if (b) await b.close();
  voxl.kill();
  fs.rmSync(root, { recursive: true, force: true });
  fs.rmSync(fixtures, { recursive: true, force: true });
}
console.log(failed ? `${failed} failed` : 'all passed');
process.exit(failed ? 1 : 0);
