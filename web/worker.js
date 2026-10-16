// Owns the WebSocket, decodes H.264 with WebCodecs and paints straight onto an
// OffscreenCanvas the moment each frame is decoded (no rAF queueing).
'use strict';

const noDraw = /nodraw/.test(self.location.search);
let presentToMain = false;
let ws = null, url = null, canvas = null, ctx = null;
let audioDecoder = null, audioPort = null, audioTs = 0;
let wantVideo = true, wantAudio = false;
// The screen arrives as independent H.264 stripes, one decoder each. A frame is a
// set of stripe updates sharing an id; frames are presented whole and in order.
let layout = [];          // [y, h] per stripe
const stripes = [];       // per stripe: { dec, codec, waitingKey, ts, ids }
const frames = new Map(); // frame id -> { need, got: [{ i, y, frame }] }
const order = [];         // frame ids awaiting presentation, oldest first
let kfRequested = 0;
const st = { frames: 0, bytes: 0, decodeMs: 0, pipeMs: 0, lastReport: performance.now(), dropped: 0 };

function send(s) { if (ws && ws.readyState === 1) ws.send(s); }
function post(m, t) { self.postMessage(m, t || []); }

function connect() {
  ws = new WebSocket(url);
  ws.binaryType = 'arraybuffer';
  ws.onopen = () => {
    post({ t: 'status', s: 'open' });
    send('v,' + (wantVideo ? 1 : 0));
    send('au,' + (wantAudio ? 1 : 0));
  };
  ws.onclose = () => {
    post({ t: 'status', s: 'closed' });
    resetDecoder();
    setTimeout(connect, 1000);
  };
  ws.onerror = () => {};
  ws.onmessage = (e) => {
    if (typeof e.data === 'string') return onText(e.data);
    const b = new Uint8Array(e.data);
    switch (b[0]) {
      case 0x01: onVideo(e.data, b); break;
      case 0x02: onAudio(b); break;
      case 0x03: post({ t: 'cursor', buf: e.data }, [e.data]); break;
      case 0x04: post({ t: 'clipImage', buf: e.data }, [e.data]); break;
    }
  };
}

function onText(s) {
  let m; try { m = JSON.parse(s); } catch { return; }
  if (m.t === 'ping') { send('po,' + m.s); return; }
  if (m.t === 'hello') {
    if (canvas.width !== m.w || canvas.height !== m.h) { canvas.width = m.w; canvas.height = m.h; }
    ctx = canvas.getContext('2d', { alpha: false, desynchronized: true });
    layout = m.stripes && m.stripes.length ? m.stripes : [[0, m.h]];
    resetDecoder();
  }
  if (m.t === 'close') { try { ws.close(); } catch {} return; }
  post(m);
}

function resetDecoder() {
  for (const sd of stripes) if (sd && sd.dec && sd.dec.state !== 'closed') { try { sd.dec.close(); } catch {} }
  stripes.length = 0;
  for (const f of frames.values()) for (const g of f.got) g.frame.close();
  frames.clear(); order.length = 0;
}

function requestKeyframe() {
  const now = performance.now();
  if (now - kfRequested > 500) { kfRequested = now; send('kf'); }
}

// avc1.PPCCLL from the first SPS in an Annex-B keyframe.
function codecFromSps(d) {
  for (let i = 0; i + 4 < d.length; i++) {
    if (d[i] === 0 && d[i + 1] === 0 && d[i + 2] === 1 && (d[i + 3] & 0x1f) === 7) {
      const h = (x) => x.toString(16).padStart(2, '0');
      return 'avc1.' + h(d[i + 4]) + h(d[i + 5]) + h(d[i + 6]);
    }
  }
  return null;
}

function stripeState(i) {
  return stripes[i] || (stripes[i] = { dec: null, codec: null, waitingKey: true, ts: 0, ids: new Map() });
}

function makeDecoder(i, c) {
  const sd = stripeState(i);
  const dec = new VideoDecoder({
    output: (frame) => {
      const id = sd.ids.get(frame.timestamp);
      sd.ids.delete(frame.timestamp);
      const f = frames.get(id);
      if (!f) { frame.close(); return; }
      f.got.push({ i, y: layout[i] ? layout[i][0] : 0, frame });
      flush();
    },
    error: (e) => {
      post({ t: 'log', s: `stripe ${i} decoder error: ${e.message}` });
      resetDecoder();
      requestKeyframe();
    },
  });
  dec.configure({ codec: c, optimizeForLatency: true, hardwareAcceleration: 'no-preference' });
  return dec;
}

