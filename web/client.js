// Main thread: layout, input capture, cursor, clipboard and audio wiring.
// Video decode/paint and the socket live in worker.js.
'use strict';
(() => {
  const surface = document.getElementById('surface');
  const canvas = document.getElementById('screen');
  const video = document.getElementById('video');
  const kbd = document.getElementById('kbd');
  const statusEl = document.getElementById('status');
  const statsEl = document.getElementById('stats');
  const params = new URLSearchParams(location.search);
  const showStats = params.has('stats');
  const isMac = /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);
  const inIframe = window !== window.top;

  let dw = 0, dh = 0, cursorMsg = null, connected = false;
  window.fps = 0;
  window.selkiesStats = {};

  if (!('VideoDecoder' in window) || !canvas.transferControlToOffscreen) {
    statusEl.textContent = 'This browser lacks WebCodecs/OffscreenCanvas support.';
    return;
  }

  const wsUrl = new URL('ws', location.href);
  wsUrl.protocol = location.protocol === 'https:' ? 'wss:' : 'ws:';
  wsUrl.search = location.search;
  const worker = new Worker('worker.js' + location.search);
  // Prefer handing decoded frames to a <video> via MediaStreamTrackGenerator: the
  // compositor scans YUV out directly instead of rasterizing into a 2D canvas.
  const useVideo = params.get('present') !== 'canvas' && typeof MediaStreamTrackGenerator !== 'undefined';
  let writer = null, writing = false, nextFrame = null;
  if (useVideo) {
    const gen = new MediaStreamTrackGenerator({ kind: 'video' });
    writer = gen.writable.getWriter();
    video.srcObject = new MediaStream([gen]);
    video.style.display = 'block';
    canvas.style.display = 'none';
    video.play().catch(() => {});
  }
  function present(frame) {
    if (writing) { if (nextFrame) nextFrame.close(); nextFrame = frame; return; }
    writing = true;
    writer.write(frame).catch(() => {}).finally(() => {
      writing = false;
      if (nextFrame) { const f = nextFrame; nextFrame = null; present(f); }
    });
  }
  const offscreen = canvas.transferControlToOffscreen();
  worker.postMessage({ t: 'init', canvas: offscreen, url: wsUrl.href, presentToMain: useVideo }, [offscreen]);
  const send = (d) => worker.postMessage({ t: 'send', d });

  worker.onmessage = (e) => {
    const m = e.data;
    switch (m.t) {
      case 'frame': present(m.frame); break;
      case 'hello':
        dw = m.w; dh = m.h; connected = true;
        layout();
        statusEl.style.display = 'none';
        break;
      case 'status':
        if (m.s === 'closed') { connected = false; statusEl.style.display = ''; statusEl.textContent = 'Reconnecting…'; }
        if (m.s === 'open') updateVisibility(true);
        break;
      case 'cursor': cursorMsg = m.buf; applyCursor(); break;
      case 'clip': onRemoteClipboard(m.d); break;
      case 'stats':
        window.fps = Math.round(m.fps);
        window.selkiesStats = m;
        if (showStats) {
          statsEl.style.display = 'block';
          statsEl.textContent = `${m.fps.toFixed(1)} fps  ${(m.kbps / 1000).toFixed(2)} Mbps\n` +
            `decode ${m.decodeMs.toFixed(1)} ms  server ${m.pipeMs.toFixed(1)} ms\n${m.codec || ''} q=${m.queue}`;
        }
        break;
      case 'log': console.warn('[selkies-rs]', m.s); break;
    }
  };

  // ---- layout ----
  let scale = 1;
  function layout() {
    if (!dw) return;
    const vw = window.innerWidth, vh = window.innerHeight;
    scale = Math.min(vw / dw, vh / dh) || 1;
    surface.style.width = `${dw * scale}px`;
    surface.style.height = `${dh * scale}px`;
    applyCursor();
  }
  window.addEventListener('resize', () => { layout(); updateVisibility(); });

  // ---- visibility: stop video when nobody can see it ----
  let videoOn = null;
  function updateVisibility(force) {
    const on = !document.hidden && window.innerWidth > 0 && window.innerHeight > 0;
    if (on !== videoOn || force) { videoOn = on; worker.postMessage({ t: 'video', on }); }
  }
  document.addEventListener('visibilitychange', () => { updateVisibility(); if (document.hidden) send('r'); });
  updateVisibility();

  // ---- cursor ----
  let cursorCanvas = document.createElement('canvas');
  function applyCursor() {
    if (!cursorMsg) return;
    const v = new DataView(cursorMsg);
    const w = v.getUint16(2), h = v.getUint16(4), hx = v.getUint16(6), hy = v.getUint16(8);
    if (!w || !h) { surface.style.cursor = 'none'; return; }
    const rgba = new Uint8ClampedArray(cursorMsg, 14, w * h * 4);
    const src = document.createElement('canvas');
    src.width = w; src.height = h;
    src.getContext('2d').putImageData(new ImageData(rgba, w, h), 0, 0);
    // Cursor images are in desktop pixels; show them at the on-screen scale.
    const dpr = window.devicePixelRatio || 1;
    const s = Math.min(scale, 128 / Math.max(w, h));
    const cw = Math.max(1, Math.round(w * s)), ch = Math.max(1, Math.round(h * s));
    const dpw = Math.max(1, Math.round(cw * dpr)), dph = Math.max(1, Math.round(ch * dpr));
    cursorCanvas.width = dpw; cursorCanvas.height = dph;
    const cx = cursorCanvas.getContext('2d');
    cx.imageSmoothingQuality = 'high';
    cx.clearRect(0, 0, dpw, dph);
    cx.drawImage(src, 0, 0, dpw, dph);
    const url = cursorCanvas.toDataURL('image/png');
    const hsx = Math.round(hx * s), hsy = Math.round(hy * s);
    surface.style.cursor = dpr > 1
      ? `-webkit-image-set(url(${url}) ${dpr}x) ${hsx} ${hsy}, url(${url}) ${hsx} ${hsy}, default`
      : `url(${url}) ${hsx} ${hsy}, default`;
    if (dpr > 1 && !surface.style.cursor) surface.style.cursor = `url(${url}) ${hsx} ${hsy}, default`;
  }

  // ---- pointer ----
  function toDesk(e) {
    const r = surface.getBoundingClientRect();
    const x = Math.round((e.clientX - r.left) * dw / r.width);
    const y = Math.round((e.clientY - r.top) * dh / r.height);
    return [Math.max(0, Math.min(dw - 1, x)), Math.max(0, Math.min(dh - 1, y))];
  }
  const BTN = [1, 2, 3, 8, 9];
  let lastMove = '';
  function move(e) {
    if (!dw) return;
    const [x, y] = toDesk(e);
    const s = `m,${x},${y}`;
    if (s !== lastMove) { lastMove = s; send(s); }
  }
  surface.addEventListener('pointermove', move);
  surface.addEventListener('pointerdown', (e) => {
    e.preventDefault();
    kbd.focus({ preventScroll: true });
    startAudio();
    flushClipboardOut();
    move(e);
    try { surface.setPointerCapture(e.pointerId); } catch {}
    send(`b,${BTN[e.button] || 1},1`);
  });
  surface.addEventListener('pointerup', (e) => { e.preventDefault(); move(e); send(`b,${BTN[e.button] || 1},0`); });
  surface.addEventListener('pointercancel', () => send('r'));
  surface.addEventListener('contextmenu', (e) => e.preventDefault());
  let accX = 0, accY = 0;
  surface.addEventListener('wheel', (e) => {
    e.preventDefault();
    move(e);
    const k = e.deltaMode === 1 ? 40 : e.deltaMode === 2 ? 800 : 1;
    accX += e.deltaX * k; accY += e.deltaY * k;
    const STEP = 40;
    const tx = Math.trunc(accX / STEP), ty = Math.trunc(accY / STEP);
    if (tx || ty) { accX -= tx * STEP; accY -= ty * STEP; send(`s,${tx},${ty}`); }
  }, { passive: false });

  // ---- keyboard ----
  const NAMED = {
    Backspace: 0xff08, Tab: 0xff09, Enter: 0xff0d, Escape: 0xff1b, Delete: 0xffff, Home: 0xff50,
    ArrowLeft: 0xff51, ArrowUp: 0xff52, ArrowRight: 0xff53, ArrowDown: 0xff54, PageUp: 0xff55,
    PageDown: 0xff56, End: 0xff57, Insert: 0xff63, CapsLock: 0xffe5, NumLock: 0xff7f, ScrollLock: 0xff14,
    Pause: 0xff13, PrintScreen: 0xff61, ContextMenu: 0xff67, AltGraph: 0xfe03, ' ': 0x20,
  };
  const BY_CODE = {
    ShiftLeft: 0xffe1, ShiftRight: 0xffe2, ControlLeft: 0xffe3, ControlRight: 0xffe4,
    AltLeft: 0xffe9, AltRight: 0xffea, MetaLeft: isMac ? 0xffe3 : 0xffeb, MetaRight: isMac ? 0xffe4 : 0xffec,
    OSLeft: 0xffeb, OSRight: 0xffec, NumpadEnter: 0xff0d,
  };
  for (let i = 1; i <= 24; i++) NAMED['F' + i] = 0xffbd + i;
  function keysymOf(e) {
    if (BY_CODE[e.code] !== undefined) {
      if (e.code === 'AltRight' && e.key === 'AltGraph') return 0xfe03;
      return BY_CODE[e.code];
    }
    const k = e.key;
    if (NAMED[k] !== undefined) return NAMED[k];
    if (!k || k === 'Dead' || k === 'Unidentified' || k === 'Process') return 0;
    const cp = k.codePointAt(0);
    if ([...k].length !== 1) return 0;
    if ((cp >= 0x20 && cp <= 0x7e) || (cp >= 0xa0 && cp <= 0xff)) return cp;
    return 0x01000000 + cp;
  }
  const down = new Map(); // code -> keysym
  let pendingPaste = null;
  function press(code, ks) { down.set(code, ks); send(`k,${ks},1`); }
  function release(code) {
    const ks = down.get(code);
    if (ks === undefined) return;
    down.delete(code); send(`k,${ks},0`);
  }
  function releaseAll() { down.clear(); send('r'); }
  function flushPaste() {
    if (!pendingPaste) return;
    const p = pendingPaste; pendingPaste = null;
    clearTimeout(p.timer);
    press(p.code, p.ks);
  }
  function onKeyDown(e) {
    if (e.isComposing) return;
    startAudio();
    flushClipboardOut();
    const ks = keysymOf(e);
    if (!ks) return;
    const accel = isMac ? e.metaKey : e.ctrlKey;
    if (accel && !e.altKey && (e.key === 'v' || e.key === 'V') && !e.repeat) {
      // Let the browser deliver a paste event to the hidden textarea first, so the
      // remote clipboard holds the local contents before the shortcut lands.
      pendingPaste = { code: e.code, ks, timer: setTimeout(flushPaste, 150) };
      return;
    }
    e.preventDefault();
    if (e.repeat && down.has(e.code)) { send(`k,${down.get(e.code)},1`); return; }
    press(e.code, ks);
  }
  function onKeyUp(e) {
    e.preventDefault();
    if (pendingPaste && pendingPaste.code === e.code) flushPaste();
    release(e.code);
    // macOS swallows keyups of keys released while Cmd is held.
    if (isMac && (e.code === 'MetaLeft' || e.code === 'MetaRight')) {
      for (const code of [...down.keys()]) if (!/^(Shift|Control|Alt|Meta)/.test(code)) release(code);
    }
  }
  for (const el of [kbd, surface]) {
    el.addEventListener('keydown', onKeyDown);
    el.addEventListener('keyup', onKeyUp);
  }
  kbd.addEventListener('paste', (e) => {
    e.preventDefault();
    const text = e.clipboardData && e.clipboardData.getData('text/plain');
    if (text) { lastClip = text; send('c,' + text); }
    flushPaste();
  });
  kbd.addEventListener('input', () => { kbd.value = ''; });
  window.addEventListener('blur', releaseAll);
  surface.addEventListener('focus', () => kbd.focus({ preventScroll: true }));
  if (!inIframe) kbd.focus({ preventScroll: true });

  // ---- clipboard (server -> client) ----
  let lastClip = null, clipOut = null;
  function onRemoteClipboard(text) {
    if (text === lastClip) return;
    lastClip = text;
    clipOut = text;
    flushClipboardOut();
  }
  function flushClipboardOut() {
    if (clipOut === null || !navigator.clipboard || (inIframe && !document.hasFocus())) return;
    const t = clipOut;
    navigator.clipboard.writeText(t).then(() => { if (clipOut === t) clipOut = null; }, () => {});
  }
  window.addEventListener('focus', flushClipboardOut);

  // ---- audio ----
  let audioStarted = false, ac = null;
  async function startAudio() {
    if (ac && ac.state === 'suspended') { ac.resume().catch(() => {}); return; }
    if (audioStarted || typeof AudioDecoder === 'undefined') return;
    audioStarted = true;
    try {
      ac = new AudioContext({ latencyHint: 'interactive', sampleRate: 48000 });
      await ac.audioWorklet.addModule('audio.js');
      const node = new AudioWorkletNode(ac, 'pcm-player', { outputChannelCount: [2] });
      node.connect(ac.destination);
      const ch = new MessageChannel();
      node.port.postMessage({ port: ch.port2 }, [ch.port2]);
      worker.postMessage({ t: 'audioPort', port: ch.port1 }, [ch.port1]);
      if (ac.state !== 'running') ac.resume().catch(() => {});
    } catch (err) {
      console.warn('[selkies-rs] audio unavailable', err);
    }
  }
  // Autoplay may be allowed (iframe allow="autoplay"); try right away.
  startAudio();
})();
