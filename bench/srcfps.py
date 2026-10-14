"""True source frame rate: hash Xvfb's shm framebuffer (sparse sample) at 1 kHz, count changes/s."""
import ctypes, sys, time, zlib
secs = float(sys.argv[1]) if len(sys.argv) > 1 else 10
libc = ctypes.CDLL(None)
libc.shmat.restype = ctypes.c_void_p
shmid = int(sys.argv[2]) if len(sys.argv) > 2 else 0
addr = libc.shmat(shmid, None, 0o10000)  # SHM_RDONLY
hdr = (ctypes.c_ubyte * 100).from_address(addr)
be = lambda i: int.from_bytes(bytes(hdr[i*4:i*4+4]), 'big')
hs, w, h, bpl, nc = be(0), be(4), be(5), be(12), be(19)
off = hs + nc * 12
buf = (ctypes.c_ubyte * (bpl * h)).from_address(addr + off)
mv = memoryview(buf).cast('B')
rows = range(0, h, 23)
last, changes, per = None, 0, []
t0 = time.time(); s0 = t0
while time.time() - t0 < secs:
    hsh = 0
    for y in rows:
        hsh = zlib.crc32(mv[y*bpl:(y+1)*bpl], hsh)
    if hsh != last:
        changes += 1; last = hsh
    now = time.time()
    if now - s0 >= 1: per.append(changes / (now - s0)); changes = 0; s0 = now
    time.sleep(0.001)
per = sorted(per[1:])
print(f"source changes/s avg {sum(per)/len(per):.1f} min {per[0]:.1f} med {per[len(per)//2]:.1f}")
