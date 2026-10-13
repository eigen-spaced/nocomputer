// WebCodecs viewer: a local file, or live video from a paired host (--connect).
// Live, the pointer and wheel over the picture go to the host; on Windows the keyboard
// goes through the native hook (hook.rs), and viewer controls move to Ctrl+Alt+Shift.
// Input is never logged.
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

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
let info = { codec: '', decode: '', decoded: 0, total: 0, ended: false, error: null, live: false, host: null, fps: 0, notice: null, hook: false,
  keyboard: null, drops: 0, decodeErrors: 0, waiting: false, lastKeyAt: null, jitterMs: null, queue: 0, keyTiming: null };
const statusText = document.createElement('span');
statusEl.append(statusText);

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
  const keys = info.hook
    ? 'Ctrl+Alt+Shift + F fullscreen  N native  arrows pan  R reset  H hide'
    : 'N native  arrows pan  R reset  F fullscreen  H hide';
  if (info.live) {
    const state = info.error ? `error: ${info.error}` : current ? `live ${info.fps} fps` : 'connecting';
    const notice = info.notice ? ` | ${info.notice}` : '';
    const keyAge = info.lastKeyAt === null ? '-' : `${Math.round((performance.now() - info.lastKeyAt) / 1000)}s`;
    const jitter = info.jitterMs === null ? '-' : `${info.jitterMs.toFixed(1)} ms`;
    const video = `drops ${info.drops}, decode errors ${info.decodeErrors}, keyframe ${keyAge} ago${info.waiting ? ', waiting for keyframe' : ''}`
      + `, jitter ${jitter}, queue ${info.queue}`;
    const keyboard = info.keyboard ? ` | ${info.keyboard}` : '';
    const timing = info.keyTiming ? ` | ${info.keyTiming}` : '';
    statusText.textContent = `${info.host ?? ''} ${info.codec} ${info.decode} | ${info.decoded} frames | ${view.mode} | ${state} | ${video}${keyboard}${timing}${notice} | ${keys} | `;
  } else {
    const state = info.error ? `error: ${info.error}` : info.ended ? 'ended (holding last frame)' : 'playing';
    statusText.textContent = `${info.codec} ${info.decode} | ${info.decoded}/${info.total} frames | ${view.mode} | ${state} | ${keys}`;
  }
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

// Returns false for a name that isn't a viewer control.
function shortcut(name) {
  if (name === 'native') { view.mode = view.mode === 'fit' ? 'native' : 'fit'; view.panX = view.panY = 0; }
  else if (name === 'reset') { view.panX = view.panY = 0; }
  else if (name === 'status') { view.showStatus = !view.showStatus; }
  else if (name === 'fullscreen') { setFullscreen(!view.fullscreen); }
  else if (name === 'exit_fullscreen') { if (view.fullscreen) setFullscreen(false); }
  else if (view.mode === 'native' && name.startsWith('pan_')) {
    if (name === 'pan_left') view.panX -= PAN_STEP;
    if (name === 'pan_right') view.panX += PAN_STEP;
    if (name === 'pan_up') view.panY -= PAN_STEP;
    if (name === 'pan_down') view.panY += PAN_STEP;
  } else return false;
  draw();
  return true;
}

const KEY_SHORTCUTS = {
  n: 'native', r: 'reset', h: 'status', f: 'fullscreen', escape: 'exit_fullscreen',
  arrowleft: 'pan_left', arrowright: 'pan_right', arrowup: 'pan_up', arrowdown: 'pan_down',
};

window.addEventListener('keydown', e => {
  // With the hook, keys belong to the host; the few that reach the page (local combos
  // such as Alt+Tab) are left to Windows.
  if (info.hook) return;
  const name = KEY_SHORTCUTS[e.key.toLowerCase()];
  if (name && shortcut(name)) e.preventDefault();
});

window.addEventListener('resize', resizeCanvas);

// ---- live ----

const LIVE_CHECK_FRAMES = 300;

