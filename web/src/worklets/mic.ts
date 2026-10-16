// Microphone capture: posts 20 ms mono Float32 chunks (960 samples at 48 kHz). The page
// posts each chunk back once encoded, and the worklet refills it instead of allocating.
class MicCapture extends AudioWorkletProcessor {
  private buf = new Float32Array(960);
  private n = 0;
  private spare: Float32Array[] = [];

  constructor() {
    super();
    this.port.onmessage = (e: MessageEvent<Float32Array>) => {
      if (this.spare.length < 4 && e.data.length === 960) this.spare.push(e.data);
    };
  }

  process(inputs: Float32Array[][]) {
    const ch = inputs[0] && inputs[0][0];
    if (!ch) return true;
    for (let i = 0; i < ch.length; i++) {
      this.buf[this.n++] = ch[i];
      if (this.n === this.buf.length) {
        this.port.postMessage(this.buf, [this.buf.buffer]);
        this.buf = this.spare.pop() ?? new Float32Array(960);
        this.n = 0;
      }
    }
    return true;
  }
}
registerProcessor('mic-capture', MicCapture);
