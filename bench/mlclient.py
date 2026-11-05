"""Native Moonlight benchmark client: streams from Sunshine with moonlight-embedded
(built with bench/moonlight, which patches it to hand over each frame as received), decodes with
FFmpeg (PyAV) and reads the loadgen marker like nclient.py. Same JSON.
Usage: mlclient.py <host> <secs> [markerX markerY]
Env: MOONLIGHT (the patched binary), ML_HOME (where it keeps its pairing keys),
ML_BITRATE (kbit/s; Moonlight's default otherwise, 40000 at 2560x1600@60),
ML_SIZE (default 2560x1600)."""
import json, os, select, signal, struct, subprocess, sys, tempfile, time
import av

host, secs = sys.argv[1], float(sys.argv[2])
mx, my = (int(sys.argv[3]), int(sys.argv[4])) if len(sys.argv) > 4 else (300, 150)
w, h = os.environ.get('ML_SIZE', '2560x1600').split('x')

def make_ctx():
    c = av.CodecContext.create('h264', 'r')
    c.thread_type = 'SLICE'
    c.thread_count = 8
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

def read_exact(fd, n, deadline):
    """n bytes from the FIFO, or None once Moonlight has exited or the deadline passed."""
    b = bytearray()
    while len(b) < n:
        if time.time() > deadline or proc.poll() is not None:
            return None
        if not select.select([fd], [], [], 0.5)[0]:
            continue
        chunk = os.read(fd, n - len(b))
        if not chunk:  # no writer (yet, or any more)
            time.sleep(0.05)
            continue
        b += chunk
    return bytes(b)

fifo = os.path.join(tempfile.mkdtemp(), 'frames')
os.mkfifo(fifo)
env = dict(os.environ, MOONLIGHT_DUMP=fifo, HOME=os.environ.get('ML_HOME', os.environ.get('HOME', '')))
proc = subprocess.Popen([os.environ.get('MOONLIGHT', 'moonlight'), 'stream', '-platform', 'fake', '-app', 'Desktop',
                         '-width', w, '-height', h, '-fps', '60', '-codec', 'h264', '-viewonly', '-localaudio',
                         *(['-bitrate', os.environ['ML_BITRATE']] if os.environ.get('ML_BITRATE') else []), host],
                        env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
# Nonblocking: Moonlight only opens the FIFO once the stream starts, if it ever does.
fd = os.open(fifo, os.O_RDONLY | os.O_NONBLOCK)
ctx = make_ctx()
seen_per_sec, lat, rlat, cur, last = [], [], [], set(), None
dec_ms, nbytes = [], 0
t_start = None
t0 = time.time(); sec0 = None
deadline = t0 + secs + 4 + 20  # time to connect
while time.time() - t0 < secs + 4:
    hdr = read_exact(fd, 20, deadline)
    if hdr is None:
        break
    us, _frame, _ftype, n = struct.unpack('<QIII', hdr)
    payload = read_exact(fd, n, deadline)
    if payload is None:
        break
    nbytes += n
    trecv = us / 1e6
    td = time.perf_counter()
    try:
        frames = ctx.decode(av.Packet(payload))
    except av.error.InvalidDataError:
        frames = []
    dec_ms.append((time.perf_counter() - td) * 1000)
    for fr in frames:
        mn, ms = read_marker(fr)
        now = time.time()
        if now - t0 < 4:  # warmup
            continue
        if sec0 is None: sec0 = now; t_start = now; nbytes = 0
        if mn != last:
            last = mn; cur.add(mn)
            l = (int(now * 1000) & 0xffffff) - ms
            if l < -1e6: l += 0x1000000
            if 0 <= l < 10000: lat.append(l); rlat.append(l - (now - trecv) * 1000)
        if now - sec0 >= 1:
            seen_per_sec.append(len(cur) / (now - sec0)); cur = set(); sec0 = now
proc.send_signal(signal.SIGINT)
try:
    proc.wait(timeout=5)
except subprocess.TimeoutExpired:
    proc.kill()
s = sorted(seen_per_sec); l = sorted(lat); d = sorted(dec_ms)
q = lambda a, p: round(a[min(len(a) - 1, int(len(a) * p))], 1) if a else None
el = time.time() - t_start if t_start else 1
print(json.dumps({"mode": "moonlight", "uniqueFps": {"avg": round(sum(s) / max(len(s), 1), 1), "min": q(s, 0), "p10": q(s, .1), "med": q(s, .5)},
    "latencyMs": {"p50": q(l, .5), "p90": q(l, .9), "p99": q(l, .99), "max": q(l, 1)}, "decodeMs": {"p50": q(d, .5), "p99": q(d, .99)},
    "recvLatMs": {"p50": q(sorted(rlat), .5), "p90": q(sorted(rlat), .9)}, "mbps": round(nbytes * 8 / el / 1e6, 2)}))