// [u8 flags][u64 LE host_us][u8 codec_len][codec][access unit]; see live.rs.
function parseLive(buf) {
  const v = new DataView(buf);
  const flags = v.getUint8(0), codecLen = v.getUint8(13);
  return {
    key: (flags & 1) === 1,
    hostUs: Number(v.getBigUint64(1, true)),
    // Key-down to now (in Rust's hands), for the first frame after a key; else null.
    keyMs: flags & 2 ? v.getUint32(9, true) / 1000 : null,
    codec: new TextDecoder().decode(new Uint8Array(buf, 14, codecLen)),
    data: new Uint8Array(buf, 14 + codecLen),
  };
}

async function live(launch) {
  const started = performance.now();
  const stats = {
    tool: 'nocomputer-viewer-live', host: launch.connect, decode: launch.decode, codec: null,
    hardwareAcceleration: ACCELERATION[launch.decode], received: 0, keyframes: 0, frames: 0,
    pageDropped: 0, width: null, height: null, maxHostGapMs: 0, decodeErrors: [], elapsedMs: null,
    userAgent: navigator.userAgent,
  };
  info.decode = ACCELERATION[launch.decode];
  info.live = true;
  if (!launch.check) startInput(launch);
  let decoder = null, codec = null, waitingForKey = true, lastHostUs = null;
  const shown = []; // performance.now() of recent outputs, for the frame rate
  // Arrival jitter: how far each arrival gap (this clock) strays from its capture gap
  // (the host's clock). Only same-clock differences are compared; worst in the last 2 s.
  const jitter = []; // [arrival ms, |gap difference| ms]
  const queueDepth = []; // [ms, decodeQueueSize before each decode]
  let lastArrival = null;
  // Key-down to the decoded picture, for the first frame after each key-down: Rust's
  // part (key read → frame handed over) plus the page's (frame received → decoded),
  // each on its own monotonic clock. The IPC hop between them (~3 ms) isn't counted.
  const keyed = new Map(); // host_us → { at: received, ms: Rust's part }
  const shownMs = [];
  const requestKeyframe = () => invoke('live_request_keyframe').catch(() => {});

  const makeDecoder = c => {
    const d = new VideoDecoder({
      output: frame => {
        stats.frames++;
        if (stats.width === null) { stats.width = frame.displayWidth; stats.height = frame.displayHeight; }
        const now = performance.now();
        const k = keyed.get(frame.timestamp);
        if (k) {
          keyed.delete(frame.timestamp);
          shownMs.push(k.ms + now - k.at);
          if (shownMs.length > 32) shownMs.shift();
          info.keyShown = { avg: shownMs.reduce((a, b) => a + b, 0) / shownMs.length, max: Math.max(...shownMs), n: (info.keyShown?.n ?? 0) + 1 };
        }
        shown.push(now);
        while (shown.length && now - shown[0] > 1000) shown.shift();
        if (launch.check) { frame.close(); return; }
        current?.close();
        current = frame;
        info.decoded = stats.frames;
        info.fps = shown.length;
        draw();
      },
      error: e => {
        // The decoder closes itself; restart at the next keyframe.
        stats.decodeErrors.push(`${e.name}: ${e.message}`);
        info.error = null;
        info.decodeErrors++;
        decoder = null;
        waitingForKey = info.waiting = true;
        requestKeyframe();
      },
    });
    d.configure(decoderConfig(c, launch.decode));
    return d;
  };

  for (;;) {
    if (launch.check && stats.frames >= LIVE_CHECK_FRAMES) break;
    let buf;
    try {
      buf = await invoke('live_next');
    } catch (e) {
      info.error = String(e);
      if (!launch.check) { draw(); return; }
      stats.decodeErrors.push(`connection: ${e}`);
      break;
    }
    const f = parseLive(buf);
    const arrival = performance.now();
    stats.received++;
    if (lastHostUs !== null && lastArrival !== null) {
      jitter.push([arrival, Math.abs((arrival - lastArrival) - (f.hostUs - lastHostUs) / 1000)]);
      while (arrival - jitter[0][0] > 2000) jitter.shift();
      info.jitterMs = Math.max(...jitter.map(j => j[1]));
    }
    lastArrival = arrival;
    if (lastHostUs !== null) stats.maxHostGapMs = Math.max(stats.maxHostGapMs, (f.hostUs - lastHostUs) / 1000);
    lastHostUs = f.hostUs;
    if (f.key) {
      stats.keyframes++;
      info.lastKeyAt = performance.now();
      if (!decoder || f.codec !== codec) {
        decoder?.close();
        codec = stats.codec = info.codec = f.codec;
        decoder = makeDecoder(codec);
        if (!info.host) invoke('live_info').then(i => { info.host = i.host_name; draw(); });
      }
      waitingForKey = false;
    }
    info.waiting = waitingForKey;
    if (waitingForKey || !decoder) continue;
    queueDepth.push([arrival, decoder.decodeQueueSize]);
    while (arrival - queueDepth[0][0] > 2000) queueDepth.shift();
    info.queue = Math.max(...queueDepth.map(q => q[1]));
    if (decoder.decodeQueueSize >= MAX_DECODE_QUEUE) {
      // Behind: skip to the next keyframe rather than build latency.
      stats.pageDropped++;
      info.drops++;
      waitingForKey = info.waiting = true;
      requestKeyframe();
      continue;
    }
    if (f.keyMs !== null) {
      if (keyed.size > 16) keyed.clear(); // outputs lost to a decoder error
      keyed.set(f.hostUs, { at: arrival, ms: f.keyMs });
    }
    decoder.decode(new EncodedVideoChunk({ type: f.key ? 'key' : 'delta', timestamp: f.hostUs, data: f.data }));
  }
  if (decoder) await decoder.flush().catch(() => {});
  const i = await invoke('live_info').catch(() => ({}));
  stats.hostName = i.host_name ?? null;
  stats.hostFingerprint = i.host_fingerprint ?? null;
  stats.viewerDropped = i.dropped ?? null;
  stats.elapsedMs = Math.round(performance.now() - started);
  const pass = stats.frames >= LIVE_CHECK_FRAMES && stats.decodeErrors.length === 0;
  await invoke('finish_check', { summary: stats, pass });
}

