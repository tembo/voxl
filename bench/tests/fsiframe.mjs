import { createRequire } from 'module';
const require = createRequire(
	process.env.PLAYWRIGHT_NODE_MODULES || process.cwd() + '/node_modules/'
);
const { chromium } = require('playwright');
const b = await chromium.launch({
	executablePath: process.env.CHROMIUM || 'chromium',
	args: ['--no-sandbox'],
});
const p = await b.newPage({ viewport: { width: 1600, height: 900 } });
await p.goto('file://' + new URL('embed.html', import.meta.url).pathname);
await p.waitForTimeout(3000);
const f = p.frames().find((fr) => fr.url().includes('6903'));
await f.evaluate(() => {
	window.__lock = 'not called';
	const kl = navigator.keyboard;
	const orig = kl.lock.bind(kl);
	kl.lock = (...a) =>
		orig(...a).then(
			() => {
				window.__lock = 'locked';
			},
			(e) => {
				window.__lock = 'rejected: ' + e.message;
			}
		);
	document.getElementById('tools').classList.add('show');
});
await f.click('#fs-btn');
await p.waitForTimeout(800);
// Browsers refuse the lock inside the iframe; embed.html takes it from the top level instead.
const top = await p.evaluate(
	() => navigator.keyboard && (navigator.keyboard.lock ? 'available' : 'n/a')
);
console.log(
	'iframe fullscreen:',
	await f.evaluate(() => !!document.fullscreenElement),
	'| lock from inside iframe:',
	await f.evaluate(() => window.__lock),
	'| top-level API:',
	top
);
await b.close();
