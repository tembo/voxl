"""Exercise real macOS display changes without recording the desktop or credentials.

Requires `websockets`. Usage: python bench/display_smoke.py target/release/voxl
Add --check-pointer to also move (and restore) the cursor through voxl's input API.
The temporary servers bind only to loopback and use an ephemeral in-memory password.
"""
import argparse
import asyncio
import ctypes
import json
import os
from pathlib import Path
import secrets
import socket
import struct
import subprocess
import sys
import time
import urllib.error
import urllib.request

from websockets.asyncio.client import connect


class Point(ctypes.Structure):
    _fields_ = [("x", ctypes.c_double), ("y", ctypes.c_double)]


class Size(ctypes.Structure):
    _fields_ = [("width", ctypes.c_double), ("height", ctypes.c_double)]


class Rect(ctypes.Structure):
    _fields_ = [("origin", Point), ("size", Size)]


def graphics():
    cg = ctypes.CDLL("/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics")
    cg.CGDisplayBounds.argtypes = [ctypes.c_uint32]
    cg.CGDisplayBounds.restype = Rect
    cg.CGEventCreate.argtypes = [ctypes.c_void_p]
    cg.CGEventCreate.restype = ctypes.c_void_p
    cg.CGEventGetLocation.argtypes = [ctypes.c_void_p]
    cg.CGEventGetLocation.restype = Point
    cf = ctypes.CDLL("/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation")
    cf.CFRelease.argtypes = [ctypes.c_void_p]
    return cg, cf


def pointer(cg, cf):
    event = cg.CGEventCreate(None)
    try:
        return cg.CGEventGetLocation(event)
    finally:
        cf.CFRelease(event)


def request(port, path, password=None):
    headers = {"Authorization": f"Bearer {password}"} if password else {}
    req = urllib.request.Request(f"http://127.0.0.1:{port}{path}", headers=headers)
    with urllib.request.urlopen(req, timeout=15) as response:
        return response.read()


async def receive(ws, predicate, timeout=15):
    async with asyncio.timeout(timeout):
        while True:
            message = await ws.recv()
            if isinstance(message, str):
                message = json.loads(message)
                if message.get("t") == "ping":
                    await ws.send(f"po,{message['s']}")
            elif message[0] == 1:
                await ws.send(f"a,{int.from_bytes(message[2:6], 'big')}")
            if predicate(message):
                return message


def is_hello(message):
    return isinstance(message, dict) and message.get("t") == "hello"


