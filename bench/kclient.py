"""Native KasmVNC benchmark client: speaks RFB over KasmVNC's WebSocket like its web
client does (same pixel format and encodings, continuous updates, fences), keeps a
full framebuffer and reads the loadgen pixel marker after every update. Reports the
same JSON as nclient.py: unique displayed frames/s, draw->display latency and Mbps.
Usage: kclient.py <host:port> <secs> [markerX markerY]"""
import asyncio, io, json, os, struct, sys, time, zlib
from concurrent.futures import ThreadPoolExecutor
import numpy as np
import websockets
from PIL import Image

hostport, secs = sys.argv[1], float(sys.argv[2])
mx, my = (int(sys.argv[3]), int(sys.argv[4])) if len(sys.argv) > 4 else (300, 150)

# What KasmVNC's web client (Chromium) sends, minus cursor, clipboard and desktop-name
# extensions that don't affect the picture: CopyRect, Tight, TightPNG, Hextile, RRE,
# Raw, quality 6, compression 2, DesktopSize, LastRect, Fence, ContinuousUpdates,
# WebP, then Kasm's quality/video settings (ignored by servers with
# IgnoreClientSettingsKasm, as in Tembo) and PreferBandwidth.
ENCODINGS = [1, 7, -260, 5, 2, 0, -26, -254, -223, -224, -312, -313, -1024,
             -1016, -1006, -996, -987, -972, -907, -865, -1984, -1996, -2034, -1997, -992]

pool = ThreadPoolExecutor(8)


def decode_image(data, w, h):
    im = Image.open(io.BytesIO(data)).convert('RGB')
    # Video mode can send a scaled-down image; the browser stretches it to the rect.
    if im.size != (w, h):
        im = im.resize((w, h), Image.BILINEAR)
    return np.asarray(im)


class Conn:
    def __init__(self, ws):
        self.ws, self.buf, self.pos, self.nbytes = ws, bytearray(), 0, 0

    async def need(self, n):
        while len(self.buf) - self.pos < n:
            msg = await self.ws.recv()
            self.nbytes += len(msg)
            if self.pos > 1 << 20:
                del self.buf[:self.pos]; self.pos = 0
            self.buf += msg

    async def read(self, n):
        await self.need(n)
        b = bytes(self.buf[self.pos:self.pos + n]); self.pos += n
        return b

    async def u8(self): return (await self.read(1))[0]
    async def u16(self): return struct.unpack('>H', await self.read(2))[0]
    async def u32(self): return struct.unpack('>I', await self.read(4))[0]
    async def s32(self): return struct.unpack('>i', await self.read(4))[0]

    async def compact(self):
        b = await self.u8(); n = b & 0x7f
        if b & 0x80:
            b = await self.u8(); n |= (b & 0x7f) << 7
            if b & 0x80:
                n |= (await self.u8()) << 14
        return n


