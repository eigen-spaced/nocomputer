// Offline WebCodecs proof. All keys are local viewer controls; nothing is sent anywhere.
const { invoke } = window.__TAURI__.core;

const ACCELERATION = { 'default': 'no-preference', 'require-hardware': 'prefer-hardware', 'software': 'prefer-software' };
const FRAME_US = 33_333;
const MAX_DECODE_QUEUE = 4;
const PAN_STEP = 64;

const canvas = document.getElementById('video');
const ctx = canvas.getContext('2d', { alpha: false });
const statusEl = document.getElementById('status');

async function loadStream() {
  const buf = await invoke('load_stream');
  const headerLen = new DataView(buf).getUint32(0, true);
  const header = JSON.parse(new TextDecoder().decode(new Uint8Array(buf, 4, headerLen)));
  let offset = 4 + headerLen;
  const chunks = header.units.map((u, i) => {
    const chunk = new EncodedVideoChunk({
      type: u.key ? 'key' : 'delta',
      timestamp: i * FRAME_US,
      duration: FRAME_US,
      data: new Uint8Array(buf, offset, u.len),
    });
    offset += u.len;
    return chunk;
  });
  return { codec: header.codec, chunks };
}

function decoderConfig(codec, decode) {
  // No `description`: the chunks are Annex B with in-band SPS/PPS.
  return { codec, hardwareAcceleration: ACCELERATION[decode], optimizeForLatency: true };
}

function waitForDequeue(decoder) {
  return new Promise(resolve => decoder.addEventListener('dequeue', resolve, { once: true }));
}

async function runCheck(launch) {
  const started = performance.now();
  const summary = {
    tool: 'nocomputer-viewer-webcodecs', file: launch.file_name, decode: launch.decode,
    codec: null, hardwareAcceleration: ACCELERATION[launch.decode], configSupported: null,
    chunks: 0, frames: 0, width: null, height: null, sizeConsistent: true, outputInOrder: true,
    maxDecodeQueue: 0, decodeErrors: [], completed: false, elapsedMs: null, userAgent: navigator.userAgent,
  };
  try {
    const { codec, chunks } = await loadStream();
    summary.codec = codec;
    summary.chunks = chunks.length;
    const config = decoderConfig(codec, launch.decode);
    summary.configSupported = (await VideoDecoder.isConfigSupported(config)).supported;
    if (!summary.configSupported) throw new Error(`config not supported: ${JSON.stringify(config)}`);

    const decoder = new VideoDecoder({
      output: frame => {
        // No B-frames are expected, so frame i must carry chunk i's timestamp.
        if (frame.timestamp !== summary.frames * FRAME_US) summary.outputInOrder = false;
        summary.frames++;
        if (summary.width === null) { summary.width = frame.displayWidth; summary.height = frame.displayHeight; }
        else if (frame.displayWidth !== summary.width || frame.displayHeight !== summary.height) summary.sizeConsistent = false;
        frame.close();
      },
      error: e => summary.decodeErrors.push(`${e.name}: ${e.message}`),
    });
    decoder.configure(config);
    for (const chunk of chunks) {
      while (decoder.decodeQueueSize >= MAX_DECODE_QUEUE) await waitForDequeue(decoder);
      decoder.decode(chunk);
      summary.maxDecodeQueue = Math.max(summary.maxDecodeQueue, decoder.decodeQueueSize);
    }
    await decoder.flush();
    decoder.close();
  } catch (e) {
    if (!summary.decodeErrors.length) summary.decodeErrors.push(`${e.name}: ${e.message}`);
  }
  summary.elapsedMs = Math.round(performance.now() - started);
  summary.completed = summary.decodeErrors.length === 0 && summary.chunks > 0 && summary.frames === summary.chunks;
  const pass = summary.completed && summary.sizeConsistent && summary.outputInOrder;
  await invoke('finish_check', { summary, pass });
}

// ---- visible playback ----

const view = { mode: 'fit', panX: 0, panY: 0, fullscreen: false, showStatus: true };
let current = null; // latest VideoFrame, held at EOF
let info = { codec: '', decode: '', decoded: 0, total: 0, ended: false, error: null };

