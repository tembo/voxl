import { createRequire } from 'module';
const require = createRequire(
	process.env.PLAYWRIGHT_NODE_MODULES || process.cwd() + '/node_modules/'
);
const { chromium } = require('playwright');
const exe = process.env.CHROMIUM || 'chromium';
const target = await chromium.launch({
	executablePath: exe,
	headless: false,
	env: { ...process.env, DISPLAY: process.env.BENCH_DISPLAY || ':3' },
	args: [
		'--no-sandbox',
		'--window-position=0,0',
		'--window-size=1600,1000',
		'--no-first-run',
	],
});
const tp = await target.newPage();
await tp.goto('file://' + new URL('target.html', import.meta.url).pathname);
await tp.bringToFront();
await tp.waitForTimeout(1500);
const st = async () =>
	tp.evaluate(() => ({
		y: Math.round(scrollY),
		clicks: __s.c(),
		ctx: __s.x(),
	}));
const drv = await chromium.launch({
	executablePath: exe,
	args: ['--no-sandbox'],
});

// 1) Mouse wheel / trackpad through the client
const ctx1 = await drv.newContext({ viewport: { width: 1280, height: 800 } });
const p = await ctx1.newPage();
await p.goto('http://localhost:' + (process.env.VOXL_PORT || '6903') + '/');
await p.waitForTimeout(2500);
await p.mouse.move(640, 400);
const ys = [(await st()).y];
for (let i = 0; i < 8; i++) {
	await p.mouse.wheel(0, 12);
	await p.waitForTimeout(60);
	ys.push((await st()).y);
}
console.log('trackpad-style wheel (12px steps): scrollY', ys.join(' '));
await tp.evaluate(() => scrollTo(0, 0));
await tp.waitForTimeout(200);
await p.mouse.wheel(0, 100);
await p.waitForTimeout(400);
console.log('one mouse notch (100px):', (await st()).y);
await ctx1.close();

// 2) Touch: tap, two-finger scroll, long press, drag
await tp.evaluate(() => scrollTo(0, 0));
await tp.waitForTimeout(200);
const ctx2 = await drv.newContext({
	viewport: { width: 1280, height: 800 },
	hasTouch: true,
	isMobile: false,
});
const t = await ctx2.newPage();
await t.goto('http://localhost:' + (process.env.VOXL_PORT || '6903') + '/');
await t.waitForTimeout(2500);
const cdp = await ctx2.newCDPSession(t);
const touch = (type, pts) =>
	cdp.send('Input.dispatchTouchEvent', {
		type,
		touchPoints: pts.map(([x, y], id) => ({ x, y, id })),
	});
const before = await st();
await touch('touchStart', [[400, 300]]);
await touch('touchEnd', []);
await t.waitForTimeout(400);
const afterTap = await st();
console.log('tap -> clicks', before.clicks, '->', afterTap.clicks);
await touch('touchStart', [
	[400, 500],
	[500, 500],
]);
for (let i = 1; i <= 10; i++) {
	await touch('touchMove', [
		[400, 500 - i * 20],
		[500, 500 - i * 20],
	]);
	await t.waitForTimeout(16);
}
await touch('touchEnd', []);
await t.waitForTimeout(400);
const afterScroll = await st();
console.log(
	'two-finger swipe up 200px -> scrollY',
	afterTap.y,
	'->',
	afterScroll.y,
	'(clicks unchanged:',
	afterScroll.clicks === afterTap.clicks,
	')'
);
await touch('touchStart', [[400, 300]]);
await t.waitForTimeout(800);
await touch('touchEnd', []);
await t.waitForTimeout(400);
console.log(
	'long press -> contextmenu',
	afterScroll.ctx,
	'->',
	(await st()).ctx
);
const toolsVisible = await t.evaluate(() => {
	const e = document.getElementById('tools');
	return (
		e.classList.contains('touch') &&
		getComputedStyle(document.getElementById('kb-btn')).display !== 'none'
	);
});
console.log('touch toolbar with keyboard button visible:', toolsVisible);
await t.screenshot({ path: '/tmp/voxl-touch-ui.png' });
await ctx2.close();
await drv.close();
await target.close();
