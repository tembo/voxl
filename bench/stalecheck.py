"""Watches voxl as a viewer does (answering pings, acknowledging frames) and decodes the
stream, then compares its last decoded picture with the real screen: what a viewer is
left looking at once the screen is still. Prints JSON with the PSNR over the region and
the number of 16x16 blocks whose average difference is over 20 levels (content that is
wrong, not just soft). VOXL_PASSWORD is the password voxl was started with.
MARKER=x,y also reads a loadgen animation's marker at (x, y) in every decoded frame and
reports, for each second after it first shows, the animation's unique frames and average
latency (from drawing to decoding), with p99 and maximum latency over the run.
Usage: stalecheck.py <stream host:port> <screenshot host:port> <secs> [x,y,w,h] [out.png]"""
import asyncio, io, json, os, sys, time, urllib.parse, urllib.request
import av, numpy as np, websockets
from PIL import Image

stream, direct, secs = sys.argv[1], sys.argv[2], float(sys.argv[3])
region = [int(v) for v in sys.argv[4].split(',')] if len(sys.argv) > 4 else None
out_png = sys.argv[5] if len(sys.argv) > 5 else None
password = os.environ.get('VOXL_PASSWORD', '')
marker = [int(v) for v in os.environ['MARKER'].split(',')] if os.environ.get('MARKER') else None
anim = []  # (time decoded, frame number, latency ms) per decoded frame showing the marker


def read_marker(frame):
    """loadgen's marker: 40 cells of 24 px, bit set where white (frame << 24 | ms)."""
    p = frame.planes[0]
    buf, ls, v = memoryview(p), p.line_size, 0
    for b in range(40):
        x, y = marker[0] + (b % 20) * 24 + 12, marker[1] + (b // 20) * 24 + 12
        if buf[y * ls + x] > 128:
            v |= 1 << b
    return v >> 24, v & 0xffffff


def screenshot():
    req = urllib.request.Request(f'http://{direct}/screenshot')
    if password:
        req.add_header('Authorization', f'Bearer {password}')
    with urllib.request.urlopen(req, timeout=10) as r:
        return np.asarray(Image.open(io.BytesIO(r.read())).convert('RGB'))


async def watch():
    query = f'?token={urllib.parse.quote(password)}' if password else ''
    ctx, last, parts = av.CodecContext.create('h264', 'r'), None, {}
    async with websockets.connect(f'ws://{stream}/ws{query}', max_size=None) as ws:
        await ws.send('v,1')
        end = time.time() + secs
        while time.time() < end:
            try:
                m = await asyncio.wait_for(ws.recv(), 1)
            except asyncio.TimeoutError:
                continue
            if isinstance(m, str):
                d = json.loads(m)
                if d.get('t') == 'ping':
                    await ws.send(f"po,{d['s']}")
                continue
            if m[0] != 1:
                continue
            for f in ctx.decode(av.Packet(m[16:])):
                # Converted now: PyAV reuses a decoded frame's buffers for later frames.
                last = f.to_ndarray(format='rgb24')
                if marker:
                    n, ms = read_marker(f)
                    now = time.time()
                    lat = (int(now * 1000) & 0xffffff) - ms
                    if 0 <= lat < 10000:
                        anim.append((now, n, lat))
            fid = int.from_bytes(m[2:6], 'big')
            parts[fid] = parts.get(fid, 0) + 1
            if parts[fid] >= m[12]:
                del parts[fid]
                await ws.send(f'a,{fid}')
    return last


seen = asyncio.run(watch())
truth = screenshot()
if seen is None:
    sys.exit('no frame decoded')
if region:
    x, y, w, h = region
    seen, truth = seen[y:y + h, x:x + w], truth[y:y + h, x:x + w]
if out_png:
    Image.fromarray(seen).save(out_png)
    Image.fromarray(truth).save(out_png.replace('.png', '-truth.png'))
diff = np.abs(seen.astype(np.int16) - truth.astype(np.int16))
mse = float((diff.astype(np.float64) ** 2).mean())
h16, w16 = diff.shape[0] // 16 * 16, diff.shape[1] // 16 * 16
blocks = diff[:h16, :w16].reshape(h16 // 16, 16, w16 // 16, 16, 3).mean(axis=(1, 3, 4))
result = {
    'psnr_db': round(10 * np.log10(255 ** 2 / mse), 2) if mse else 99.0,
    'bad_blocks': int((blocks > 20).sum()),
    'blocks': int(blocks.size),
}
if anim:
    t0, per_s = anim[0][0], {}
    for t, n, lat in anim:
        per_s.setdefault(int(t - t0), []).append((n, lat))
    secs_seen = sorted(per_s)[:-1]  # the last second is partial
    lats = sorted(lat for _, _, lat in anim)
    result['anim'] = {
        'fps': [len({n for n, _ in per_s[s]}) for s in secs_seen],
        'lat_ms': [round(sum(l for _, l in per_s[s]) / len(per_s[s])) for s in secs_seen],
        'lat_p99_ms': lats[int(len(lats) * 0.99)],
        'lat_max_ms': lats[-1],
    }
print(json.dumps(result))
