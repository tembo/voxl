"""Native Neko benchmark client: logs in, signals over Neko's WebSocket API (v3) like its
web client, receives the WebRTC video with aiortc (decoded by FFmpeg/libvpx) and reads
the loadgen pixel marker from every decoded frame. Same JSON as nclient.py.
DEBUG=1 prints signaling and connection states.
Usage: rtcpy.sh nkclient.py <host:port> <secs> [markerX markerY]"""
import asyncio, json, os, sys, time, urllib.request
import websockets
from aiortc import RTCPeerConnection, RTCSessionDescription
from aiortc.sdp import candidate_from_sdp
import aiortc.rtcrtpreceiver as _rx
from aiortc.jitterbuffer import JitterBuffer as _JB

# aiortc's video jitter buffer holds 128 packets, fewer than one 2560x1600 keyframe,
# so such frames never complete. Browsers have no such limit.
_rx.JitterBuffer = lambda capacity, **kw: _JB(capacity=max(capacity, 4096), **kw)

hostport, secs = sys.argv[1], float(sys.argv[2])
mx, my = (int(sys.argv[3]), int(sys.argv[4])) if len(sys.argv) > 4 else (300, 150)


def login():
    req = urllib.request.Request(f'http://{hostport}/api/login', data=json.dumps({'username': 'bench', 'password': 'x'}).encode(),
                                 headers={'Content-Type': 'application/json'})
    with urllib.request.urlopen(req) as r:
        cookie = r.headers.get('Set-Cookie', '').split(';')[0]
        token = json.loads(r.read() or b'{}').get('token')
    return cookie, token


def read_marker(frame):
    p = frame.planes[0]
    ls, buf = p.line_size, memoryview(p)
    v = 0
    for b in range(40):
        x = mx + (b % 20) * 24 + 12
        y = my + (b // 20) * 24 + 12
        if buf[y * ls + x] > 128:
            v |= 1 << b
    return v >> 24, v & 0xffffff


async def main():
    cookie, token = login()
    url = f'ws://{hostport}/api/ws' + (f'?token={token}' if token else '')
    pc = RTCPeerConnection()
    track_ready = asyncio.get_event_loop().create_future()

    dbg = os.environ.get('DEBUG')

    @pc.on('iceconnectionstatechange')
    def on_ice():
        if dbg: print('ice', pc.iceConnectionState, file=sys.stderr)

    @pc.on('connectionstatechange')
    def on_conn():
        if dbg: print('conn', pc.connectionState, file=sys.stderr)

    @pc.on('track')
    def on_track(t):
        if dbg: print('track', t.kind, file=sys.stderr)
        if t.kind == 'video' and not track_ready.done():
            track_ready.set_result(t)

    async with websockets.connect(url, additional_headers={'Cookie': cookie} if cookie else {}, max_size=None) as ws:
        pending = []

        async def add_candidate(pl):
            c = candidate_from_sdp(pl['candidate'].split(':', 1)[1])
            c.sdpMid, c.sdpMLineIndex = pl.get('sdpMid'), pl.get('sdpMLineIndex')
            await pc.addIceCandidate(c)

        async def signaling():
            async for raw in ws:
                m = json.loads(raw)
                ev, pl = m.get('event'), m.get('payload') or {}
                if dbg and ev.startswith('signal'): print('<-', ev, json.dumps(pl)[:300].replace('\\r\\n', ' | '), file=sys.stderr)
                if ev == 'system/heartbeat':
                    await ws.send(json.dumps({'event': 'client/heartbeat'}))
                elif ev == 'signal/provide':
                    await pc.setRemoteDescription(RTCSessionDescription(pl['sdp'], 'offer'))
                    await pc.setLocalDescription(await pc.createAnswer())
                    await ws.send(json.dumps({'event': 'signal/answer', 'payload': {'sdp': pc.localDescription.sdp}}))
                    # Candidates can arrive before the offer.
                    for p in pending:
                        await add_candidate(p)
                    pending.clear()
                elif ev == 'signal/candidate' and pl.get('candidate'):
                    if pc.remoteDescription is None:
                        pending.append(pl)
                    else:
                        await add_candidate(pl)

        sig = asyncio.create_task(signaling())
        await ws.send(json.dumps({'event': 'signal/request', 'payload': {'video': {}, 'audio': {'disabled': True}}}))
        track = await asyncio.wait_for(track_ready, 20)

        t0 = time.time(); sec0 = None
        seen_per_sec, cur, last, lat = [], set(), None, []
        bytes0 = None
        while time.time() - t0 < secs + 4:
            try:
                frame = await asyncio.wait_for(track.recv(), 1)
            except asyncio.TimeoutError:
                # A stalled stream counts as seconds with no new frames.
                now = time.time()
                if sec0 is None and now - t0 >= 4:
                    sec0 = now
                    bytes0 = await rx_bytes(pc)
                if sec0 is not None and now - sec0 >= 1:
                    seen_per_sec.append(len(cur) / (now - sec0)); cur = set(); sec0 = now
                continue
            now = time.time()
            if now - t0 < 4:
                continue
            if sec0 is None:
                sec0 = now
                bytes0 = await rx_bytes(pc)
            n, ms = read_marker(frame)
            if n != last:
                last = n; cur.add(n)
                l = (int(now * 1000) & 0xffffff) - ms
                if l < -1e6: l += 0x1000000
                if 0 <= l < 120000: lat.append(l)
            if now - sec0 >= 1:
                seen_per_sec.append(len(cur) / (now - sec0)); cur = set(); sec0 = now
        mbps = (await rx_bytes(pc) - (bytes0 or 0)) * 8 / 1e6 / secs
        sig.cancel()
    await pc.close()
    s, l = sorted(seen_per_sec), sorted(lat)
    q = lambda a, p: round(a[min(len(a) - 1, int(len(a) * p))], 1) if a else None
    print(json.dumps({'mode': 'neko', 'uniqueFps': {'avg': round(sum(s) / max(len(s), 1), 1), 'min': q(s, 0), 'p10': q(s, 0.1), 'med': q(s, 0.5)},
                      'latencyMs': {'p50': q(l, 0.5), 'p90': q(l, 0.9), 'p99': q(l, 0.99), 'max': q(l, 1)}, 'mbps': round(mbps, 2)}))


async def rx_bytes(pc):
    st = await pc.getStats()
    return sum(getattr(r, 'bytesReceived', 0) or 0 for r in st.values() if r.type == 'transport')


asyncio.run(main())
