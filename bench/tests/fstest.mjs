import { createRequire } from 'module';
const require = createRequire(
	process.env.PLAYWRIGHT_NODE_MODULES || process.cwd() + '/node_modules/'
);
const { chromium } = require('playwright');
const b = await chromium.launch({
	executablePath: process.env.CHROMIUM || 'chromium',
	args: ['--no-sandbox'],
});
const p = await b.newPage({ viewport: { width: 1280, height: 800 } });
await p.goto('http://localhost:' + (process.env.VOXL_PORT || '6903') + '/');
await p.waitForTimeout(2500);
await p.evaluate(() => {
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
});
await p.mouse.move(1240, 20);
await p.waitForTimeout(400);
console.log(
	'toolbar shown on hover:',
	await p.evaluate(() =>
		document.getElementById('tools').classList.contains('show')
	)
);
await p.screenshot({
	path: '/tmp/voxl-toolbar.png',
	clip: { x: 1080, y: 0, width: 200, height: 80 },
});
await p.click('#fs-btn');
await p.waitForTimeout(800);
console.log(
	'fullscreen:',
	await p.evaluate(() => !!document.fullscreenElement),
	'| keyboard lock:',
	await p.evaluate(() => window.__lock)
);
let sent = [];
await p.exposeFunction('__sent', (d) => sent.push(d));
await p.keyboard.press('Control+w');
await p.waitForTimeout(300);
console.log('page still open after Ctrl+W:', !p.isClosed());
await b.close();
