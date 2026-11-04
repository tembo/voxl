"""Summarizes a voxl log written with VOXL_DEBUG_STRIPES=1: how much time the encode
thread spent, how many frames took longer than two frame intervals at 60 FPS (33 ms),
and how often frames were encoded twice (an overshoot re-encoded) or sharpening dropped.
Usage: stripelog.py <voxl log>"""
import json, re, sys

text = open(sys.argv[1]).read()
wall = [int(m) / 1000 for m in re.findall(r'encode (\d+) us wall', text)]
print(json.dumps({
    'frames': len(wall),
    'encode_thread_s': round(sum(wall) / 1000, 2),
    'over_33ms': sum(1 for w in wall if w > 33.3),
    'slowest_ms': round(max(wall, default=0)),
    'reencoded': text.count(' again at qp '),
    'sharpening_dropped': text.count('paint-over overshot'),
}))