function resizeCanvas() {
  const dpr = window.devicePixelRatio || 1;
  canvas.width = Math.round(canvas.clientWidth * dpr);
  canvas.height = Math.round(canvas.clientHeight * dpr);
  draw();
}

// Destination rectangle in canvas (device) pixels.
function contentRect(vw, vh) {
  const cw = canvas.width, ch = canvas.height;
  if (view.mode === 'fit') {
    const s = Math.min(cw / vw, ch / vh);
    const w = vw * s, h = vh * s;
    return { x: (cw - w) / 2, y: (ch - h) / 2, w, h };
  }
  // Native: one video pixel per output pixel. Centre when smaller; pan within bounds when larger.
  const maxX = Math.max(0, (vw - cw) / 2), maxY = Math.max(0, (vh - ch) / 2);
  view.panX = Math.min(maxX, Math.max(-maxX, view.panX));
  view.panY = Math.min(maxY, Math.max(-maxY, view.panY));
  return { x: Math.round((cw - vw) / 2 - view.panX), y: Math.round((ch - vh) / 2 - view.panY), w: vw, h: vh };
}

function draw() {
  ctx.fillStyle = '#000';
  ctx.fillRect(0, 0, canvas.width, canvas.height);
  if (current) {
    const r = contentRect(current.displayWidth, current.displayHeight);
    ctx.imageSmoothingEnabled = view.mode === 'fit';
    ctx.imageSmoothingQuality = 'high';
    ctx.drawImage(current, r.x, r.y, r.w, r.h);
  }
  const state = info.error ? `error: ${info.error}` : info.ended ? 'ended (holding last frame)' : 'playing';
  statusEl.textContent = `${info.codec} ${info.decode} | ${info.decoded}/${info.total} frames | ${view.mode} | ${state} | N native  arrows pan  R reset  F fullscreen  H hide`;
  statusEl.classList.toggle('hidden', !view.showStatus);
}

async function play(launch) {
  info.decode = ACCELERATION[launch.decode];
  try {
    const { codec, chunks } = await loadStream();
    Object.assign(info, { codec, total: chunks.length });
    const decoder = new VideoDecoder({
      output: frame => {
        current?.close();
        current = frame;
        info.decoded++;
        draw();
      },
      error: e => { info.error = `${e.name}: ${e.message}`; draw(); },
    });
    decoder.configure(decoderConfig(codec, launch.decode));
    let next = 0;
    const timer = setInterval(async () => {
      if (info.error) return clearInterval(timer);
      if (next === chunks.length) {
        clearInterval(timer);
        await decoder.flush().catch(() => {});
        info.ended = true;
        draw();
        return;
      }
      // Paced at 30 Hz; skip a tick rather than queue when the decoder is behind.
      if (decoder.decodeQueueSize < MAX_DECODE_QUEUE) decoder.decode(chunks[next++]);
    }, FRAME_US / 1000);
  } catch (e) {
    info.error = `${e.name}: ${e.message}`;
    draw();
  }
}

async function setFullscreen(on) {
  view.fullscreen = on;
  await invoke('set_fullscreen', { on });
}

window.addEventListener('keydown', e => {
  const k = e.key.toLowerCase();
  if (k === 'n') { view.mode = view.mode === 'fit' ? 'native' : 'fit'; view.panX = view.panY = 0; }
  else if (k === 'r') { view.panX = view.panY = 0; }
  else if (k === 'h') { view.showStatus = !view.showStatus; }
  else if (k === 'f') { setFullscreen(!view.fullscreen); }
  else if (k === 'escape') { if (view.fullscreen) setFullscreen(false); }
  else if (view.mode === 'native' && e.key.startsWith('Arrow')) {
    if (e.key === 'ArrowLeft') view.panX -= PAN_STEP;
    if (e.key === 'ArrowRight') view.panX += PAN_STEP;
    if (e.key === 'ArrowUp') view.panY -= PAN_STEP;
    if (e.key === 'ArrowDown') view.panY += PAN_STEP;
  } else return;
  e.preventDefault();
  draw();
});

window.addEventListener('resize', resizeCanvas);

(async () => {
  resizeCanvas();
  const launch = await invoke('launch_options');
  if (launch.check) await runCheck(launch);
  else await play(launch);
})();
