// IPC latency bench (see src-tauri/src/bench.rs). Prints one JSON summary and exits.
const { invoke, Channel } = window.__TAURI__.core;

const FPS = 60;
const FRAMES = 600; // 10 s per phase
// Rough 1080p60 H.264 sizes: typical delta frame, busy delta frame, keyframe.
const SIZES = [32 * 1024, 128 * 1024, 512 * 1024];
const TRANSPORTS = ['push', 'pull'];

const statusEl = document.getElementById('status');
const canvas = document.getElementById('video');
// Monotonic; mapped onto the Rust process's monotonic clock by the calibrated offset.
const nowUs = () => performance.now() * 1000;

// NTP-style: the fastest round trip bounds the offset error to half its length.
async function calibrate() {
  let best = Infinity, offsetUs = 0;
  for (let i = 0; i < 200; i++) {
    const t0 = nowUs();
    const rust = await invoke('bench_now');
    const t1 = nowUs();
    if (t1 - t0 < best) { best = t1 - t0; offsetUs = rust - (t0 + t1) / 2; }
  }
  return { offsetUs, errorBoundMs: best / 2000 };
}

// Optional load: hardware-decode the launch file in a loop at FPS and draw every frame,
// as the live viewer will. Restarting at chunk 0 is safe because it is an IDR.
async function startDecodeLoad() {
  const launch = await invoke('launch_options').catch(() => null);
  if (!launch) return null;
  const buf = await invoke('load_stream');
  const headerLen = new DataView(buf).getUint32(0, true);
  const header = JSON.parse(new TextDecoder().decode(new Uint8Array(buf, 4, headerLen)));
  let offset = 4 + headerLen;
  const units = header.units.map(u => { const d = new Uint8Array(buf, offset, u.len); offset += u.len; return { key: u.key, data: d }; });
  const ctx = canvas.getContext('2d', { alpha: false });
  const load = { file: launch.file_name, codec: header.codec, decoded: 0, errors: 0, stop: false };
  const decoder = new VideoDecoder({
    output: frame => {
      if (canvas.width !== frame.displayWidth) { canvas.width = frame.displayWidth; canvas.height = frame.displayHeight; }
      ctx.drawImage(frame, 0, 0);
      frame.close();
      load.decoded++;
    },
    error: () => { load.errors++; },
  });
  decoder.configure({ codec: header.codec, hardwareAcceleration: 'no-preference', optimizeForLatency: true });
  let i = 0, n = 0;
  const tick = () => {
    if (load.stop || decoder.state !== 'configured') return;
    if (decoder.decodeQueueSize < 4) {
      const u = units[i];
      decoder.decode(new EncodedVideoChunk({ type: u.key ? 'key' : 'delta', timestamp: n * 1e6 / FPS, data: u.data }));
      i = (i + 1) % units.length; n++;
    }
    setTimeout(tick, 1000 / FPS);
  };
  tick();
  return load;
}

function newPhase(transport, size) {
  return { transport, size, received: 0, badSize: 0, outOfOrder: 0, lastSeq: -1, latencies: [], arrivals: [] };
}

function record(phase, msg, offsetUs) {
  const arrived = nowUs() + offsetUs;
  const u8 = msg instanceof ArrayBuffer ? new Uint8Array(msg) : new Uint8Array(msg.buffer, msg.byteOffset, msg.byteLength);
  const dv = new DataView(u8.buffer, u8.byteOffset, u8.byteLength);
  const due = dv.getFloat64(0, true), seq = dv.getUint32(8, true);
  if (u8.byteLength !== phase.size) phase.badSize++;
  if (seq !== phase.lastSeq + 1) phase.outOfOrder++;
  phase.lastSeq = seq;
  phase.received++;
  phase.latencies.push((arrived - due) / 1000);
  phase.arrivals.push(arrived / 1000);
}

function runPush(phase, offsetUs) {
  return new Promise(async resolve => {
    const timer = setTimeout(resolve, (FRAMES / FPS) * 1000 + 5000);
    const channel = new Channel();
    channel.onmessage = msg => {
      record(phase, msg, offsetUs);
      if (phase.received === FRAMES) { clearTimeout(timer); resolve(); }
    };
    await invoke('bench_push', { channel, size: phase.size, frames: FRAMES, fps: FPS });
  });
}

async function runPull(phase, offsetUs) {
  await invoke('bench_pull_start', { size: phase.size, frames: FRAMES, fps: FPS });
  for (let seq = 0; seq < FRAMES; seq++) {
    record(phase, await invoke('bench_pull', { seq }), offsetUs);
  }
}

function pct(sorted, p) {
  return sorted.length ? sorted[Math.min(sorted.length - 1, Math.floor(p / 100 * sorted.length))] : null;
}

function summarize(phase) {
  const r = v => (v === null ? null : Math.round(v * 100) / 100);
  const lat = [...phase.latencies].sort((a, b) => a - b);
  const frameMs = 1000 / FPS;
  // Deviation of each arrival gap from the frame interval: what a renderer would see as stutter.
  const gaps = phase.arrivals.slice(1).map((t, i) => Math.abs(t - phase.arrivals[i] - frameMs)).sort((a, b) => a - b);
  return {
    transport: phase.transport, sizeKiB: phase.size / 1024, received: phase.received, expected: FRAMES,
    badSize: phase.badSize, outOfOrder: phase.outOfOrder,
    latencyMs: { p50: r(pct(lat, 50)), p95: r(pct(lat, 95)), p99: r(pct(lat, 99)), max: r(lat.at(-1) ?? null) },
    lateOverOneFrame: lat.filter(v => v > frameMs).length,
    gapDeviationMs: { p50: r(pct(gaps, 50)), p95: r(pct(gaps, 95)), max: r(gaps.at(-1) ?? null) },
  };
}

async function main() {
  statusEl.textContent = 'calibrating clock offset';
  const cal = await calibrate();
  const load = await startDecodeLoad();
  const results = [];
  for (const size of SIZES) {
    for (const transport of TRANSPORTS) {
      statusEl.textContent = `${transport} ${size / 1024} KiB at ${FPS} fps (${results.length + 1}/${SIZES.length * TRANSPORTS.length})`;
      const phase = newPhase(transport, size);
      await (transport === 'push' ? runPush : runPull)(phase, cal.offsetUs);
      results.push(summarize(phase));
    }
  }
  if (load) load.stop = true;
  const summary = {
    tool: 'nocomputer-viewer-ipc-bench', fps: FPS, framesPerPhase: FRAMES,
    decodeLoad: load && { file: load.file, codec: load.codec, framesDecoded: load.decoded, errors: load.errors },
    clockOffsetErrorBoundMs: Math.round(cal.errorBoundMs * 1000) / 1000,
    results, userAgent: navigator.userAgent,
  };
  const pass = results.every(x => x.received === FRAMES && x.badSize === 0 && x.outOfOrder === 0);
  await invoke('finish_check', { summary, pass });
}

main().catch(e => invoke('finish_check', { summary: { tool: 'nocomputer-viewer-ipc-bench', error: String(e) }, pass: false }));
