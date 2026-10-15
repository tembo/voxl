"""Native benchmark client: decodes the stream with FFmpeg (PyAV) and reads the loadgen
pixel marker, reporting unique displayed frames/s and capture->decode latency.
Usage: nclient.py voxl|selkies <host:port> <secs> [markerX markerY]"""
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

def read_marker(frame, yoff=0):
    p = frame.planes[0]
    ls = p.line_size
    buf = memoryview(p)
    v = 0
    for b in range(40):
        x = mx + (b % 20) * 24 + 12
        y = my + (b // 20) * 24 + 12 - yoff
        if buf[y * ls + x] > 128:
            v |= 1 << b
    return v >> 24, v & 0xffffff

async def main():
    url = f'ws://{hostport}/ws' if mode == 'voxl' else f'ws://{hostport}/api/websockets'
    ctx = make_ctx()
    ctxs, got = {}, {}
    sy = 0
    seen_per_sec, lat, cur, last = [], [], set(), None
    dec_ms, nbytes, nframes = [], 0, 0
    rlat = []
    netlat = []
    dps, dcount, dsec0, last_hash = [], 0, None, None
    t_start = None
    async with websockets.connect(url, max_size=None, compression=None) as ws:
        if mode == 'voxl':
            await ws.send('v,1')
        else:
            await ws.send('SETTINGS,' + json.dumps({"is_manual_resolution_mode": False, "initialClientWidth": 2560, "initialClientHeight": 1600, "scaling_dpi": 96, "keyboardLayout": "us", "useCssScaling": True, "displayId": "primary"}))
            await ws.send('cr')
        t0 = time.time(); sec0 = None
        while time.time() - t0 < secs + 4:
            msg = await ws.recv()
            if isinstance(msg, str):
                if mode == 'voxl' and msg.startswith('{"t":"ping"'):
                    await ws.send('po,' + str(json.loads(msg)['s']))
                continue
            if mode == 'voxl':
                if msg[0] != 1: continue
                fid = int.from_bytes(msg[2:6], 'big'); payload = msg[16:]
                si = msg[10]; parts = msg[12]; sy = int.from_bytes(msg[14:16], 'big')
                if si not in ctxs: ctxs[si] = make_ctx()
                ctx = ctxs[si]
            else:
                if msg[0] != 4: continue
                fid = int.from_bytes(msg[2:4], 'big'); payload = msg[10:]
            nbytes += len(msg)
            trecv = time.time()
            if mode == 'voxl' and os.environ.get('NETLAT'): netlat.append((int(trecv*1000) & 0xffffffff) - int.from_bytes(msg[6:10], 'big'))
            td = time.perf_counter()
            if os.environ.get('DUMP'): open(os.environ['DUMP'], 'ab').write(payload)
            frames = ctx.decode(av.Packet(payload))
            dec_ms.append((time.perf_counter() - td) * 1000)
            if mode == 'voxl':
                got[fid] = got.get(fid, 0) + 1
                if got[fid] >= parts:
                    del got[fid]
                    await ws.send(f'a,{fid}')
            else:
                await ws.send(f'CLIENT_FRAME_ACK {fid}')
            for fr in frames:
                if mode == 'voxl' and not (sy <= my and my + 48 <= sy + fr.height):
                    continue
                nframes += 1
                if os.environ.get('DISTINCT'):
                    pl = fr.planes[0]; mv = memoryview(pl); ls = pl.line_size
                    h = hash(b''.join(bytes(mv[y * ls: y * ls + fr.width:7]) for y in range(0, fr.height, 9)))
                    now = time.time()
                    if now - t0 >= 4:
                        if h != last_hash: dcount += 1
                        if dsec0 is None: dsec0 = now
                        if now - dsec0 >= 1: dps.append(dcount / (now - dsec0)); dcount = 0; dsec0 = now
                    last_hash = h
                    continue
                n, ms = read_marker(fr, sy if mode == 'voxl' else 0)
                now = time.time()
                if now - t0 < 4:  # warmup
                    continue
                if sec0 is None: sec0 = now; t_start = now; nbytes = 0
                if n != last:
                    last = n; cur.add(n)
                    l = (int(now * 1000) & 0xffffff) - ms
                    if l < -1e6: l += 0x1000000
                    if os.environ.get('DBG') and len(lat) < 3: print('dbg', n, ms, l, file=sys.stderr)
                    if 0 <= l < 10000: lat.append(l); rlat.append(l - (now - trecv) * 1000)
                if now - sec0 >= 1:
                    seen_per_sec.append(len(cur) / (now - sec0)); cur = set(); sec0 = now
    s = sorted(seen_per_sec); l = sorted(lat); d = sorted(dec_ms)
    q = lambda a, p: round(a[min(len(a) - 1, int(len(a) * p))], 1) if a else None
    el = time.time() - t_start if t_start else 1
    print(json.dumps({"mode": mode, "uniqueFps": {"avg": round(sum(s) / max(len(s), 1), 1), "min": q(s, 0), "p10": q(s, .1), "med": q(s, .5)},
        "latencyMs": {"p50": q(l, .5), "p90": q(l, .9), "p99": q(l, .99), "max": q(l, 1)}, "decodeMs": {"p50": q(d, .5), "p99": q(d, .99)},
        "recvLatMs": {"p50": q(sorted(rlat), .5), "p90": q(sorted(rlat), .9)}, "netLat": {"p50": q(sorted(netlat), .5), "p90": q(sorted(netlat), .9)}, "distinctFps": {"avg": round(sum(dps)/max(len(dps),1),1), "min": q(sorted(dps),0), "p10": q(sorted(dps),.1), "med": q(sorted(dps),.5)}, "mbps": round(nbytes * 8 / el / 1e6, 2)}))

asyncio.run(main())
