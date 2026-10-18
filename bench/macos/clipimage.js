// macOS clipboard images both ways (restores the text clipboard afterwards).
// Usage: PORT=6960 node clipimage.js
const net = require('net'); const { execSync } = require('child_process'); const fs = require('fs');
const sh = (c) => execSync(c, { encoding: 'utf8' }).trim();
const s = net.connect(+(process.env.PORT || 6960), '127.0.0.1'); let up = false, buf = Buffer.alloc(0), got = null;
const frame = (op, p) => { const m = Buffer.from([1,2,3,4]); let h; if (p.length < 126) h = Buffer.from([0x80|op, 0x80|p.length]); else if (p.length < 65536) h = Buffer.from([0x80|op, 0x80|126, p.length>>8, p.length&255]); else { h = Buffer.alloc(10); h[0] = 0x80|op; h[1] = 0x80|127; h.writeBigUInt64BE(BigInt(p.length), 2); } return Buffer.concat([h, m, Buffer.from(p.map((b,i)=>b^m[i&3]))]); };
s.on('connect', () => s.write('GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n'));
s.on('data', (d) => { buf = Buffer.concat([buf, d]); if (!up) { const i = buf.indexOf('\r\n\r\n'); if (i < 0) return; up = true; buf = buf.subarray(i+4); go(); }
  for (;;) { if (buf.length < 2) return; let len = buf[1]&127, off = 2; if (len===126) { if (buf.length<4) return; len = buf.readUInt16BE(2); off = 4; } else if (len===127) { if (buf.length<10) return; len = Number(buf.readBigUInt64BE(2)); off = 10; }
    if (buf.length < off+len) return; const op = buf[0]&15, p = buf.subarray(off, off+len); buf = buf.subarray(off+len); if (op === 2 && p[0] === 4) got = p.subarray(1); } });
const sleep = (ms) => new Promise(r => setTimeout(r, ms));
async function go() {
  const saved = sh('pbpaste');
  // A real PNG: a 64x64 screenshot crop of the menu bar area.
  sh('screencapture -x -R0,0,64,64 -t png /tmp/voxl-clip.png');
  const png = fs.readFileSync('/tmp/voxl-clip.png');
  s.write(frame(2, Buffer.concat([Buffer.from([0x11]), png])));
  await sleep(500);
  const info = sh(`osascript -e 'clipboard info'`);
  console.log((/PNGf/.test(info) && /TIFF/.test(info) ? 'PASS' : 'FAIL') + ' viewer -> Mac image: ' + info.slice(0, 80));
  got = null;
  // A different image than the one just pasted (that one is not echoed back).
  sh('screencapture -x -R64,0,48,48 -t png /tmp/voxl-clip.png');
  sh(`osascript -e 'set the clipboard to (read (POSIX file "/tmp/voxl-clip.png") as «class PNGf»)'`);
  await sleep(800);
  console.log((got && got.subarray(1, 4).toString() === 'PNG' ? 'PASS' : 'FAIL') + ` Mac -> viewer image (${got ? got.length : 0} bytes PNG)`);
  sh(`printf '%s' ${JSON.stringify(saved)} | pbcopy`); fs.unlinkSync('/tmp/voxl-clip.png');
  process.exit(0);
}
