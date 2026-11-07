"""Native decode benchmark. Run with bench/py.sh; no physical-display latency claim.
Usage: measure.py host:port seconds voxl_pid compositor_pid source_pid
"""
import asyncio
import json
import os
import sys
import time
import urllib.request

import av
import websockets

host, duration = sys.argv[1], float(sys.argv[2])
pids = [int(x) for x in sys.argv[3:]]


def ticks(pid):
    # comm can contain spaces: fields after the last ')' start at proc field 3.
    fields = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
    return (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")


def stats():
    with urllib.request.urlopen(f"http://{host}/stats") as response:
        return json.load(response)


def quantile(values, q):
    values = sorted(values)
    return round(values[min(int(len(values) * q), len(values) - 1)], 2) if values else None


async def main():
    ctx = av.CodecContext.create("h264", "r")
    ctx.thread_count = 1
    ctx.flags |= av.codec.context.Flags.low_delay
    latencies, decode = [], []
    seen = set()
    last = None
    source_first = source_last = None
    byte_count = 0
    start = time.monotonic()
    warmup = start + 4
    end = warmup + duration
    cpu0 = before = measured = None
    async with websockets.connect(f"ws://{host}/ws", compression=None, max_size=None) as ws:
        await ws.send("v,1")
        while time.monotonic() < end:
            now = time.monotonic()
            if before is None and now >= warmup:
                before, cpu0, measured = stats(), [ticks(p) for p in pids], now
            try:
                msg = await asyncio.wait_for(ws.recv(), min(.25, max(.001, end-now)))
            except asyncio.TimeoutError:
                continue
            if isinstance(msg, str):
                event = json.loads(msg)
                if event.get("t") == "ping":
                    await ws.send(f"po,{event['s']}")
                continue
            if msg[0] != 1:
                continue
            assert msg[12] == 1, "benchmark expects one stripe"
            fid = int.from_bytes(msg[2:6], "big")
            t = time.monotonic()
            frames = ctx.decode(av.Packet(msg[16:]))
            elapsed = (time.monotonic()-t)*1000
            await ws.send(f"a,{fid}")
            if before is None:
                continue
            byte_count += len(msg)
            decode.append(elapsed)
            for frame in frames:
                pixels = memoryview(frame.planes[0])
                stride = frame.planes[0].line_size
                marker = sum((pixels[(b//20*24+12)*stride+b%20*24+12] > 128) << b for b in range(40))
                sequence, timestamp = marker >> 24, marker & 0xffffff
                if sequence != last:
                    last = sequence
                    seen.add(sequence)
                    source_last = (sequence, time.monotonic())
                    if source_first is None:
                        source_first = source_last
                    age = ((int(time.time()*1000) & 0xffffff)-timestamp) & 0xffffff
                    if age < 10000:
                        latencies.append(age)
        after, cpu1 = stats(), [ticks(p) for p in pids]
    elapsed = time.monotonic()-measured
    delta = {key: after[key]-before[key] for key in before if isinstance(before[key], int)}
    encoded = delta["encoded"]
    result = {
        "seconds": round(elapsed, 3), "unique_fps": round(len(seen)/elapsed, 2),
        "latency_ms": {"p50": quantile(latencies, .5), "p95": quantile(latencies, .95), "p99": quantile(latencies, .99)},
        "source_progress_fps": (round(((source_last[0]-source_first[0]) & 0xffff)/(source_last[1]-source_first[1]), 2) if source_first and source_last[1] > source_first[1] else None),
        "decode_ms_p50": quantile(decode, .5),
        "cpu_percent": [round((b-a)/elapsed*100, 2) for a, b in zip(cpu0, cpu1)],
        "server_plus_compositor_cpu_percent": round(sum(b-a for a,b in zip(cpu0[:2],cpu1[:2]))/elapsed*100, 2),
        "mbps": round(byte_count*8/elapsed/1e6, 3), "stats_delta": delta,
        "convert_ms_per_frame": round(delta["convert_us_total"]/max(1, encoded)/1000, 3),
        "encode_ms_per_frame": round(delta["encode_us_total"]/max(1, encoded)/1000, 3),
    }
    print(json.dumps(result))


asyncio.run(main())
