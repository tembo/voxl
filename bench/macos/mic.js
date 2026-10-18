// macOS microphone: 2 s of a 440 Hz tone as PCM at real-time pace; voxl logs how
// much was dropped (start voxl with --microphone-enabled [--microphone-device NAME]).
// Usage: PORT=6960 node mic.js
const net = require('net');
const s = net.connect(+(process.env.PORT || 6960), '127.0.0.1'); let up = false, buf = Buffer.alloc(0), micOn = null;
const frame = (op, p) => { const m = Buffer.from([1,2,3,4]); const h = p.length < 126 ? Buffer.from([0x80|op, 0x80|p.length]) : Buffer.from([0x80|op, 0x80|126, p.length>>8, p.length&255]); return Buffer.concat([h, m, Buffer.from(p.map((b,i)=>b^m[i&3]))]); };
const send = (t) => s.write(frame(1, Buffer.from(t)));
s.on('connect', () => s.write('GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n'));
s.on('data', (d) => { buf = Buffer.concat([buf, d]); if (!up) { const i = buf.indexOf('\r\n\r\n'); if (i < 0) return; up = true; buf = buf.subarray(i+4); go(); }
  for (;;) { if (buf.length < 2) return; let len = buf[1]&127, off = 2; if (len===126) { if (buf.length<4) return; len = buf.readUInt16BE(2); off = 4; } else if (len===127) { if (buf.length<10) return; len = Number(buf.readBigUInt64BE(2)); off = 10; }
    if (buf.length < off+len) return; const op = buf[0]&15, p = buf.subarray(off, off+len); buf = buf.subarray(off+len);
    if (op === 1) { const j = JSON.parse(p.toString()); if (j.t === 'hello') console.log('hello mic', j.mic); if (j.t === 'mic') micOn = j.on; } } });
const sleep = (ms) => new Promise(r => setTimeout(r, ms));
async function go() {
  await sleep(300); send('mi,1'); await sleep(200); console.log('mic on ack', micOn);
  let t = 0; const t0 = Date.now();
  for (let i = 0; i < 100; i++) {
    const pcm = Buffer.alloc(1 + 960 * 2); pcm[0] = 0x13;
    for (let j = 0; j < 960; j++, t++) pcm.writeInt16LE(Math.round(3000 * Math.sin(2 * Math.PI * 440 * t / 48000)), 1 + j * 2);
    s.write(frame(2, pcm));
    const due = t0 + (i + 1) * 20 - Date.now(); if (due > 0) await sleep(due);
  }
  await sleep(200); send('mi,0'); await sleep(300); process.exit(0);
}
