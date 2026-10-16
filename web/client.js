// Main thread: layout, input capture, cursor, clipboard and audio wiring.
// Video decode/paint and the socket live in worker.js.
'use strict';
(() => {
  const surface = document.getElementById('surface');
  const canvas = document.getElementById('screen');
  const kbd = document.getElementById('kbd');
  const statusEl = document.getElementById('status');
  const statsEl = document.getElementById('stats');
  const params = new URLSearchParams(location.search);
  const showStats = params.has('stats');
  const isMac = /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);
  const inIframe = window !== window.top;

  let dw = 0, dh = 0, cursorMsg = null, connected = false;
  window.fps = 0;
  window.voxlStats = {};

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
  // One <video> per encoded stripe, stacked; the worker hands over every stripe of a
  // frame in one message so they are written to their tracks together.
  let stripeOut = [], stripeLayout = [[0, 1]];
  if (useVideo) canvas.style.display = 'none';
  function buildStripes(list) {
    for (const s of stripeOut) s.video.remove();
    stripeOut = [];
    if (!useVideo) return;
    for (const [y, h] of list) {
      const gen = new MediaStreamTrackGenerator({ kind: 'video' });
      const video = document.createElement('video');
      video.className = 'stripe';
      video.muted = true; video.autoplay = true; video.playsInline = true; video.disablePictureInPicture = true;
      video.style.top = `${(y / dh) * 100}%`;
      video.style.height = `${(h / dh) * 100}%`;
      video.srcObject = new MediaStream([gen]);
      surface.appendChild(video);
      video.play().catch(() => {});
      stripeOut.push({ video, writer: gen.writable.getWriter(), writing: false, next: null });
    }
  }
  function writeStripe(o, frame) {
    if (o.writing) { if (o.next) o.next.close(); o.next = frame; return; }
    o.writing = true;
    o.writer.write(frame).catch(() => {}).finally(() => {
      o.writing = false;
      if (o.next) { const f = o.next; o.next = null; writeStripe(o, f); }
    });
  }
  function present(parts) {
    for (const p of parts) {
      const o = stripeOut[p.i];
      if (o) writeStripe(o, p.frame); else p.frame.close();
    }
  }
  const offscreen = canvas.transferControlToOffscreen();
  worker.postMessage({ t: 'init', canvas: offscreen, url: wsUrl.href, presentToMain: useVideo }, [offscreen]);
  const send = (d) => worker.postMessage({ t: 'send', d });

  worker.onmessage = (e) => {
    const m = e.data;
    switch (m.t) {
      case 'frames': present(m.parts); break;
      case 'hello':
        dw = m.w; dh = m.h; connected = true;
        stripeLayout = m.stripes && m.stripes.length ? m.stripes : [[0, m.h]];
        buildStripes(stripeLayout);
        layout();
        statusEl.style.display = 'none';
        break;
      case 'status':
        if (m.s === 'closed') { connected = false; statusEl.style.display = ''; statusEl.textContent = 'Reconnecting…'; }
        if (m.s === 'open') updateVisibility(true);
        break;
      case 'cursor': cursorMsg = m.buf; applyCursor(); break;
      case 'clip': onRemoteClipboard(m.d); break;
      case 'clipImage': onRemoteImage(m.buf); break;
      case 'stats':
        window.fps = Math.round(m.fps);
        window.voxlStats = m;
        if (showStats) {
          statsEl.style.display = 'block';
          statsEl.textContent = `${m.fps.toFixed(1)} fps  ${(m.kbps / 1000).toFixed(2)} Mbps\n` +
            `decode ${m.decodeMs.toFixed(1)} ms  server ${m.pipeMs.toFixed(1)} ms\n${m.codec || ''} q=${m.queue}`;
        }
        break;
      case 'log': console.warn('[voxl]', m.s); break;
    }
  };

  // ---- layout ----
  let scale = 1;
  function layout() {
    if (!dw) return;
    const vw = window.innerWidth, vh = window.innerHeight;
    const dpr = window.devicePixelRatio || 1;
    // Snap the height so every stripe boundary lands on a whole device pixel;
    // otherwise the stacked stripe layers show hairline seams.
    const gcd = (a, b) => (b ? gcd(b, a % b) : a);
    const q = dh / stripeLayout.reduce((g, [y]) => gcd(g, y), dh);
    const devH = Math.max(q, Math.floor((Math.min(vw / dw, vh / dh) || 1) * dh * dpr / q) * q);
    scale = devH / dpr / dh;
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
  surface.addEventListener('pointermove', (e) => { if (e.pointerType === 'touch') touch.move(e); else move(e); });
  surface.addEventListener('pointerdown', (e) => {
    e.preventDefault();
    startAudio();
    flushClipboardOut();
    if (e.pointerType === 'touch') { touch.down(e); return; }
    kbd.focus({ preventScroll: true });
    move(e);
    try { surface.setPointerCapture(e.pointerId); } catch {}
    send(`b,${BTN[e.button] || 1},1`);
  });
  surface.addEventListener('pointerup', (e) => {
    e.preventDefault();
    if (e.pointerType === 'touch') { touch.up(e); return; }
    move(e); send(`b,${BTN[e.button] || 1},0`);
  });
  surface.addEventListener('pointercancel', (e) => { if (e.pointerType === 'touch') touch.cancel(e); else send('r'); });
  surface.addEventListener('contextmenu', (e) => e.preventDefault());

  // Scroll is sent in desktop pixels, batched per frame; the server turns it into
  // smooth (pixel-precise) scrolling, or wheel clicks on X servers that can't.
  let scrollX = 0, scrollY = 0, scrollQueued = false;
  function queueScroll(dx, dy) {
    scrollX += dx; scrollY += dy;
    if (scrollQueued) return;
    scrollQueued = true;
    requestAnimationFrame(() => {
      scrollQueued = false;
      if (Math.abs(scrollX) < 0.5 && Math.abs(scrollY) < 0.5) return;
      send(`sp,${scrollX.toFixed(1)},${scrollY.toFixed(1)}`);
      scrollX = 0; scrollY = 0;
    });
  }
  surface.addEventListener('wheel', (e) => {
    e.preventDefault();
    move(e);
    const k = (e.deltaMode === 1 ? 40 : e.deltaMode === 2 ? 800 : 1) * dw / surface.getBoundingClientRect().width;
    queueScroll(e.deltaX * k, e.deltaY * k);
  }, { passive: false });

  // ---- touch: tap = click, drag = left-drag, long press = right click,
  // two fingers = scroll (content follows the fingers) ----
  const touch = (() => {
    const pts = new Map();
    let mode = null, start = null, timer = null, lastMid = null;
    const SLOP = 10, LONG_PRESS = 550;
    const mid = () => { let x = 0, y = 0; for (const p of pts.values()) { x += p.x; y += p.y; } return [x / pts.size, y / pts.size]; };
    const moveTo = (x, y) => { const [dx, dy] = toDesk({ clientX: x, clientY: y }); lastMove = ''; send(`m,${dx},${dy}`); };
    function down(e) {
      try { surface.setPointerCapture(e.pointerId); } catch {}
      pts.set(e.pointerId, { x: e.clientX, y: e.clientY });
      if (pts.size === 1) {
        mode = 'pending'; start = { x: e.clientX, y: e.clientY };
        timer = setTimeout(() => {
          if (mode !== 'pending') return;
          moveTo(start.x, start.y); send('b,3,1'); send('b,3,0'); mode = 'done';
          if (navigator.vibrate) navigator.vibrate(10);
        }, LONG_PRESS);
      } else if (pts.size === 2) {
        clearTimeout(timer);
        if (mode === 'drag') send('b,1,0');
        mode = 'scroll'; lastMid = mid();
      }
    }
    function move(e) {
      const p = pts.get(e.pointerId);
      if (!p) return;
      p.x = e.clientX; p.y = e.clientY;
      if (mode === 'pending' && Math.hypot(p.x - start.x, p.y - start.y) > SLOP) {
        clearTimeout(timer); mode = 'drag'; moveTo(start.x, start.y); send('b,1,1');
      }
      if (mode === 'drag') moveTo(p.x, p.y);
      else if (mode === 'scroll' && pts.size >= 2) {
        const m = mid(), k = dw / surface.getBoundingClientRect().width;
        queueScroll((lastMid[0] - m[0]) * k, (lastMid[1] - m[1]) * k);
        lastMid = m;
      }
    }
    function up(e) {
      if (!pts.delete(e.pointerId)) return;
      if (mode === 'pending') { clearTimeout(timer); moveTo(start.x, start.y); send('b,1,1'); send('b,1,0'); mode = 'done'; }
      else if (mode === 'drag') { send('b,1,0'); mode = 'done'; }
      else if (mode === 'scroll' && pts.size < 2) mode = 'done';
      if (pts.size === 0) mode = null;
    }
    function cancel(e) {
      clearTimeout(timer);
      if (mode === 'drag') send('b,1,0');
      pts.delete(e.pointerId); if (pts.size === 0) mode = null;
    }
    return { down, move, up, cancel };
  })();

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
    const dt = e.clipboardData;
    const text = dt && dt.getData('text/plain');
    if (text) { lastClip = text; send('c,' + text); flushPaste(); return; }
    const item = dt && [...dt.items].find((i) => i.kind === 'file' && i.type.startsWith('image/'));
    if (!item) { flushPaste(); return; }
    // Hold the paste shortcut until the image is on the remote clipboard.
    if (pendingPaste) { clearTimeout(pendingPaste.timer); pendingPaste.timer = setTimeout(flushPaste, 3000); }
    toPng(item.getAsFile()).then((png) => {
      const msg = new Uint8Array(png.byteLength + 1);
      msg[0] = 0x11; msg.set(new Uint8Array(png), 1);
      worker.postMessage({ t: 'sendBin', d: msg.buffer }, [msg.buffer]);
    }).catch((err) => console.warn('[voxl] image paste failed', err)).finally(flushPaste);
  });
  async function toPng(file) {
    if (file.type === 'image/png') return file.arrayBuffer();
    const bmp = await createImageBitmap(file);
    const c = new OffscreenCanvas(bmp.width, bmp.height);
    c.getContext('2d').drawImage(bmp, 0, 0);
    return (await c.convertToBlob({ type: 'image/png' })).arrayBuffer();
  }
  // Text that arrives without usable keydowns (IME, dead-key composition, mobile
  // keyboards, insertText) is typed as individual keysyms.
  function typeText(text) {
    for (const ch of text) {
      const cp = ch.codePointAt(0);
      const ks = ch === '\n' ? 0xff0d : (cp >= 0x20 && cp <= 0x7e) || (cp >= 0xa0 && cp <= 0xff) ? cp : 0x01000000 + cp;
      send(`k,${ks},1`); send(`k,${ks},0`);
    }
  }
  kbd.addEventListener('compositionend', (e) => { if (e.data) typeText(e.data); kbd.value = ''; });
  kbd.addEventListener('input', (e) => {
    if (!e.isComposing) {
      // On-screen keyboards often deliver text and edits only as input events.
      if (e.inputType === 'insertText' && e.data) typeText(e.data);
      else if (e.inputType === 'deleteContentBackward') { send('k,65288,1'); send('k,65288,0'); }
      else if (e.inputType === 'deleteContentForward') { send('k,65535,1'); send('k,65535,0'); }
      else if (e.inputType === 'insertLineBreak' || e.inputType === 'insertParagraph') { send('k,65293,1'); send('k,65293,0'); }
      kbd.value = '';
    }
  });
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
  function onRemoteImage(buf) {
    clipOut = new Blob([new Uint8Array(buf, 1)], { type: 'image/png' });
    flushClipboardOut();
  }
  function flushClipboardOut() {
    if (clipOut === null || !navigator.clipboard || (inIframe && !document.hasFocus())) return;
    const c = clipOut;
    const done = () => { if (clipOut === c) clipOut = null; };
    if (typeof c === 'string') navigator.clipboard.writeText(c).then(done, () => {});
    else if (typeof ClipboardItem !== 'undefined') navigator.clipboard.write([new ClipboardItem({ 'image/png': c })]).then(done, () => {});
  }
  window.addEventListener('focus', flushClipboardOut);

  // ---- toolbar: on-screen keyboard (touch) and fullscreen ----
  const tools = document.getElementById('tools');
  const kbBtn = document.getElementById('kb-btn');
  const fsBtn = document.getElementById('fs-btn');
  let toolsTimer = null;
  function showTools(ms) {
    tools.classList.add('show');
    clearTimeout(toolsTimer);
    if (ms) toolsTimer = setTimeout(() => tools.classList.remove('show'), ms);
  }
  // Touch devices keep the toolbar visible; mice reveal it near the top edge.
  if (matchMedia('(pointer: coarse)').matches) { tools.classList.add('touch'); showTools(0); }
  else document.addEventListener('pointermove', (e) => { if (e.pointerType === 'mouse' && e.clientY < 48 && e.clientX > window.innerWidth - 160) showTools(2500); });
  for (const b of [kbBtn, fsBtn]) b.addEventListener('pointerdown', (e) => e.stopPropagation());
  kbBtn.addEventListener('click', () => {
    if (document.activeElement === kbd && kbd.dataset.soft) { kbd.blur(); delete kbd.dataset.soft; }
    else { kbd.dataset.soft = '1'; kbd.focus({ preventScroll: true }); }
  });
  if (!document.fullscreenEnabled) fsBtn.style.display = 'none';
  fsBtn.addEventListener('click', () => {
    if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
    else document.documentElement.requestFullscreen({ navigationUI: 'hide' }).catch(() => {});
  });
  // In fullscreen, capture browser shortcuts (Cmd/Ctrl+W, T, L, ...) for the desktop.
  // Keyboard Lock is Chromium-only; hold Esc to leave fullscreen.
  document.addEventListener('fullscreenchange', () => {
    const kl = navigator.keyboard;
    if (document.fullscreenElement) { if (kl && kl.lock) kl.lock().catch(() => {}); }
    else if (kl && kl.unlock) kl.unlock();
    fsBtn.classList.toggle('on', !!document.fullscreenElement);
    layout();
  });

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
      console.warn('[voxl] audio unavailable', err);
    }
  }
  // Autoplay may be allowed (iframe allow="autoplay"); try right away.
  startAudio();
})();
