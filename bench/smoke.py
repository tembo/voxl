"""End-to-end smoke test of a running voxl: screenshot, hello, video, cursor, input and
clipboard. Typing must change the screen, so run it with a text editor focused (CI
opens Notepad). Exits non-zero on failure. SMOKE_SHOT=<path> saves the last screenshot.
Usage: smoke.py <host:port> [expected os]"""
import asyncio, json, os, sys, time, urllib.request
import websockets

hostport = sys.argv[1]
want_os = sys.argv[2] if len(sys.argv) > 2 else None
fails = []


def check(ok, what):
    print(('ok   ' if ok else 'FAIL ') + what, flush=True)
    if not ok:
        fails.append(what)


def screenshot():
    with urllib.request.urlopen(f'http://{hostport}/screenshot', timeout=10) as r:
        return r.read()


async def frames(ws, secs, stats):
    """Receives (and acknowledges) for `secs`; counts video frames, keyframes, cursors."""
    t0 = time.time()
    while time.time() - t0 < secs:
        try:
            msg = await asyncio.wait_for(ws.recv(), 0.5)
        except asyncio.TimeoutError:
            continue
        if isinstance(msg, str):
            m = json.loads(msg)
            if m.get('t') == 'ping':
                await ws.send(f"po,{m['s']}")
            elif m.get('t') == 'clip':
                stats['clip'] = m.get('d')
            continue
        if msg[0] == 1:
            stats['video'] += 1
            stats['key'] += msg[1] & 1
            await ws.send(f'a,{int.from_bytes(msg[2:6], "big")}')
        elif msg[0] == 3:
            stats['cursor'] += 1


async def main():
    png = screenshot()
    check(png[:8] == b'\x89PNG\r\n\x1a\n' and len(png) > 1000, f'GET /screenshot: {len(png)} byte PNG')
    async with websockets.connect(f'ws://{hostport}/ws', max_size=None, compression=None) as ws:
        hello = None
        t0 = time.time()
        while hello is None and time.time() - t0 < 10:
            m = await asyncio.wait_for(ws.recv(), 10)
            if isinstance(m, str) and '"hello"' in m:
                hello = json.loads(m)
        check(hello is not None and hello.get('w', 0) > 0, f'hello: {hello}')
        if want_os:
            check(hello and hello.get('os') == want_os, f'hello os is {want_os}')
        w, h = hello['w'], hello['h']
        stats = {'video': 0, 'key': 0, 'cursor': 0, 'clip': None}
        await ws.send('v,1')
        await frames(ws, 4, stats)
        check(stats['key'] >= 1, f"keyframe received ({stats['video']} frames)")
        # Click into the focused editor, then type: the text must arrive as new frames.
        await ws.send(f'm,{w // 2},{h // 2}')
        await ws.send('b,1,1')
        await ws.send('b,1,0')
        await frames(ws, 1, stats)
        check(stats['cursor'] >= 1, f"cursor image received ({stats['cursor']})")
        before = stats['video']
        for c in 'voxl smoke test ':
            await ws.send(f'k,{ord(c)},1')
            await ws.send(f'k,{ord(c)},0')
            await frames(ws, 0.05, stats)
        await frames(ws, 2, stats)
        check(stats['video'] - before >= 2, f"typing produced {stats['video'] - before} new frames")
        # Shift+letter: the layout lookup must give an uppercase letter, not a shortcut.
        await ws.send('k,65505,1')
        await ws.send('k,86,1')
        await ws.send('k,86,0')
        await ws.send('k,65505,0')
        # Clipboard from the viewer.
        await ws.send('c,voxl-clipboard-check')
        await frames(ws, 1, stats)
    after = screenshot()
    check(after != png, 'screen changed after input')
    if os.environ.get('SMOKE_SHOT'):
        open(os.environ['SMOKE_SHOT'], 'wb').write(after)
    if fails:
        print(f'{len(fails)} check(s) failed')
        sys.exit(1)


asyncio.run(main())
