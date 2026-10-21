// macOS performance: a headed Chrome animates the main display while a headless
// viewer decodes the stream; prints viewer FPS, pipeline and encode times.
// Usage: PORT=6960 node perf.js   (needs playwright-core and Google Chrome)
const { chromium } = require(process.env.PLAYWRIGHT_CORE || 'playwright-core');
const exe = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const PORT = process.env.PORT || 6960;
const stats = async () => (await fetch(`http://127.0.0.1:${PORT}/stats`)).json();
(async () => {
	const anim = await chromium.launch({ executablePath: exe, headless: false, args: ['--window-position=0,0', '--window-size=1920,1080', '--start-fullscreen'] });
	const ap = await anim.newPage({ viewport: null });
	await ap.setContent(`<body style="margin:0;overflow:hidden;background:#111"><canvas id=c></canvas><script>
		const c = document.getElementById('c'), x = c.getContext('2d');
		function f(t) { c.width = innerWidth; c.height = innerHeight;
			for (let i = 0; i < 40; i++) { x.fillStyle = 'hsl(' + ((t / 10 + i * 9) % 360) + ',70%,50%)'; x.fillRect((t / 3 + i * 97) % innerWidth, (i * 53 + t / 7) % innerHeight, 180, 120); }
			x.fillStyle = '#fff'; x.font = '28px sans-serif'; for (let i = 0; i < 30; i++) x.fillText('voxl on macOS — scrolling text line ' + i, 40, ((i * 40 - t / 4) % innerHeight + innerHeight) % innerHeight);
			requestAnimationFrame(f); } requestAnimationFrame(f);</script></body>`);
	await ap.waitForTimeout(1500);
	const viewer = await chromium.launch({ executablePath: exe, args: ['--enable-gpu'] });
	const vp = await viewer.newPage({ viewport: { width: 1600, height: 900 } });
	await vp.goto(`http://127.0.0.1:${PORT}/?stats`);
	await vp.waitForTimeout(2000);
	const s0 = await stats(), t0 = Date.now();
	const fps = [];
	for (let i = 0; i < 6; i++) { await vp.waitForTimeout(1000); fps.push(await vp.evaluate(() => window.voxlStats)); }
	const s1 = await stats(), dt = (Date.now() - t0) / 1000;
	await vp.screenshot({ path: 'perf.png' });
	const n = s1.encoded - s0.encoded;
	console.log(JSON.stringify({
		viewerFps: fps.map((s) => +s.fps.toFixed(1)),
		mbps: +(fps[fps.length - 1].kbps / 1000).toFixed(2),
		pipeMs: +fps[fps.length - 1].pipeMs.toFixed(1), decodeMs: +fps[fps.length - 1].decodeMs.toFixed(1),
		serverFps: +(n / dt).toFixed(1),
		encodeMs: +((s1.encode_us_total - s0.encode_us_total) / n / 1000).toFixed(2),
		convertMs: +((s1.convert_us_total - s0.convert_us_total) / n / 1000).toFixed(2),
		flowBlocked: s1.flow_blocked - s0.flow_blocked,
	}));
	await viewer.close(); await anim.close();
})();
