// Dropping files into apps (--drop-into-apps) end to end: starts its own Xvfb (XVFB),
// a Chromium window on it showing a page with a drop zone (the "app"), and its own
// voxl. Files dropped on the stream in a headless viewer must reach that page as a
// local drop would, besides being uploaded; drops elsewhere only upload.
// Usage: node droptest.mjs   (XVFB, VOXL_BIN, CHROMIUM, PLAYWRIGHT_NODE_MODULES; needs xdotool)
import { createRequire } from 'module';
import { spawn, execFileSync } from 'child_process';
import fs from 'fs';
import os from 'os';
import path from 'path';
const require = createRequire(process.env.PLAYWRIGHT_NODE_MODULES || process.cwd() + '/node_modules/');
const { chromium } = require('playwright');

const HERE = new URL('.', import.meta.url).pathname;
const bin = process.env.VOXL_BIN || path.join(HERE, '../../target/release/voxl');
const DISPLAY = process.env.DROP_DISPLAY || ':9';
const port = process.env.VOXL_PORT || '6909';
const appCdp = process.env.APP_CDP_PORT || '9339';
const base = `http://127.0.0.1:${port}`;
const [W, H] = [1600, 900];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let failed = 0;
const check = (ok, what) => { console.log(`${ok ? 'PASS' : 'FAIL'} ${what}`); if (!ok) failed++; };

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'voxl-drop-'));
const root = path.join(tmp, 'root');
fs.mkdirSync(root);
fs.writeFileSync(path.join(tmp, 'app.html'), `<!doctype html><body style="margin:0">
<div id="zone" style="position:fixed;inset:0;background:#eef">drop here</div>
<script>
window.dropped = null;
zone.addEventListener('dragover', (e) => e.preventDefault());
zone.addEventListener('drop', async (e) => {
  e.preventDefault();
  window.dropped = await Promise.all([...e.dataTransfer.files].map(async (f) => [f.name, await f.text()]));
});
</script>`);

const env = { ...process.env, DISPLAY };
const xvfb = spawn(process.env.XVFB || 'Xvfb', [DISPLAY, '-screen', '0', `${W}x${H}x24`, '-ac', '-nolisten', 'tcp'], { stdio: 'ignore' });
await sleep(1000);
// The app: an 800x600 window in the top left corner. Nothing is under the rest of the screen.
const app = spawn(process.env.CHROMIUM || 'chromium', ['--ozone-platform=x11', '--no-first-run', '--no-default-browser-check', `--user-data-dir=${tmp}/profile`,
  `--remote-debugging-port=${appCdp}`, '--window-position=0,0', '--window-size=800,600', `--app=file://${tmp}/app.html`], { env, stdio: 'ignore' });
