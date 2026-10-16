import { createRequire } from 'module';
const require = createRequire(
	process.env.PLAYWRIGHT_NODE_MODULES || process.cwd() + '/node_modules/'
);
const { chromium } = require('playwright');
const b = await chromium.launch({
	executablePath: process.env.CHROMIUM || 'chromium',
	args: ['--no-sandbox'],
});
const report = async (frame, label) => {
	const r = await frame.evaluate(() => {
		const s = document.getElementById('surface').getBoundingClientRect();
		return {
			vw: innerWidth,
			vh: innerHeight,
			surface: [Math.round(s.width), Math.round(s.height)],
			ratio: +(s.width / s.height).toFixed(3),
			fps: window.voxlStats && Math.round(window.voxlStats.fps),
		};
	});
	console.log(label, JSON.stringify(r));
};
// 1) Embedded in a sidebar iframe, resized live
const p = await b.newPage({ viewport: { width: 1600, height: 900 } });
await p.goto('file://' + new URL('embed.html', import.meta.url).pathname);
await p.waitForTimeout(4000);
const f = await (await p.$('#f')).contentFrame();
for (const w of [640, 420, 960]) {
	await p.evaluate((w) => {
		document.getElementById('sb').style.width = w + 'px';
	}, w);
	await p.waitForTimeout(1500);
	await report(f, `sidebar ${w}px:`);
	await p.screenshot({ path: `/tmp/rz-sidebar-${w}.png` });
}
// 2) Opened directly as a tab at several window sizes
const t = await b.newPage({ viewport: { width: 1280, height: 800 } });
await t.goto('http://localhost:' + (process.env.VOXL_PORT || '6902') + '/');
await t.waitForTimeout(3000);
for (const [w, h] of [
	[1280, 800],
	[1920, 1080],
	[900, 1100],
]) {
	await t.setViewportSize({ width: w, height: h });
	await t.waitForTimeout(1500);
	await report(t.mainFrame(), `tab ${w}x${h}:`);
	await t.screenshot({ path: `/tmp/rz-tab-${w}x${h}.png` });
}
await b.close();
