// macOS end to end, as a raw WebSocket client of a running voxl (PORT, default 6960):
// hello, keyframe, cursor image, clipboard text both ways, system audio, typing and
// mouse in a scratch TextEdit document (closed unsaved) and, with --enable-resize,
// a display mode switch and back. Restores the text clipboard; plays one quiet sound.
// Usage: PORT=6960 node features.js
const net = require('net');
const { execSync } = require('child_process');
const PORT = +(process.env.PORT || 6960);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const sh = (c) => execSync(c, { encoding: 'utf8' }).trim();
let failed = 0;
const check = (ok, what) => { console.log(`${ok ? 'PASS' : 'FAIL'} ${what}`); if (!ok) failed++; };

function connect() {
	const s = net.connect(PORT, '127.0.0.1');
	const c = { s, msgs: [], bin: { 1: 0, 2: 0, 3: 0, 4: 0 }, cursor: null, hellos: [], clips: [], keys: 0 };
	let buf = Buffer.alloc(0), up = false;
	c.send = (text) => {
		const p = Buffer.from(text), m = Buffer.from([1, 2, 3, 4]);
		const h = p.length < 126 ? Buffer.from([0x81, 0x80 | p.length]) : Buffer.from([0x81, 0x80 | 126, p.length >> 8, p.length & 255]);
		s.write(Buffer.concat([h, m, Buffer.from(p.map((b, i) => b ^ m[i & 3]))]));
	};
	c.ready = new Promise((res) => {
		s.on('connect', () => s.write(`GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n`));
		s.on('data', (d) => {
			buf = Buffer.concat([buf, d]);
			if (!up) { const i = buf.indexOf('\r\n\r\n'); if (i < 0) return; up = true; buf = buf.subarray(i + 4); res(); }
			for (;;) {
				if (buf.length < 2) return;
				let len = buf[1] & 127, off = 2;
				if (len === 126) { if (buf.length < 4) return; len = buf.readUInt16BE(2); off = 4; }
				else if (len === 127) { if (buf.length < 10) return; len = Number(buf.readBigUInt64BE(2)); off = 10; }
				if (buf.length < off + len) return;
				const op = buf[0] & 15, p = buf.subarray(off, off + len); buf = buf.subarray(off + len);
				if (op === 1) {
					const j = JSON.parse(p.toString());
					if (j.t === 'hello') c.hellos.push(j);
					else if (j.t === 'clip') c.clips.push(j.d);
					else if (j.t === 'ping') c.send('po,' + j.s);
				} else if (op === 2) {
					c.bin[p[0]] = (c.bin[p[0]] || 0) + 1;
					if (p[0] === 1) { if (p[1] & 1) c.keys++; c.send('a,' + p.readUInt32BE(2)); }
					if (p[0] === 3) c.cursor = { w: p.readUInt16BE(2), h: p.readUInt16BE(4), hx: p.readUInt16BE(6), hy: p.readUInt16BE(8), bytes: p.length - 14 };
					if (p[0] === 4) c.clipImage = p.length - 1;
				}
			}
		});
	});
	return c;
}

const XK = { Shift: 0xffe1, Super: 0xffeb, Return: 0xff0d, BackSpace: 0xff08, Left: 0xff51 };
const ks = (ch) => { const cp = ch.codePointAt(0); return (cp >= 0x20 && cp <= 0x7e) || (cp >= 0xa0 && cp <= 0xff) ? cp : 0x01000000 + cp; };