const voxl = spawn(bin, [`--port=${port}`, '--audio-enabled=false', '--enable-basic-auth=false', `--files-root=${root}`, '--drop-into-apps'], { env, stdio: 'ignore' });
let b, appBrowser;
try {
  for (let i = 0; i < 50; i++) { try { if ((await fetch(`${base}/healthz`)).ok) break; } catch {} await sleep(100); }
  for (let i = 0; i < 50; i++) { try { appBrowser = await chromium.connectOverCDP(`http://127.0.0.1:${appCdp}`); break; } catch { await sleep(200); } }
  let appPage;
  for (let i = 0; i < 50 && !appPage; i++) {
    appPage = appBrowser.contexts().flatMap((c) => c.pages()).find((p) => p.url().endsWith('app.html'));
    if (!appPage) await sleep(200);
  }
  await appPage.waitForFunction(() => 'dropped' in window);
  const dropped = () => appPage.evaluate(() => window.dropped);
  const resetApp = () => appPage.evaluate(() => { window.dropped = null; });

  // HTTP guards
  const put = (name, body) => fetch(`${base}/files/upload?name=${encodeURIComponent(name)}`, { method: 'PUT', body, headers: { 'X-Voxl-Upload': '1' } }).then((r) => r.json());
  const drop = (body, headers = { 'X-Voxl-Upload': '1' }) =>
    fetch(`${base}/files/drop`, { method: 'POST', body: JSON.stringify(body), headers: { 'Content-Type': 'application/json', ...headers } });
  const { path: apiFile } = await put('api.txt', 'through the API');
  check((await drop({ x: 400, y: 300, paths: [apiFile] }, {})).status === 403, 'drop requires the X-Voxl-Upload header');
  check((await drop({ x: 400, y: 300, paths: ['../../etc/passwd'] })).status === 400, 'drop refuses paths outside the root');
  check((await drop({ x: 400, y: 300, paths: ['Downloads'] })).status === 400, 'drop refuses folders');
  check(JSON.stringify(await (await drop({ x: 1200, y: 700, paths: [apiFile] })).json()) === '{"dropped":false}', 'nothing under the point: not dropped');
  check(await dropped() === null, 'the app got nothing from a drop beside it');
  check(JSON.stringify(await (await drop({ x: 400, y: 300, paths: [apiFile] })).json()) === '{"dropped":true}', 'API drop on the app: dropped');
  // The page reads the files after the drop, so wait for it.
  await appPage.waitForFunction(() => window.dropped, null, { timeout: 10000 }).catch(() => {});
  check(JSON.stringify(await dropped()) === '[["api.txt","through the API"]]', 'the app got the uploaded file and its contents');
  await resetApp();

  b = await chromium.launch({ executablePath: process.env.CHROMIUM || 'chromium', args: ['--no-sandbox'] });
  const p = await b.newPage({ viewport: { width: W, height: H } });
  await p.goto(`${base}/`);
  await p.waitForFunction(() => document.getElementById('tools').classList.contains('files'), null, { timeout: 10000 });
  // Drags files over the desktop point (x, y) of the element `sel` and drops them there.
  const dragDrop = (x, y, files, sel = '#surface', thenDrop = true) => p.evaluate(({ x, y, files, sel, W, H, thenDrop }) => {
    const s = document.getElementById('surface').getBoundingClientRect();
    const clientX = s.left + ((x + 0.5) * s.width) / W, clientY = s.top + ((y + 0.5) * s.height) / H;
    const dt = new DataTransfer();
    for (const [name, text] of files) dt.items.add(new File([text], name));
    const target = document.querySelector(sel);
    for (const type of thenDrop ? ['dragenter', 'dragover', 'drop'] : ['dragenter', 'dragover'])
      target.dispatchEvent(new DragEvent(type, { dataTransfer: dt, clientX, clientY, bubbles: true, cancelable: true }));
  }, { x, y, files, sel, W, H, thenDrop });
  const pointer = () => execFileSync('xdotool', ['getmouselocation'], { env }).toString().match(/x:(\d+) y:(\d+)/).slice(1).map(Number);

  await dragDrop(400, 300, [['note.txt', 'dragged in']], '#surface', false);
  check(!(await p.isVisible('#drop')), 'no upload card while dragging over the desktop');
  await sleep(200);
  check(JSON.stringify(pointer()) === '[400,300]', `the remote pointer follows the drag (${pointer()})`);
  await p.evaluate(() => window.dispatchEvent(new DragEvent('dragleave', { dataTransfer: new DataTransfer(), bubbles: true })));

  await dragDrop(400, 300, [['note.txt', 'dragged in']]);
  await appPage.waitForFunction(() => window.dropped, null, { timeout: 10000 }).catch(() => {});
  check(JSON.stringify(await dropped()) === '[["note.txt","dragged in"]]', 'a file dropped on the app reaches it');
  check(fs.readFileSync(path.join(root, 'Downloads/note.txt'), 'utf8') === 'dragged in', 'and is saved in ~/Downloads');
  await sleep(1700);
  check(!(await p.isVisible('#files.open')), 'the Files panel stays shut when the app takes the files');
  await resetApp();

  await dragDrop(300, 200, [['a.csv', 'a,b'], ['b.md', '# b']]);
  await appPage.waitForFunction(() => window.dropped, null, { timeout: 10000 }).catch(() => {});
  check(JSON.stringify(await dropped()) === '[["a.csv","a,b"],["b.md","# b"]]', 'several files reach the app in one drop');
  await resetApp();

  await dragDrop(1200, 700, [['beside.txt', 'nowhere']]);
  await p.waitForSelector('#files.open', { timeout: 10000 }).catch(() => {});
  check(fs.existsSync(path.join(root, 'Downloads/beside.txt')) && await p.isVisible('#files.open'), 'beside any app: uploaded, and the panel opens to show it');

  // The panel is open now: a drop on it only uploads.
  await dragDrop(1500, 300, [['panel.txt', 'panel only']], '#files', false);
  check(await p.isVisible('#drop'), 'the upload card shows over the Files panel');
  await dragDrop(1500, 300, [['panel.txt', 'panel only']], '#files');
  await p.waitForFunction(() => [...document.querySelectorAll('#files-list .f-name')].some((e) => e.textContent === 'panel.txt'), null, { timeout: 10000 });
  await sleep(500);
  check(await dropped() === null, 'a drop on the Files panel only uploads');
} catch (e) {
  check(false, `test error: ${e.message}`);
} finally {
  if (b) await b.close();
  voxl.kill();
  // Quit the app through CDP and wait, so it stops writing its profile before cleanup.
  const exited = new Promise((r) => { app.once('exit', r); setTimeout(r, 5000); });
  if (appBrowser) await appBrowser.newBrowserCDPSession().then((s) => s.send('Browser.close')).catch(() => app.kill());
  else app.kill();
  await exited;
  xvfb.kill();
  fs.rmSync(tmp, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
}
console.log(failed ? `${failed} failed` : 'all passed');
process.exit(failed ? 1 : 0);
