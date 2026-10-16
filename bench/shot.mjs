// node shot.mjs <url> <out.png> [waitMs]
import { createRequire } from 'module';
const require = createRequire(
	process.env.PLAYWRIGHT_NODE_MODULES || process.cwd() + '/node_modules/'
);
const { chromium } = require('playwright');
const [url, out, wait = '4000'] = process.argv.slice(2);
const probe = process.env.PROBE;
const b = await chromium.launch({
	executablePath: process.env.CHROMIUM || 'chromium',
	args: ['--no-sandbox'],
});
const p = await b.newPage({
	viewport: { width: 1280, height: 800 },
	deviceScaleFactor: 2,
});
p.on('console', (m) => console.log('[page]', m.type(), m.text()));
await p.goto(url);
await p.waitForTimeout(+wait);
if (probe)
	console.log(
		'probe',
		JSON.stringify(
			await p.evaluate(() => ({
				stats: window.voxlStats || window.selkiesStats,
				videos: [...document.querySelectorAll('video.stripe')].map((v) => [
					v.videoWidth,
					v.videoHeight,
					v.style.top,
					v.style.height,
				]),
				cursor: document.getElementById('surface').style.cursor.slice(0, 40),
			}))
		)
	);
await p.screenshot({ path: out });
await b.close();
