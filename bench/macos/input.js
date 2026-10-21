// macOS input against a page in a headed Chrome with a temporary profile: clicks,
// double/right/middle click, typing (symbols, accents, CJK, emoji), Cmd shortcuts,
// Shift selection, drag selection, pixel and wheel scrolling, cursor shape.
// Usage: PORT=6960 node input.js   (needs playwright-core and Google Chrome)
const { chromium } = require(process.env.PLAYWRIGHT_CORE || 'playwright-core');
const net = require('net');
const exe = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const PORT = +(process.env.PORT || 6960);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let failed = 0;
const check = (ok, what) => { console.log(`${ok ? 'PASS' : 'FAIL'} ${what}`); if (!ok) failed++; };
function ws() {
	const s = net.connect(PORT, '127.0.0.1'); let up = false, buf = Buffer.alloc(0); const c = { hello: null };
	c.send = (t) => { const p = Buffer.from(t), m = Buffer.from([1,2,3,4]); const h = p.length < 126 ? Buffer.from([0x81, 0x80|p.length]) : Buffer.from([0x81, 0x80|126, p.length>>8, p.length&255]); s.write(Buffer.concat([h, m, Buffer.from(p.map((b,i)=>b^m[i&3]))])); };
	c.ready = new Promise((res) => {
		s.on('connect', () => s.write('GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n'));
		s.on('data', (d) => { buf = Buffer.concat([buf, d]); if (!up) { const i = buf.indexOf('\r\n\r\n'); if (i < 0) return; up = true; buf = buf.subarray(i+4); }
			for (;;) { if (buf.length < 2) return; let len = buf[1]&127, off = 2; if (len===126) { if (buf.length<4) return; len = buf.readUInt16BE(2); off = 4; } else if (len===127) { if (buf.length<10) return; len = Number(buf.readBigUInt64BE(2)); off = 10; }
				if (buf.length < off+len) return; const op = buf[0]&15, p = buf.subarray(off, off+len); buf = buf.subarray(off+len);
				if (op === 1) { const j = JSON.parse(p.toString()); if (j.t === 'hello') { c.hello = j; res(); } } else if (p[0] === 3) c.cursor = [p.readUInt16BE(2), p.readUInt16BE(4), p.readUInt16BE(6), p.readUInt16BE(8)].join(','); } });
	});
	return c;
}
const ks = (ch) => { const cp = ch.codePointAt(0); return (cp >= 0x20 && cp <= 0x7e) || (cp >= 0xa0 && cp <= 0xff) ? cp : 0x01000000 + cp; };
const SHIFT = 0xffe1, SUPER = 0xffeb;
(async () => {
	const b = await chromium.launch({ executablePath: exe, headless: false, args: ['--window-position=100,100', '--window-size=1200,800'] });
	const p = await b.newPage({ viewport: null });
	await p.setContent(`<body style="margin:0;font:20px sans-serif">
		<input id=t style="position:absolute;left:40px;top:40px;width:600px;font-size:20px" autocomplete=off spellcheck=false>
		<button id=btn style="position:absolute;left:700px;top:40px">click me</button>
		<p id=para style="position:absolute;left:40px;top:120px;margin:0">alpha beta gamma delta</p>
		<div style="height:5000px"></div>
		<script>window.log=[];btn.onclick=()=>log.push('click');btn.ondblclick=()=>log.push('dbl');btn.oncontextmenu=(e)=>{e.preventDefault();log.push('ctx')};
		btn.onauxclick=(e)=>log.push('aux'+e.button);</script></body>`);
	await p.bringToFront();
	await sleep(800);
	const c = ws(); await c.ready;
	const k = c.hello.resize ? c.hello.resize.scale : c.hello.w / 1920;
	const geo = await p.evaluate(() => [screenX, screenY + (outerHeight - innerHeight)]);
	const at = async (sel, dx = 0.5, dy = 0.5) => { const r = await p.evaluate((s) => { const b = document.querySelector(s).getBoundingClientRect(); return [b.x, b.y, b.width, b.height]; }, sel); return [Math.round((geo[0] + r[0] + r[2] * dx) * k), Math.round((geo[1] + r[1] + r[3] * dy) * k)]; };
	const move = async (sel, dx, dy) => { const [x, y] = await at(sel, dx, dy); c.send(`m,${x},${y}`); await sleep(60); };
	const click = async (btn = 1, n = 1) => { for (let i = 0; i < n; i++) { c.send(`b,${btn},1`); c.send(`b,${btn},0`); } await sleep(250); };
	const key = (sym) => { c.send(`k,${sym},1`); c.send(`k,${sym},0`); };
	const type = async (text) => { for (const ch of text) { const up = ch !== ch.toLowerCase(); if (up) c.send(`k,${SHIFT},1`); key(ks(ch)); if (up) c.send(`k,${SHIFT},0`); await sleep(12); } };
	const log = () => p.evaluate(() => window.log.splice(0));

	await move('#t'); await sleep(300); const overText = c.cursor;
	await move('#btn'); await sleep(300); const overButton = c.cursor;
	check(overText && overButton && overText !== overButton, `cursor shape follows (w,h,hotx,hoty): over text ${overText}, over button ${overButton}`);
	await click(1);
	check(JSON.stringify(await log()) === '["click"]', 'left click');
	await click(1, 2);
	check((await log()).includes('dbl'), 'double click');
	await click(3);
	check((await log()).includes('ctx'), 'right click (context menu)');
	await click(2);
	check((await log()).includes('aux1'), 'middle click');

	await move('#t'); await click(1);
	const text = 'Hello Mac! @#$%^&*()_+{}|:"<>? é ü ß 你好 😀';
	await type(text);
	await sleep(300);
	let v = await p.$eval('#t', (e) => e.value);
	check(v === text, `typing (US layout, shifted symbols, accents, CJK, emoji): ${JSON.stringify(v)}`);
	// Cmd+A, Cmd+X, then Cmd+V twice.
	c.send(`k,${SUPER},1`); await sleep(50);
	for (const ch of 'axvv') { key(ks(ch)); await sleep(120); }
	c.send(`k,${SUPER},0`);
	await sleep(400);
	v = await p.$eval('#t', (e) => e.value);
	check(v === text + text, `Cmd+A / Cmd+X / Cmd+V: ${JSON.stringify(v.slice(0, 30))}…`);
	// Shift+Left x3 then Backspace.
	c.send(`k,${SHIFT},1`); for (let i = 0; i < 3; i++) key(0xff51); c.send(`k,${SHIFT},0`); key(0xff08);
	await sleep(300);
	v = await p.$eval('#t', (e) => e.value);
	check(v === (text + text).slice(0, -4), `Shift+Left selection + Backspace (emoji is 2 UTF-16 units): ${JSON.stringify(v.slice(-8))}`);

	// Drag to select "beta gamma" in the paragraph.
	await move('#para', 0.27, 0.5); c.send('b,1,1');
	for (const f of [0.35, 0.45, 0.55, 0.85]) await move('#para', f, 0.5);
	c.send('b,1,0'); await sleep(200);
	const sel = await p.evaluate(() => getSelection().toString());
	check(/beta gamma/.test(sel), `drag selects text: ${JSON.stringify(sel)}`);

	// Pixel scrolling: 300 desktop px down, then back.
	await move('#para', 0.5, 3);
	const y0 = await p.evaluate(() => scrollY);
	for (let i = 0; i < 10; i++) { c.send('sp,0,30'); await sleep(16); }
	await sleep(600);
	const y1 = await p.evaluate(() => scrollY);
	check(Math.abs((y1 - y0) - 300 / k) < 300 / k * 0.25, `pixel scroll: ${y1 - y0}px of page for 300 desktop px (expect ~${Math.round(300 / k)})`);
	c.send('s,0,-3'); await sleep(600);
	const y2 = await p.evaluate(() => scrollY);
	check(y2 < y1, `wheel clicks scroll up: ${y1} -> ${y2}`);
	await b.close();
	console.log(failed ? `${failed} failed` : 'all passed');
	process.exit(failed ? 1 : 0);
})();
