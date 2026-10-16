// Microphone capture: posts 20 ms mono Float32 chunks (960 samples at 48 kHz).
class MicCapture extends AudioWorkletProcessor {
  constructor() {
    super();
    this.buf = new Float32Array(960);
    this.n = 0;
  }
  process(inputs) {
    const ch = inputs[0] && inputs[0][0];
    if (!ch) return true;
    for (let i = 0; i < ch.length; i++) {
      this.buf[this.n++] = ch[i];
      if (this.n === this.buf.length) {
        this.port.postMessage(this.buf, [this.buf.buffer]);
        this.buf = new Float32Array(960);
        this.n = 0;
      }
    }
    return true;
  }
}
registerProcessor('mic-capture', MicCapture);
