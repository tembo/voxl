"""TCP proxy emulating a bottleneck link: rate limit (Mbit/s) on server->client,
plus fixed one-way delay each way. Queue is unbounded (like a bufferbloated path).
Usage: shaper.py <listen_port> <target_host:port> <mbps> <rtt_ms>"""
import asyncio, sys, time

listen, target, mbps, rtt = int(sys.argv[1]), sys.argv[2], float(sys.argv[3]), float(sys.argv[4])
th, tp = target.split(':')

async def pipe(reader, writer, rate_bps, delay):
    q = asyncio.Queue()
    async def sender():
        while True:
            t, data = await q.get()
            if data is None:
                break
            dt = t - time.monotonic()
            if dt > 0:
                await asyncio.sleep(dt)
            writer.write(data)
            await writer.drain()
        writer.close()
    task = asyncio.create_task(sender())
    last = time.monotonic()
    while True:
        data = await reader.read(65536)
        if not data:
            await q.put((0, None)); break
        now = time.monotonic()
        if rate_bps:
            last = max(last, now) + len(data) * 8 / rate_bps
            t = last + delay
        else:
            t = now + delay
        await q.put((t, data))
    await task

async def handle(cr, cw):
    sr, sw = await asyncio.open_connection(th, int(tp))
    await asyncio.gather(pipe(cr, sw, 0, rtt / 2000), pipe(sr, cw, mbps * 1e6, rtt / 2000), return_exceptions=True)

async def main():
    srv = await asyncio.start_server(handle, '127.0.0.1', listen)
    async with srv:
        await srv.serve_forever()
asyncio.run(main())
