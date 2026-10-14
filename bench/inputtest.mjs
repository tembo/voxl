// Drives the desktop through the selkies-rs web client and checks the result on X.
import { createRequire } from 'module';
import { execSync } from 'child_process';
const require = createRequire(process.env.PLAYWRIGHT_NODE_MODULES || process.cwd() + '/node_modules/');
const { chromium } = require('playwright');
const url = process.argv[2] || 'http://localhost:6902/';
const b = await chromium.launch({ executablePath: process.env.CHROMIUM || 'chromium', args: ['--no-sandbox'] });
const ctx = await b.newContext({ viewport: { width: 1280, height: 800 } });
await ctx.grantPermissions(['clipboard-read', 'clipboard-write'], { origin: new URL(url).origin });
const p = await ctx.newPage();
p.on('console', m => m.type() !== 'log' && console.log('[page]', m.type(), m.text()));
await p.goto(url); await p.waitForTimeout(3000);
// Click Chromium's omnibox (desktop 600,254 -> viewport scale 0.5).
await p.mouse.click(300, 95);
await p.waitForTimeout(300);
await p.keyboard.press('Control+a');
await p.keyboard.type('Hello, World! @#$%^&*()_+ äöü', { delay: 15 });
await p.waitForTimeout(800);
execSync('DISPLAY=:1 ffmpeg -loglevel error -y -f x11grab -video_size 1400x120 -i :1+100,140 -frames:v 1 /tmp/input1.png');
// Clipboard: select all + copy on the remote, expect it in the local clipboard.
await p.keyboard.press('Control+a'); await p.keyboard.press('Control+c');
await p.waitForTimeout(800);
console.log('local clipboard after remote copy:', JSON.stringify(await p.evaluate(() => navigator.clipboard.readText())));
// Local -> remote paste.
await p.evaluate(() => navigator.clipboard.writeText('pasted-from-local-123'));
await p.keyboard.press('Control+a'); await p.keyboard.press('Control+v');
await p.waitForTimeout(800);
execSync('DISPLAY=:1 ffmpeg -loglevel error -y -f x11grab -video_size 1400x120 -i :1+100,140 -frames:v 1 /tmp/input2.png');
await p.keyboard.press('Escape');
await p.waitForTimeout(200);
await b.close();
