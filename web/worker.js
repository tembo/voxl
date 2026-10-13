// Owns the WebSocket, decodes H.264 with WebCodecs and paints straight onto an
// OffscreenCanvas the moment each frame is decoded (no rAF queueing).
'use strict';

const noDraw = /nodraw/.test(self.location.search);
let presentToMain = false;
let ws = null, url = null, canvas = null, ctx = null;
let decoder = null, codec = null, waitingKey = true;
let audioDecoder = null, audioPort = null, audioTs = 0;
let wantVideo = true, wantAudio = false;
const pendingIds = new Map(); // chunk timestamp -> frame id
let tsCounter = 0;
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
    }
  };
}

function onText(s) {
  let m; try { m = JSON.parse(s); } catch { return; }
  if (m.t === 'ping') { send('po,' + m.s); return; }
  if (m.t === 'hello') {
    if (canvas.width !== m.w || canvas.height !== m.h) { canvas.width = m.w; canvas.height = m.h; }
    ctx = canvas.getContext('2d', { alpha: false, desynchronized: true });
  }
  if (m.t === 'close') { try { ws.close(); } catch {} return; }
  post(m);
}

function resetDecoder() {
  if (decoder && decoder.state !== 'closed') { try { decoder.close(); } catch {} }
  decoder = null; codec = null; waitingKey = true; pendingIds.clear();
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

function makeDecoder(c) {
  const dec = new VideoDecoder({
    output: (frame) => {
      const id = pendingIds.get(frame.timestamp);
      pendingIds.delete(frame.timestamp);
      if (presentToMain) post({ t: 'frame', frame }, [frame]);
      else { if (ctx && !noDraw) ctx.drawImage(frame, 0, 0); frame.close(); }
      if (id !== undefined) {
        send('a,' + id);
        st.frames++;
        const t = decodeStart.get(id);
        if (t !== undefined) { st.decodeMs += performance.now() - t; decodeStart.delete(id); }
      }
    },
    error: (e) => {
      post({ t: 'log', s: 'decoder error: ' + e.message });
      resetDecoder();
      send('kf');
    },
  });
  dec.configure({ codec: c, optimizeForLatency: true, hardwareAcceleration: 'no-preference' });
  return dec;
}

const decodeStart = new Map();

function onVideo(buf, b) {
  const flags = b[1];
  const key = (flags & 1) === 1;
  const id = ((b[2] << 24) | (b[3] << 16) | (b[4] << 8) | b[5]) >>> 0;
  const pipe = ((b[6] << 24) | (b[7] << 16) | (b[8] << 8) | b[9]) >>> 0;
  const data = b.subarray(10);
  st.bytes += b.length;
  st.pipeMs += pipe / 10;
  if (waitingKey && !key) { send('a,' + id); st.dropped++; return; }
  if (key) {
    const c = codecFromSps(data) || 'avc1.640034';
    if (!decoder || decoder.state === 'closed' || c !== codec) {
      if (decoder && decoder.state !== 'closed') { try { decoder.close(); } catch {} }
      codec = c;
      try { decoder = makeDecoder(c); } catch (e) { post({ t: 'log', s: 'configure failed ' + e }); return; }
    }
    waitingKey = false;
  }
  const ts = ++tsCounter;
  pendingIds.set(ts, id);
  decodeStart.set(id, performance.now());
  try {
    decoder.decode(new EncodedVideoChunk({ type: key ? 'key' : 'delta', timestamp: ts, data }));
  } catch (e) {
    post({ t: 'log', s: 'decode threw ' + e });
    resetDecoder();
    send('kf');
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
    pipeMs: f ? st.pipeMs / f : 0, codec, dropped: st.dropped, queue: decoder ? decoder.decodeQueueSize : 0 });
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
  } else if (m.t === 'video') {
    if (wantVideo !== m.on) { wantVideo = m.on; if (m.on) resetDecoder(); send('v,' + (m.on ? 1 : 0)); }
  } else if (m.t === 'audioPort') {
    audioPort = m.port; wantAudio = true; send('au,1');
  }
};
