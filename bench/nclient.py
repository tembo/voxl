"""Native benchmark client: decodes the stream with FFmpeg (PyAV) and reads the loadgen
pixel marker, reporting unique displayed frames/s and capture->decode latency.
Usage: nclient.py rs|selkies <host:port> <secs> [markerX markerY]"""
import asyncio, json, os, sys, time
import av, websockets

mode, hostport, secs = sys.argv[1], sys.argv[2], float(sys.argv[3])
mx, my = (int(sys.argv[4]), int(sys.argv[5])) if len(sys.argv) > 5 else (300, 150)

def make_ctx():
    c = av.CodecContext.create('h264', 'r')
    c.thread_type = 'SLICE'
    c.thread_count = 8
    if os.environ.get('LOWDELAY'): c.flags |= av.codec.context.Flags.low_delay
    c.flags2 |= av.codec.context.Flags2.fast if hasattr(av.codec.context, 'Flags2') else 0
    return c

def read_marker(frame):
    p = frame.planes[0]
    ls = p.line_size
    buf = memoryview(p)
    v = 0
    for b in range(40):
        x = mx + (b % 20) * 24 + 12
        y = my + (b // 20) * 24 + 12
        if buf[y * ls + x] > 128:
            v |= 1 << b
    return v >> 24, v & 0xffffff

async def main():
    url = f'ws://{hostport}/ws' if mode == 'rs' else f'ws://{hostport}/api/websockets'
    ctx = make_ctx()
    seen_per_sec, lat, cur, last = [], [], set(), None
    dec_ms, nbytes, nframes = [], 0, 0
    rlat = []
    netlat = []
    t_start = None
    async with websockets.connect(url, max_size=None, compression=None) as ws:
        if mode == 'rs':
            await ws.send('v,1')
        else:
            await ws.send('SETTINGS,' + json.dumps({"is_manual_resolution_mode": False, "initialClientWidth": 2560, "initialClientHeight": 1600, "scaling_dpi": 96, "keyboardLayout": "us", "useCssScaling": True, "displayId": "primary"}))
            await ws.send('cr')
        t0 = time.time(); sec0 = None
        while time.time() - t0 < secs + 4:
            msg = await ws.recv()
            if isinstance(msg, str):
                if mode == 'rs' and msg.startswith('{"t":"ping"'):
                    await ws.send('po,' + str(json.loads(msg)['s']))
                continue
            if mode == 'rs':
                if msg[0] != 1: continue
                fid = int.from_bytes(msg[2:6], 'big'); payload = msg[10:]
            else:
                if msg[0] != 4: continue
                fid = int.from_bytes(msg[2:4], 'big'); payload = msg[10:]
            nbytes += len(msg)
            trecv = time.time()
            if mode == 'rs' and os.environ.get('NETLAT'): netlat.append((int(trecv*1000) & 0xffffffff) - int.from_bytes(msg[6:10], 'big'))
            td = time.perf_counter()
            if os.environ.get('DUMP'): open(os.environ['DUMP'], 'ab').write(payload)
            frames = ctx.decode(av.Packet(payload))
            dec_ms.append((time.perf_counter() - td) * 1000)
            await ws.send(f'a,{fid}' if mode == 'rs' else f'CLIENT_FRAME_ACK {fid}')
            for fr in frames:
                nframes += 1
                n, ms = read_marker(fr)
                now = time.time()
                if now - t0 < 4:  # warmup
                    continue
                if sec0 is None: sec0 = now; t_start = now; nbytes = 0
                if n != last:
                    last = n; cur.add(n)
                    l = (int(now * 1000) & 0xffffff) - ms
                    if l < -1e6: l += 0x1000000
                    if 0 <= l < 10000: lat.append(l); rlat.append(l - (now - trecv) * 1000)
                if now - sec0 >= 1:
                    seen_per_sec.append(len(cur) / (now - sec0)); cur = set(); sec0 = now
    s = sorted(seen_per_sec); l = sorted(lat); d = sorted(dec_ms)
    q = lambda a, p: round(a[min(len(a) - 1, int(len(a) * p))], 1) if a else None
    el = time.time() - t_start if t_start else 1
    print(json.dumps({"mode": mode, "uniqueFps": {"avg": round(sum(s) / max(len(s), 1), 1), "min": q(s, 0), "p10": q(s, .1), "med": q(s, .5)},
        "latencyMs": {"p50": q(l, .5), "p90": q(l, .9), "p99": q(l, .99), "max": q(l, 1)}, "decodeMs": {"p50": q(d, .5), "p99": q(d, .99)},
        "recvLatMs": {"p50": q(sorted(rlat), .5), "p90": q(sorted(rlat), .9)}, "netLat": {"p50": q(sorted(netlat), .5), "p90": q(sorted(netlat), .9)}, "mbps": round(nbytes * 8 / el / 1e6, 2)}))

asyncio.run(main())
