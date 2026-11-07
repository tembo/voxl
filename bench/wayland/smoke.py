"""Behavior checks against a running server and native loadgen in manual mode.
Usage: smoke.py host:port loadgen_pid [wayland|x11]
"""
import asyncio
import io
import json
import os
import signal
import sys
import time
import urllib.request

import av
from PIL import Image
import websockets

host, pid = sys.argv[1], int(sys.argv[2])
backend = sys.argv[3] if len(sys.argv) > 3 else "wayland"
step = 0


def get(path):
    return urllib.request.urlopen(f"http://{host}{path}", timeout=4)


def stats():
    with get("/stats") as response:
        return json.load(response)


def screenshot():
    with get("/screenshot") as response:
        return Image.open(io.BytesIO(response.read())).convert("RGB"), int(response.headers["X-Voxl-Seq"])


def advance():
    global step
    os.kill(pid, signal.SIGUSR1)
    step += 1


def expected():
    return ((255 if ((step+1)//2)%2 else 0, 0, 0), (0, 255 if (step//2)%2 else 0, 0))


def matches(image, tolerance=0):
    return all(abs(a-b) <= tolerance for point, color in zip([(620,620), (1020,1020)], expected())
               for a,b in zip(image.getpixel(point), color))


async def until(predicate, timeout=3):
    end = time.monotonic()+timeout
    while not predicate():
        assert time.monotonic() < end, "condition timed out"
        await asyncio.sleep(.025)


async def main():
    await until(lambda: matches(screenshot()[0]))
    before = stats()
    _, seq = screenshot()
    change = asyncio.create_task(asyncio.to_thread(lambda: json.load(get(f"/wait?change&since={seq}&region=600,600,64,64&timeout=2000"))))
    advance()
    result = await change
    assert result["changed"] and not result["timed_out"], result
    await until(lambda: matches(screenshot()[0]))
    with get(f"/wait?change&since={seq}&region=1500,1200,64,64&timeout=150") as response:
        outside = json.load(response)
    assert outside["timed_out"] and not outside["changed"], outside
    # Force alternating SHM slots and snapshots without any video viewer.
    for _ in range(7):
        advance()
        await until(lambda: matches(screenshot()[0]))
    after = stats()
    assert after["encoded"] == before["encoded"], "no-viewer activity encoded video"
    with get("/wait?idle=200&timeout=1500") as response:
        idle = json.load(response)
    assert not idle["timed_out"], idle

    latest = None
    held = []
    holding = False
    ctx = av.CodecContext.create("h264", "r")
    ctx.thread_count = 1
    ctx.flags |= av.codec.context.Flags.low_delay
    async with websockets.connect(f"ws://{host}/ws", compression=None, max_size=None) as ws:
        await ws.send("v,1")

        async def receive():
            nonlocal latest
            async for msg in ws:
                if isinstance(msg, str):
                    event = json.loads(msg)
                    if event.get("t") == "ping":
                        await ws.send(f"po,{event['s']}")
                    continue
                if msg[0] != 1:
                    continue
                fid = int.from_bytes(msg[2:6], "big")
                for frame in ctx.decode(av.Packet(msg[16:])):
                    latest = frame.to_image()
                if holding:
                    held.append(fid)
                else:
                    await ws.send(f"a,{fid}")

        receiver = asyncio.create_task(receive())
        await until(lambda: latest is not None and matches(latest, 35))
        # Keep decoding, withhold ACKs, and change both distant regions after the
        # shared capture gate blocks. Resuming must include skipped-frame damage.
        holding = True
        blocked0 = stats()["flow_blocked"]
        for _ in range(21):
            advance()
            await asyncio.sleep(.08)
        assert stats()["flow_blocked"] > blocked0, "did not exercise backpressure"
        holding = False
        for fid in held:
            await ws.send(f"a,{fid}")
        await until(lambda: latest is not None and matches(latest, 35))
        await asyncio.sleep(.8)  # finish paintover
        encoded = stats()["encoded"]
        await asyncio.sleep(.8)
        assert stats()["encoded"] == encoded, "static screen kept encoding"
        receiver.cancel()
        try:
            await receiver
        except asyncio.CancelledError:
            pass
        await ws.close()
    print(json.dumps({"backend": backend, "passed": ["snapshot pixels and sequence", "regional /wait", "no-viewer activity without encoding", "alternating capture buffers", "H.264 decoded pixels", "damage across backpressure", "idle stops encoding"]}))


asyncio.run(main())
