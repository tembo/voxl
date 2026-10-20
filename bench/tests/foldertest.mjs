// Folder transfer end to end: starts its own voxl on BENCH_DISPLAY with a temporary
// --files-root, downloads a folder as a zip (checked against the originals) and
// uploads folders through the HTTP API and the Files panel's folder picker.
// Usage: node foldertest.mjs   (VOXL_BIN, BENCH_DISPLAY, CHROMIUM, PLAYWRIGHT_NODE_MODULES)
import { createRequire } from 'module';
import { spawn, execFileSync } from 'child_process';
import crypto from 'crypto';
import fs from 'fs';
import os from 'os';
import path from 'path';
const require = createRequire(process.env.PLAYWRIGHT_NODE_MODULES || process.cwd() + '/node_modules/');
const { chromium } = require('playwright');

const HERE = new URL('.', import.meta.url).pathname;
const bin = process.env.VOXL_BIN || path.join(HERE, '../../target/release/voxl');
const port = process.env.VOXL_PORT || '6906';
const base = `http://127.0.0.1:${port}`;
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'voxl-folders-'));
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'voxl-folders-tmp-'));
const sha = (p) => crypto.createHash('sha256').update(fs.readFileSync(p)).digest('hex');

// A project-like tree: nested folders, an empty folder, a dotfile, a large random
// (incompressible) file, a compressible one, unicode names, and a symlink out.
const proj = path.join(root, 'project');
fs.mkdirSync(path.join(proj, 'src/deep/er'), { recursive: true });
fs.mkdirSync(path.join(proj, 'empty'));
fs.writeFileSync(path.join(proj, '.env'), 'SECRET=1\n');
fs.writeFileSync(path.join(proj, 'src/main.rs'), 'fn main() {}\n'.repeat(5000));
fs.writeFileSync(path.join(proj, 'src/deep/er/naïve file.txt'), 'unicode');
fs.writeFileSync(path.join(proj, 'video.mp4'), crypto.randomBytes(20 << 20));
fs.symlinkSync('/etc', path.join(proj, 'etc-link'));

let failed = 0;
const check = (ok, what) => { console.log(`${ok ? 'PASS' : 'FAIL'} ${what}`); if (!ok) failed++; };

const voxl = spawn(bin, [`--port=${port}`, '--audio-enabled=false', `--files-root=${root}`],
	{ env: { ...process.env, DISPLAY: process.env.BENCH_DISPLAY || ':2' }, stdio: 'ignore' });
