// Jitter-buffered PCM player. Receives planar Float32 [L, R] chunks on a MessagePort.
class PcmPlayer extends AudioWorkletProcessor {
  constructor() {
    super();
    this.cap = 48000; // 1 s ring
    this.l = new Float32Array(this.cap);
    this.r = new Float32Array(this.cap);
    this.rd = 0; this.wr = 0; this.fill = 0;
    this.target = 2400; // 50 ms
    this.priming = true;
    this.port.onmessage = (e) => {
      if (e.data.port) { e.data.port.onmessage = (m) => this.push(m.data); }
    };
  }
  push({ l, r }) {
    for (let i = 0; i < l.length; i++) {
      this.l[this.wr] = l[i]; this.r[this.wr] = r ? r[i] : l[i];
      this.wr = (this.wr + 1) % this.cap;
    }
    this.fill += l.length;
    // Too much queued: drop the oldest so latency stays bounded.
    if (this.fill > this.target * 3) {
      const drop = this.fill - this.target;
      this.rd = (this.rd + drop) % this.cap; this.fill -= drop;
    }
  }
  process(_, outputs) {
    const out = outputs[0], n = out[0].length;
    if (this.priming) { if (this.fill < this.target) return true; this.priming = false; }
    if (this.fill < n) { this.priming = true; return true; }
    for (let i = 0; i < n; i++) {
      out[0][i] = this.l[this.rd];
      if (out[1]) out[1][i] = this.r[this.rd];
      this.rd = (this.rd + 1) % this.cap;
    }
    this.fill -= n;
    return true;
  }
}
registerProcessor('pcm-player', PcmPlayer);
