// The playground's realtime transcription: captures the microphone as PCM16 at `rate`, in chunks
// of 100 ms, posted to the main thread. Each output sample averages the input samples it covers,
// which keeps resampling from aliasing. A file of its own because the page's content security
// policy only runs scripts from the gateway, not from blob: URLs.
class PcmCapture extends AudioWorkletProcessor {
  constructor(options) {
    super();
    this.ratio = sampleRate / options.processorOptions.rate;
    this.size = Math.round(options.processorOptions.rate / 10);
    this.out = new Int16Array(this.size);
    this.n = 0;
    this.sum = 0;
    this.count = 0;
    this.acc = 0;
    this.last = 0;
  }
  process(inputs) {
    const ch = inputs[0] && inputs[0][0];
    if (!ch) return true;
    for (let i = 0; i < ch.length; i++) {
      this.sum += ch[i];
      this.count++;
      this.acc += 1;
      while (this.acc >= this.ratio) {
        this.acc -= this.ratio;
        if (this.count) {
          this.last = this.sum / this.count;
          this.sum = 0;
          this.count = 0;
        }
        const v = Math.max(-1, Math.min(1, this.last));
        this.out[this.n++] = v < 0 ? v * 0x8000 : v * 0x7fff;
        if (this.n === this.size) {
          this.port.postMessage(this.out.buffer, [this.out.buffer]);
          this.out = new Int16Array(this.size);
          this.n = 0;
        }
      }
    }
    return true;
  }
}
registerProcessor("pcm-capture", PcmCapture);
