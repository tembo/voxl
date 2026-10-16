import { createRequire } from 'module';
const require = createRequire(
	process.env.PLAYWRIGHT_NODE_MODULES || process.cwd() + '/node_modules/'
);
const { chromium } = require('playwright');
const disp = process.argv[2] || ':3',
	port = process.argv[3] || '6903';
const b = await chromium.launch({
	executablePath: process.env.CHROMIUM || 'chromium',
	headless: false,
	env: { ...process.env, DISPLAY: disp },
	args: [
		'--no-sandbox',
		'--window-position=60,90',
		'--window-size=1600,950',
		'--no-first-run',
		'--disable-smooth-scrolling',
	],
});
const p = await b.newPage();
await p.goto('file://' + (process.env.SCROLL_PAGE || '/tmp/scroll.html'));
await p.bringToFront();
await p.waitForTimeout(1500);
const ws = new WebSocket(`ws://localhost:${port}/ws`);
await new Promise((r) => ws.addEventListener('open', r, { once: true }));
ws.send('m,900,700');
await p.waitForTimeout(300);
const ys = [await p.evaluate(() => scrollY)];
for (let i = 0; i < 12; i++) {
	ws.send('sp,0,15');
	await p.waitForTimeout(40);
	ys.push(await p.evaluate(() => scrollY));
}
console.log(
	'scrollY after each 15px gesture step:',
	ys.map((y) => Math.round(y * 10) / 10).join(' ')
);
ws.close();
await b.close();
