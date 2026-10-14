// node shot.mjs <url> <out.png> [waitMs]
import { createRequire } from 'module';
const require = createRequire(process.env.PLAYWRIGHT_NODE_MODULES || process.cwd() + '/node_modules/');
const { chromium } = require('playwright');
const [url, out, wait = '4000'] = process.argv.slice(2);
const b = await chromium.launch({ executablePath: process.env.CHROMIUM || 'chromium', args: ['--no-sandbox'] });
const p = await b.newPage({ viewport: { width: 1280, height: 800 }, deviceScaleFactor: 2 });
p.on('console', m => console.log('[page]', m.type(), m.text()));
await p.goto(url); await p.waitForTimeout(+wait);
await p.screenshot({ path: out });
await b.close();