async def start_server(binary, password, read_only=False):
    with socket.socket() as available:
        available.bind(("127.0.0.1", 0))
        port = available.getsockname()[1]
    env = dict(os.environ, VOXL_PASSWORD=password)
    process = subprocess.Popen(
        [str(binary), f"--port={port}", "--addr=127.0.0.1", "--audio-enabled=false",
         "--file-transfers=none", "--enable-clipboard=false", "--framerate=15",
         f"--read-only={str(read_only).lower()}"],
        env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    try:
        for _ in range(150):
            if process.poll() is not None:
                raise RuntimeError("Test server exited: check Screen Recording permission")
            try:
                if await asyncio.to_thread(request, port, "/healthz") == b"ok":
                    return process, port
            except (OSError, urllib.error.URLError):
                pass
            await asyncio.sleep(0.1)
        raise RuntimeError("Test server did not become ready")
    except BaseException:
        process.terminate()
        process.wait(timeout=10)
        raise


async def main(binary, check_pointer):
    password = secrets.token_urlsafe(24)
    process, port = await start_server(binary, password)
    headers = {"Authorization": f"Bearer {password}"}
    cg, cf = graphics()
    original = pointer(cg, cf) if check_pointer else None
    try:
        try:
            await asyncio.to_thread(request, port, "/screenshot")
            raise AssertionError("Unauthenticated screenshot was accepted")
        except urllib.error.HTTPError as error:
            assert error.code == 401
        uri = f"ws://127.0.0.1:{port}/ws"
        async with connect(uri, additional_headers=headers, max_size=32 << 20) as ws, connect(
            uri, additional_headers=headers, max_size=32 << 20
        ) as observer:
            hello = await receive(ws, is_hello)
            await receive(observer, is_hello)
            available = hello["displays"]["available"]
            assert len(available) >= 2, "Connect at least two displays for this test"
            print(f"Found {len(available)} displays; starting repeated live switches", flush=True)
            await ws.send(f"da,{hello['displays']['generation']}")
            await ws.send("ds,4294967295")
            await receive(ws, lambda m: isinstance(m, dict) and m.get("t") == "display-error")
            await ws.send("v,1")
            try:
                for iteration in range(8):
                    current = hello["displays"]["selected"]
                    target = next(d["id"] for d in available if d["id"] != current)
                    generation = hello["displays"]["generation"]
                    started = time.monotonic()
                    await ws.send(f"ds,{target}")
                    hello = await receive(ws, is_hello)
                    other = await receive(observer, is_hello)
                    assert hello["displays"]["selected"] == target
                    assert hello["displays"]["generation"] == generation + 1
                    assert other["displays"] == hello["displays"], "Viewers diverged"
                    await ws.send(f"da,{hello['displays']['generation']}")
                    frame = await receive(ws, lambda m: isinstance(m, bytes) and m[0] == 1)
                    assert frame[1] & 1, "New display did not begin with a keyframe"
                    png = await asyncio.to_thread(request, port, "/screenshot", password)
                    assert png[:8] == b"\x89PNG\r\n\x1a\n"
                    assert struct.unpack(">II", png[16:24]) == (hello["w"], hello["h"])
                    if check_pointer:
                        bounds = cg.CGDisplayBounds(target)
                        scale = bounds.size.width / hello["w"]
                        await ws.send("m,80,100")
                        expected = (bounds.origin.x + 80.5 * scale, bounds.origin.y + 100.5 * scale)
                        for _ in range(50):
                            await asyncio.sleep(0.02)
                            actual = pointer(cg, cf)
                            if abs(actual.x - expected[0]) <= 1 and abs(actual.y - expected[1]) <= 1:
                                break
                        else:
                            raise AssertionError("Pointer did not follow the selected display")
                    print(f"Switch {iteration + 1}: display {target}, {hello['w']}x{hello['h']}, keyframe + PNG OK ({time.monotonic() - started:.2f}s)", flush=True)
            finally:
                if original:
                    bounds = cg.CGDisplayBounds(hello["displays"]["selected"])
                    scale = hello["w"] / bounds.size.width
                    x = round((original.x - bounds.origin.x) * scale - 0.5)
                    y = round((original.y - bounds.origin.y) * scale - 0.5)
                    await ws.send(f"m,{x},{y}")
                    await asyncio.sleep(0.1)
        assert process.poll() is None, "Switching exited the server"
    finally:
        process.terminate()
        process.wait(timeout=10)

    process, port = await start_server(binary, password, read_only=True)
    try:
        async with connect(f"ws://127.0.0.1:{port}/ws", additional_headers=headers) as ws:
            before = await receive(ws, is_hello)
            target = next(d["id"] for d in before["displays"]["available"] if d["id"] != before["displays"]["selected"])
            await ws.send(f"ds,{target}")
            try:
                await receive(ws, is_hello, timeout=0.5)
                raise AssertionError("Read-only viewer changed the display")
            except TimeoutError:
                pass
        async with connect(f"ws://127.0.0.1:{port}/ws", additional_headers=headers) as ws:
            after = await receive(ws, is_hello)
            assert after["displays"] == before["displays"]
        print("PASS: read-only protection, authentication, two viewers, repeated capture and input geometry", flush=True)
    finally:
        process.terminate()
        process.wait(timeout=10)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--check-pointer", action="store_true")
    args = parser.parse_args()
    if sys.platform != "darwin":
        raise SystemExit("This smoke test requires macOS and two attached displays")
    asyncio.run(main(args.binary.resolve(), args.check_pointer))