(async () => {
	const c = connect();
	await c.ready;
	c.send('v,1');
	await sleep(1500);
	const h = c.hellos[0];
	check(h && h.os === 'macos' && h.w > 0, `hello: os=${h?.os} ${h?.w}x${h?.h} audio=${h?.audio} mic=${h?.mic} resize=${JSON.stringify(h?.resize)} files=${JSON.stringify(h?.files)}`);
	check(c.keys >= 1 && c.bin[1] >= 1, `keyframe received (${c.bin[1]} video messages)`);
	check(c.cursor && c.cursor.w > 0 && c.cursor.bytes === c.cursor.w * c.cursor.h * 4, `cursor image ${JSON.stringify(c.cursor)}`);

	// Clipboard both ways (the user's text clipboard is restored afterwards).
	const saved = sh('pbpaste');
	sh(`printf 'voxl mac clipboard %s' ${Date.now()} | pbcopy`);
	const want = sh('pbpaste');
	await sleep(800);
	check(c.clips.includes(want), `Mac -> viewer clipboard (${JSON.stringify(c.clips.slice(-1))})`);
	c.send('c,from the viewer ✓');
	await sleep(300);
	check(sh('pbpaste') === 'from the viewer ✓', `viewer -> Mac clipboard (${JSON.stringify(sh('pbpaste'))})`);

	// Audio: a quiet system sound while subscribed.
	c.send('au,1');
	await sleep(1200);
	const a0 = c.bin[2] || 0;
	sh('afplay -v 0.15 /System/Library/Sounds/Tink.aiff');
	await sleep(500);
	check((c.bin[2] || 0) - a0 > 5, `system audio as Opus (${(c.bin[2] || 0) - a0} packets during a sound)`);
	c.send('au,0');

	// Keyboard and mouse in a scratch TextEdit document (closed without saving).
	sh(`osascript -e 'tell application "TextEdit" to make new document' -e 'tell application "TextEdit" to activate'`);
	await sleep(1200);
	const b = sh(`osascript -e 'tell application "TextEdit" to get bounds of window 1'`).split(', ').map(Number);
	const mainWidth = +sh(`osascript -l JavaScript -e "ObjC.import('AppKit'); $.NSScreen.screens.objectAtIndex(0).frame.size.width"`);
	const k = h.w / mainWidth;
	const px = (x, y) => [Math.round(x * k), Math.round(y * k)];
	// Click into the document body.
	const [cx, cy] = px((b[0] + b[2]) / 2, (b[1] + b[3]) / 2);
	c.send(`m,${cx},${cy}`); await sleep(100);
	c.send('b,1,1'); c.send('b,1,0'); await sleep(300);
	const type = async (text) => {
		for (const ch of text) {
			const upper = ch !== ch.toLowerCase();
			if (upper) c.send(`k,${XK.Shift},1`);
			c.send(`k,${ks(ch)},1`); c.send(`k,${ks(ch)},0`);
			if (upper) c.send(`k,${XK.Shift},0`);
			await sleep(15);
		}
	};
	await type('Hello Mac, world! @#$ é ü 你好');
	c.send(`k,${XK.Return},1`); c.send(`k,${XK.Return},0`);
	await type('second line');
	// Shortcuts: Cmd+Left (line start), then Backspace joins the lines.
	c.send(`k,${XK.Super},1`); c.send(`k,${XK.Left},1`); c.send(`k,${XK.Left},0`); c.send(`k,${XK.Super},0`);
	c.send(`k,${XK.BackSpace},1`); c.send(`k,${XK.BackSpace},0`);
	await sleep(400);
	let text = sh(`osascript -e 'tell application "TextEdit" to get text of document 1'`);
	check(text === 'Hello Mac, world! @#$ é ü 你好second line', `typing, Return, Cmd+Left, Backspace: ${JSON.stringify(text)}`);
	// Double-click selects a word; typing replaces it.
	const pos = sh(`osascript -e 'tell application "System Events" to tell process "TextEdit" to get position of text area 1 of scroll area 1 of window 1'`).split(', ').map(Number);
	const [wx, wy] = px(pos[0] + 22, pos[1] + 12);
	c.send(`m,${wx},${wy}`); await sleep(100);
	c.send('b,1,1'); c.send('b,1,0'); c.send('b,1,1'); c.send('b,1,0'); await sleep(300);
	await type('Howdy');
	await sleep(300);
	text = sh(`osascript -e 'tell application "TextEdit" to get text of document 1'`);
	check(text.startsWith('Howdy Mac'), `double-click selects a word: ${JSON.stringify(text.slice(0, 20))}`);
	// Cmd+A, Cmd+C: shortcut and Mac -> viewer clipboard again.
	c.send(`k,${XK.Super},1`); c.send(`k,${ks('a')},1`); c.send(`k,${ks('a')},0`); c.send(`k,${ks('c')},1`); c.send(`k,${ks('c')},0`); c.send(`k,${XK.Super},0`);
	await sleep(800);
	check(c.clips[c.clips.length - 1] === text, `Cmd+A Cmd+C copies the document (${JSON.stringify(c.clips[c.clips.length - 1]?.slice(0, 20))})`);
	sh(`osascript -e 'tell application "TextEdit" to close document 1 saving no'`);
	sh(`printf '%s' ${JSON.stringify(saved)} | pbcopy`);

	// Dynamic resolution: ask for a smaller desktop, then back.
	if (h.resize) {
		const orig = [h.w, h.h];
		c.send(`sz,${Math.round(1280 * h.resize.scale)},${Math.round(720 * h.resize.scale)}`);
		await sleep(2500);
		const h2 = c.hellos[c.hellos.length - 1];
		const pts = sh(`osascript -l JavaScript -e "ObjC.import('AppKit'); const f = $.NSScreen.screens.objectAtIndex(0).frame.size; f.width + 'x' + f.height"`);
		check(h2.w < orig[0], `resize to a 1280x720 window: stream ${h2.w}x${h2.h}, desktop ${pts}`);
		const keysBefore = c.keys;
		c.send(`sz,${orig[0]},${orig[1]}`);
		await sleep(2500);
		const h3 = c.hellos[c.hellos.length - 1];
		check(h3.w === orig[0] && h3.h === orig[1] && c.keys > keysBefore, `back to ${h3.w}x${h3.h} with a keyframe`);
	}
	console.log(`video messages ${c.bin[1]}, cursor ${c.bin[3]}`);
	console.log(failed ? `${failed} failed` : 'all passed');
	process.exit(failed ? 1 : 0);
})();