async def main():
    url = f'ws://{hostport}/websockify'
    async with websockets.connect(url, subprotocols=['binary'], origin=f'http://{hostport}', max_size=None, compression=None) as ws:
        c = Conn(ws)
        ver = await c.read(12)
        await ws.send(b'RFB 003.008\n')
        types = await c.read(await c.u8())
        if 1 not in types:
            sys.exit(f'server wants authentication (security types {list(types)}); start it with -SecurityTypes None')
        await ws.send(bytes([1]))
        if await c.u32() != 0:
            sys.exit('security handshake failed')
        await ws.send(bytes([1]))  # shared
        W, H = await c.u16(), await c.u16()
        await c.read(16)
        await c.read(await c.u32())
        # 32 bpp, depth 24, little endian, true colour, R/G/B at bytes 0/1/2, as the web client.
        await ws.send(bytes([0, 0, 0, 0, 32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0]))
        await ws.send(struct.pack('>BBH', 2, 0, len(ENCODINGS)) + b''.join(struct.pack('>i', e) for e in ENCODINGS))
        await ws.send(struct.pack('>BBHHHH', 3, 0, 0, 0, W, H))

        fb = np.zeros((H, W, 3), np.uint8)
        zs = [zlib.decompressobj() for _ in range(4)]
        cu = False
        t0 = time.time(); sec0 = None; t_start = None
        seen_per_sec, cur, last, lat, apply_ms = [], set(), None, [], []
        while time.time() - t0 < secs + 4:
            mt = await c.u8()
            if mt == 0:
                await c.read(1)
                nrects = await c.u16()
                ops = []
                i = 0
                while i < nrects:
                    i += 1
                    x, y, w, h = struct.unpack('>HHHH', await c.read(8))
                    enc = await c.s32()
                    if enc == -224:  # LastRect
                        break
                    if enc == -223:  # DesktopSize
                        W, H = w, h; fb = np.zeros((H, W, 3), np.uint8); continue
                    if enc == 0:
                        px = np.frombuffer(await c.read(w * h * 4), np.uint8).reshape(h, w, 4)[:, :, :3]
                        ops.append(('px', x, y, w, h, px))
                    elif enc == 1:
                        sx, sy = await c.u16(), await c.u16()
                        ops.append(('copy', x, y, w, h, (sx, sy)))
                    elif enc == 7:
                        ctl = await c.u8()
                        for s in range(4):
                            if ctl >> s & 1:
                                zs[s] = zlib.decompressobj()
                        ctl >>= 4
                        if ctl == 8:
                            ops.append(('fill', x, y, w, h, np.frombuffer(await c.read(3), np.uint8)))
                        elif ctl in (9, 10, 11):  # JPEG, PNG, WebP
                            data = await c.read(await c.compact())
                            ops.append(('px', x, y, w, h, pool.submit(decode_image, data, w, h)))
                        elif ctl in (12, 13):  # QOI (not requested), translucent overlay
                            await c.read(await c.compact())
                        elif ctl & 8 == 0:
                            stream = ctl & 3
                            filt = await c.u8() if ctl & 4 else 0
                            if filt == 1:
                                ncol = await c.u8() + 1
                                pal = np.frombuffer(await c.read(ncol * 3), np.uint8).reshape(ncol, 3)
                                rowb = (w + 7) // 8 if ncol <= 2 else w
                                size = rowb * h
                            elif filt == 0:
                                size = w * h * 3
                            else:
                                sys.exit(f'unsupported tight filter {filt}')
                            raw = await c.read(size) if size < 12 else zs[stream].decompress(await c.read(await c.compact()))
                            a = np.frombuffer(raw, np.uint8)
                            if filt == 0:
                                px = a.reshape(h, w, 3)
                            elif ncol <= 2:
                                bits = np.unpackbits(a.reshape(h, rowb), axis=1)[:, :w]
                                px = pal[bits]
                            else:
                                px = pal[a.reshape(h, w)]
                            ops.append(('px', x, y, w, h, px))
                        else:
                            sys.exit(f'unsupported tight subencoding {ctl}')
                    else:
                        sys.exit(f'unsupported encoding {enc}')
                ta = time.perf_counter()
                for kind, x, y, w, h, v in ops:
                    if kind == 'px':
                        fb[y:y + h, x:x + w] = v.result() if hasattr(v, 'result') else v
                    elif kind == 'fill':
                        fb[y:y + h, x:x + w] = v
                    else:
                        sx, sy = v
                        fb[y:y + h, x:x + w] = fb[sy:sy + h, sx:sx + w].copy()
                apply_ms.append((time.perf_counter() - ta) * 1000)
                if not cu:
                    await ws.send(struct.pack('>BBHHHH', 3, 1, 0, 0, W, H))
                # Read the marker: 40 bits as 24x24 cells, white = 1.
                v = 0
                for b in range(40):
                    px, py = mx + (b % 20) * 24 + 12, my + (b // 20) * 24 + 12
                    if py < H and px < W and int(fb[py, px].sum()) > 384:
                        v |= 1 << b
                n, ms = v >> 24, v & 0xffffff
                now = time.time()
                if now - t0 < 4:
                    continue
                if sec0 is None:
                    sec0 = t_start = now; c.nbytes = 0
                if n != last:
                    last = n; cur.add(n)
                    l = (int(now * 1000) & 0xffffff) - ms
                    if l < -1e6: l += 0x1000000
                    if 0 <= l < 10000: lat.append(l)
                if now - sec0 >= 1:
                    seen_per_sec.append(len(cur) / (now - sec0)); cur = set(); sec0 = now
            elif mt == 150:  # EndOfContinuousUpdates: the server supports them, so use them
                if not cu:
                    cu = True
                    await ws.send(struct.pack('>BBHHHH', 150, 1, 0, 0, W, H))
            elif mt == 248:  # Fence: answer requests right away, like the web client
                await c.read(3)
                flags = await c.u32()
                payload = await c.read(await c.u8())
                if flags & 0x80000000:
                    await ws.send(struct.pack('>BxxxIB', 248, flags & 7, len(payload)) + payload)
            elif mt == 3:
                await c.read(3)
                await c.read(await c.u32())
            elif mt == 2 or mt == 179:
                pass
            else:
                sys.exit(f'unexpected server message {mt}')
    if os.environ.get('SNAPSHOT'):
        Image.fromarray(fb).save(os.environ['SNAPSHOT'])
    s = sorted(seen_per_sec); l = sorted(lat); d = sorted(apply_ms)
    q = lambda a, p: round(a[min(len(a) - 1, int(len(a) * p))], 1) if a else None
    el = time.time() - t_start if t_start else 1
    print(json.dumps({"mode": "kasmvnc", "uniqueFps": {"avg": round(sum(s) / max(len(s), 1), 1), "min": q(s, 0), "p10": q(s, .1), "med": q(s, .5)},
        "latencyMs": {"p50": q(l, .5), "p90": q(l, .9), "p99": q(l, .99), "max": q(l, 1)}, "decodeMs": {"p50": q(d, .5), "p99": q(d, .99)},
        "mbps": round(c.nbytes * 8 / el / 1e6, 2)}))
    sys.stdout.flush()
    os._exit(0)

asyncio.run(main())