let b;
try {
	for (let i = 0; i < 50; i++) { try { if ((await fetch(`${base}/healthz`)).ok) break; } catch {} await new Promise((r) => setTimeout(r, 100)); }

	// Folder download as a zip, streamed.
	const t0 = Date.now();
	const r = await fetch(`${base}/files/download?path=project`);
	const zip = Buffer.from(await r.arrayBuffer());
	check(r.ok && r.headers.get('content-type') === 'application/zip' && /project\.zip/.test(r.headers.get('content-disposition') || ''),
		`folder downloads as project.zip (${r.status}, ${(zip.length / 1048576).toFixed(1)} MB in ${Date.now() - t0} ms)`);
	fs.writeFileSync(path.join(tmp, 'p.zip'), zip);
	execFileSync('unzip', ['-q', path.join(tmp, 'p.zip'), '-d', path.join(tmp, 'out')]);
	const out = path.join(tmp, 'out/project');
	const same = ['.env', 'src/main.rs', 'src/deep/er/naïve file.txt', 'video.mp4'].every((f) => fs.existsSync(path.join(out, f)) && sha(path.join(out, f)) === sha(path.join(proj, f)));
	check(same, 'zip holds every file byte for byte (dotfiles, unicode names, nested folders)');
	check(fs.existsSync(path.join(out, 'empty')) && fs.statSync(path.join(out, 'empty')).isDirectory(), 'empty folders are kept');
	check(!fs.existsSync(path.join(out, 'etc-link')), 'symlinks are left out');
	const listing = execFileSync('unzip', ['-v', path.join(tmp, 'p.zip')]).toString();
	check(/Stored.*video\.mp4/.test(listing) && /Defl.*main\.rs/.test(listing), 'compressed formats are stored, the rest deflated');
	check((await fetch(`${base}/files/download?path=project/etc-link`)).status === 403, 'a symlinked folder leading outside the root is refused');

	// Folder upload through the API: sub creates folders, merges, and stays inside the root.
	const up = (q, body) => fetch(`${base}/files/upload?${new URLSearchParams(q)}`, { method: 'PUT', headers: { 'X-Voxl-Upload': '1' }, body });
	check((await up({ path: '', name: 'a.txt', sub: 'Up/one/two' }, 'A')).ok && fs.readFileSync(path.join(root, 'Up/one/two/a.txt'), 'utf8') === 'A', 'upload with sub creates nested folders');
	check((await up({ path: '', name: 'b.txt', sub: 'Up/one' }, 'B')).ok && fs.existsSync(path.join(root, 'Up/one/two/a.txt')) && fs.existsSync(path.join(root, 'Up/one/b.txt')), 'an existing folder is merged into');
	check((await up({ path: '', name: 'x', sub: 'Up/../..' }, 'x')).status === 400, 'sub rejects ..');
	check((await up({ path: '', name: 'n.txt', sub: 'v1..2/a..b' }, 'n')).ok && fs.existsSync(path.join(root, 'v1..2/a..b/n.txt')), 'folder names with .. inside them (v1..2) are fine');
	check((await up({ path: '', name: 'x', sub: 'project/etc-link/evil' }, 'x')).status === 403, 'sub refuses symlinks leading outside the root');

	// Folder upload through the panel's folder picker.
	const pick = path.join(tmp, 'Photos');
	fs.mkdirSync(path.join(pick, '2024/summer'), { recursive: true });
	for (let i = 0; i < 12; i++) fs.writeFileSync(path.join(pick, `2024/summer/img${i}.jpg`), crypto.randomBytes(200 << 10));
	fs.writeFileSync(path.join(pick, 'notes.txt'), 'notes');
	b = await chromium.launch({ executablePath: process.env.CHROMIUM || 'chromium', args: ['--no-sandbox'] });
	const p = await b.newPage({ viewport: { width: 1280, height: 800 } });
	await p.goto(base + '/');
	await p.waitForFunction(() => document.getElementById('tools').classList.contains('files'));
	await p.evaluate(() => document.getElementById('files-btn').click());
	await p.waitForSelector('#files.open');
	await p.locator('#files-input-dir').setInputFiles(pick);
	await p.waitForFunction(() => /Uploaded/.test(document.getElementById('files-uploads').textContent || ''), null, { timeout: 20000 });
	const row = await p.textContent('#files-uploads');
	const n = fs.readdirSync(path.join(root, 'Downloads/Photos/2024/summer')).length;
	check(n === 12 && fs.existsSync(path.join(root, 'Downloads/Photos/notes.txt')) && sha(path.join(root, 'Downloads/Photos/2024/summer/img3.jpg')) === sha(path.join(pick, '2024/summer/img3.jpg')),
		`folder picker uploads the tree into the open folder (${n} photos), one progress row: ${JSON.stringify(row)}`);

	// The folder row offers a zip download on hover.
	await p.evaluate(() => [...document.querySelectorAll('#files-crumbs a')][0].click());
	await p.waitForSelector('.f-row.dir');
	const projRow = p.locator('.f-row.dir', { hasText: 'project' });
	await projRow.hover();
	check(await projRow.locator('.f-dl').isVisible(), 'folder rows show a download button on hover');
	const [dl] = await Promise.all([p.waitForEvent('download'), projRow.locator('.f-dl').click()]);
	check(dl.suggestedFilename() === 'project.zip', `clicking it downloads ${dl.suggestedFilename()}`);
	check(!(await p.evaluate(() => document.getElementById('files-crumbs').textContent || '')).includes('project'), 'the click downloads without opening the folder');
	await projRow.hover();
	await p.screenshot({ path: process.env.SHOT || path.join(tmp, 'panel.png') });
} finally {
	if (b) await b.close();
	voxl.kill();
	fs.rmSync(root, { recursive: true, force: true });
	fs.rmSync(tmp, { recursive: true, force: true });
}
console.log(failed ? `${failed} failed` : 'all passed');
process.exit(failed ? 1 : 0);