// Present every complete frame at the head of the queue.
function flush() {
  while (order.length) {
    const id = order[0];
    const f = frames.get(id);
    if (f && f.got.length < f.need) return;
    order.shift(); frames.delete(id);
    if (!f) continue;
    if (f.got.length) {
      if (presentToMain) post({ t: 'frames', parts: f.got }, f.got.map((g) => g.frame));
      else { for (const g of f.got) { if (ctx && !noDraw) ctx.drawImage(g.frame, 0, g.y); g.frame.close(); } }
    }
    send('a,' + id);
    st.frames++;
    st.decodeMs += performance.now() - f.t0;
  }
}

function onVideo(buf, b) {
  const key = (b[1] & 1) === 1;
  const id = ((b[2] << 24) | (b[3] << 16) | (b[4] << 8) | b[5]) >>> 0;
  const pipe = ((b[6] << 24) | (b[7] << 16) | (b[8] << 8) | b[9]) >>> 0;
  const i = b[10], parts = b[12];
  const data = b.subarray(16);
  st.bytes += b.length;
  let f = frames.get(id);
  if (!f) {
    f = { need: parts, got: [], t0: performance.now() };
    frames.set(id, f); order.push(id);
    st.pipeMs += pipe / 10;
  }
  const sd = stripeState(i);
  if (sd.waitingKey && !key) { f.need--; st.dropped++; requestKeyframe(); flush(); return; }
  if (key) {
    const c = codecFromSps(data) || 'avc1.640034';
    if (!sd.dec || sd.dec.state === 'closed' || c !== sd.codec) {
      if (sd.dec && sd.dec.state !== 'closed') { try { sd.dec.close(); } catch {} }
      sd.codec = c; sd.ids.clear();
      try { sd.dec = makeDecoder(i, c); } catch (e) { post({ t: 'log', s: 'configure failed ' + e }); f.need--; flush(); return; }
    }
    sd.waitingKey = false;
  }
  const ts = ++sd.ts;
  sd.ids.set(ts, id);
  try {
    sd.dec.decode(new EncodedVideoChunk({ type: key ? 'key' : 'delta', timestamp: ts, data }));
  } catch (e) {
    post({ t: 'log', s: 'decode threw ' + e });
    resetDecoder();
    requestKeyframe();
  }
}

function onAudio(b) {
  if (!audioPort || typeof AudioDecoder === 'undefined') return;
  if (!audioDecoder) {
    audioDecoder = new AudioDecoder({
      output: (ad) => {
        const n = ad.numberOfFrames;
        const l = new Float32Array(n), r = new Float32Array(n);
        ad.copyTo(l, { planeIndex: 0, format: 'f32-planar' });
        if (ad.numberOfChannels > 1) ad.copyTo(r, { planeIndex: 1, format: 'f32-planar' }); else r.set(l);
        ad.close();
        audioPort.postMessage({ l, r }, [l.buffer, r.buffer]);
      },
      error: () => { audioDecoder = null; },
    });
    audioDecoder.configure({ codec: 'opus', sampleRate: 48000, numberOfChannels: 2 });
  }
  audioTs += 10000;
  try { audioDecoder.decode(new EncodedAudioChunk({ type: 'key', timestamp: audioTs, data: b.subarray(1) })); } catch {}
}

setInterval(() => {
  const now = performance.now();
  const dt = (now - st.lastReport) / 1000;
  const f = st.frames;
  post({ t: 'stats', fps: f / dt, kbps: st.bytes * 8 / 1000 / dt, decodeMs: f ? st.decodeMs / f : 0,
    pipeMs: f ? st.pipeMs / f : 0, codec: stripes[0] && stripes[0].codec, stripes: layout.length, dropped: st.dropped, queue: order.length });
  st.frames = 0; st.bytes = 0; st.decodeMs = 0; st.pipeMs = 0; st.lastReport = now;
}, 1000);

self.onmessage = (e) => {
  const m = e.data;
  if (m.t === 'init') {
    canvas = m.canvas; url = m.url; presentToMain = !!m.presentToMain;
    ctx = canvas.getContext('2d', { alpha: false, desynchronized: true });
    connect();
  } else if (m.t === 'send') {
    send(m.d);
  } else if (m.t === 'sendBin') {
    if (ws && ws.readyState === 1) ws.send(m.d);
  } else if (m.t === 'video') {
    if (wantVideo !== m.on) { wantVideo = m.on; if (m.on) resetDecoder(); send('v,' + (m.on ? 1 : 0)); }
  } else if (m.t === 'audioPort') {
    audioPort = m.port; wantAudio = true; send('au,1');
  }
};