// ---- live input ----

const BUTTONS = ['left', 'middle', 'right', 'back', 'forward']; // by MouseEvent.button
const LINES_PER_PAGE = 10;

function startInput(launch) {
  info.hook = launch.keyboard_hook;
  let queue = [];
  let pendingMove = null;
  let sending = false;
  const held = new Set(); // buttons sent down
  const scrollRest = { x: 0, y: 0 }; // sub-pixel wheel remainders

  // One call in flight, so input reaches the host in order.
  const pump = async () => {
    if (sending || !queue.length) return;
    sending = true;
    const events = queue;
    queue = [];
    await invoke('live_input', { events }).catch(() => {});
    sending = false;
    pump();
  };
  const flushMove = () => {
    if (pendingMove) { queue.push(pendingMove); pendingMove = null; }
  };
  const send = event => { flushMove(); queue.push(event); pump(); };
  const frame = () => { if (pendingMove) { flushMove(); pump(); } requestAnimationFrame(frame); };
  requestAnimationFrame(frame);

  // Normalized position in the picture. Outside it (the letterbox, or beyond the window
  // under pointer capture) that is null, or the nearest edge with `clamp`, as while a
  // drag that started in the picture is held.
  const position = (e, clamp = false) => {
    if (!current) return null;
    const box = canvas.getBoundingClientRect(); // not offsetX: the event may target another element
    const scale = canvas.width / box.width;
    const r = contentRect(current.displayWidth, current.displayHeight);
    const px = (e.clientX - box.left) * scale - r.x, py = (e.clientY - box.top) * scale - r.y;
    if (!clamp && !(px >= 0 && px < r.w && py >= 0 && py < r.h)) return null;
    // The last CSS pixel of the picture starts one CSS pixel (`scale` canvas pixels) before
    // its far edge: map it to 65535, so the host's last row and column (the Dock, hot
    // corners) can be reached.
    const x = Math.min(1, Math.max(0, px / Math.max(1, r.w - scale)));
    const y = Math.min(1, Math.max(0, py / Math.max(1, r.h - scale)));
    return { kind: 'pointer_move', x: Math.round(x * 65535), y: Math.round(y * 65535) };
  };
  const releaseHeld = () => {
    for (const button of held) send({ kind: 'pointer_button', button, down: false });
    held.clear();
  };

  canvas.addEventListener('pointermove', e => {
    const p = position(e, held.size > 0);
    if (p) pendingMove = p;
  });
  canvas.addEventListener('pointerdown', e => canvas.setPointerCapture(e.pointerId));
  // Capture can end without a mouseup (another window took it, say): let go on the host.
  canvas.addEventListener('lostpointercapture', releaseHeld);
  canvas.addEventListener('mousedown', e => {
    e.preventDefault(); // no middle-click autoscroll or focus changes
    const p = position(e);
    const button = BUTTONS[e.button];
    if (!p || !button) return; // a press in the letterbox stays local, and so does its release
    pendingMove = p;
    held.add(button);
    send({ kind: 'pointer_button', button, down: true });
  });
  window.addEventListener('mouseup', e => {
    e.preventDefault(); // back/forward never navigate the page
    const button = BUTTONS[e.button];
    if (!held.delete(button)) return;
    // Always sent, at the nearest picture edge if released outside it.
    const p = position(e, true);
    if (p) pendingMove = p;
    send({ kind: 'pointer_button', button, down: false });
  });
  canvas.addEventListener('auxclick', e => e.preventDefault());
  canvas.addEventListener('contextmenu', e => e.preventDefault());
  canvas.addEventListener('wheel', e => {
    e.preventDefault(); // and no page zoom
    if (!position(e)) return;
    let unit = 'pixel', dx = e.deltaX, dy = e.deltaY;
    if (e.deltaMode === WheelEvent.DOM_DELTA_LINE) unit = 'line';
    if (e.deltaMode === WheelEvent.DOM_DELTA_PAGE) { unit = 'line'; dx *= LINES_PER_PAGE; dy *= LINES_PER_PAGE; }
    if (unit === 'pixel') {
      dx += scrollRest.x; dy += scrollRest.y;
      scrollRest.x = dx - Math.trunc(dx); scrollRest.y = dy - Math.trunc(dy);
    }
    dx = Math.trunc(dx); dy = Math.trunc(dy);
    if (dx || dy) send({ kind: 'scroll', dx, dy, unit });
  }, { passive: false });
  // The window going to the background releases everything on the host (hook.rs).
  window.addEventListener('blur', () => held.clear());

  if (info.hook) {
    listen('viewer-shortcut', e => shortcut(e.payload));
    statusEl.classList.add('interactive');
    const label = document.createElement('label');
    label.title = 'Which key sends Command to the host. Win+L and Ctrl+Alt+Del always stay with Windows.';
    label.append('Command: ');
    const select = document.createElement('select');
    for (const [value, text] of [['win', 'Windows key'], ['right_alt', 'Right Alt']]) select.add(new Option(text, value));
    let saved = null;
    try { saved = localStorage.getItem('commandKey'); } catch {}
    if (saved === 'win' || saved === 'right_alt') select.value = saved;
    const apply = () => {
      invoke('live_settings', { commandKey: select.value }).catch(() => {});
      try { localStorage.setItem('commandKey', select.value); } catch {}
      select.blur(); // keys belong to the host, not the menu
    };
    select.addEventListener('change', apply);
    apply();
    label.append(select);
    statusEl.append(label);
  }
  setInterval(async () => {
    const i = await invoke('live_info').catch(() => null);
    if (!i) return;
    info.notice = i.notice;
    if (info.hook) info.keyboard = i.keyboard_active ? `keys on (${i.keys_sent} sent)` : 'keys off (window not in front)';
    const ms = (avg, max) => `${avg.toFixed(0)}/${max.toFixed(0)}`;
    if (i.key_frame?.n) {
      const shown = info.keyShown ? `, shown ${ms(info.keyShown.avg, info.keyShown.max)}` : '';
      info.keyTiming = `key→frame ${ms(i.key_frame.avg_ms, i.key_frame.max_ms)}${shown} ms avg/max (${i.key_frame.n}), send max ${i.key_send.max_ms.toFixed(1)} ms`;
    }
    draw();
  }, 1000);
}

(async () => {
  resizeCanvas();
  const launch = await invoke('launch_options');
  if (launch.connect) await live(launch);
  else if (launch.check) await runCheck(launch);
  else await play(launch);
})();
