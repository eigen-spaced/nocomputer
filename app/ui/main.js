// WebCodecs viewer: a local file, or live video from a paired host, reached through
// the start screen (saved hosts, pairing, login; see connect.rs).
// Live, the pointer and wheel over the picture go to the host; on Windows the keyboard
// goes through the native hook (hook.rs), and viewer controls move to Ctrl+Alt+Shift.
// Input is never logged.
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const ACCELERATION = { 'default': 'no-preference', 'require-hardware': 'prefer-hardware', 'software': 'prefer-software' };
const FRAME_US = 33_333;
const MAX_DECODE_QUEUE = 4;
const PAN_STEP = 64;
// A frame within this many device pixels of the canvas, in either direction, is drawn
// 1:1 in fit mode: the host streams at the canvas size, and its even-size and aspect
// rounding must not turn into a blurring 1.002x scale.
const SNAP_PX = 4;
// Live: ask the host for this in both fields for its native size (VIEW_SIZE_NATIVE).
const VIEW_SIZE_NATIVE = 65535;
const VIEW_SIZE_DEBOUNCE_MS = 200;

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

// The canvas in device pixels, as the browser laid it out. clientWidth times
// devicePixelRatio can be a pixel off at 125% scaling, and a 1:1 picture needs it exact.
let deviceSize = null;

function resizeCanvas() {
  const dpr = window.devicePixelRatio || 1;
  [canvas.width, canvas.height] = deviceSize ?? [Math.round(canvas.clientWidth * dpr), Math.round(canvas.clientHeight * dpr)];
  draw();
  reportViewSize();
}

const resizeObserver = new ResizeObserver(([entry]) => {
  const s = entry.devicePixelContentBoxSize?.[0];
  deviceSize = s && s.inlineSize > 0 && s.blockSize > 0 ? [s.inlineSize, s.blockSize] : null;
  resizeCanvas();
});
try { resizeObserver.observe(canvas, { box: 'device-pixel-content-box' }); } catch { resizeObserver.observe(canvas); }

// Live: tells the host the size to stream, so the picture is scaled once, on the host,
// and fit mode shows it 1:1: the canvas, or the host's native size in native mode.
let viewSizeOn = false, viewSizeTimer = null, sentViewSize = null;
function reportViewSize() {
  if (!viewSizeOn) return;
  clearTimeout(viewSizeTimer);
  viewSizeTimer = setTimeout(() => {
    const native = view.mode === 'native';
    const width = native ? VIEW_SIZE_NATIVE : Math.min(canvas.width, VIEW_SIZE_NATIVE - 1);
    const height = native ? VIEW_SIZE_NATIVE : Math.min(canvas.height, VIEW_SIZE_NATIVE - 1);
    if (width < 1 || height < 1 || sentViewSize === `${width}x${height}`) return;
    sentViewSize = `${width}x${height}`;
    invoke('live_view_size', { width, height }).catch(() => {});
  }, sentViewSize === null ? 0 : VIEW_SIZE_DEBOUNCE_MS);
}

// Destination rectangle in canvas (device) pixels.
function contentRect(vw, vh) {
  const cw = canvas.width, ch = canvas.height;
  if (view.mode === 'fit') {
    if (Math.abs(cw - vw) <= SNAP_PX || Math.abs(ch - vh) <= SNAP_PX) {
      if (vw <= cw + SNAP_PX && vh <= ch + SNAP_PX) return { x: Math.floor((cw - vw) / 2), y: Math.floor((ch - vh) / 2), w: vw, h: vh };
    }
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
  let size = '';
  if (current) {
    const r = contentRect(current.displayWidth, current.displayHeight);
    const scaled = r.w !== current.displayWidth || r.h !== current.displayHeight;
    ctx.imageSmoothingEnabled = scaled;
    ctx.imageSmoothingQuality = 'high';
    ctx.drawImage(current, r.x, r.y, r.w, r.h);
    size = ` ${current.displayWidth}x${current.displayHeight} ${scaled ? 'scaled' : '1:1'} in ${canvas.width}x${canvas.height}`;
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
    const clipboard = info.clipboard ? ` | ${info.clipboard}` : '';
    statusText.textContent = `${info.host ?? ''} ${info.codec} ${info.decode} | ${info.decoded} frames | ${view.mode}${size} | ${state} | ${video}${keyboard}${timing}${clipboard}${notice} | ${keys} | `;
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
  if (name === 'native') { view.mode = view.mode === 'fit' ? 'native' : 'fit'; view.panX = view.panY = 0; reportViewSize(); }
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
  if (document.body.classList.contains('home')) return; // typing into the start screen
  // With the hook, keys belong to the host. The ones that reach the page (local combos,
  // or typing with the pointer off the window) mustn't work the browser: no reload,
  // print or find. Windows' own shortcuts never come here, and Alt+F4 still closes.
  if (info.hook) {
    if (!(e.altKey && e.key === 'F4')) e.preventDefault();
    return;
  }
  const name = KEY_SHORTCUTS[e.key.toLowerCase()];
  if (name && shortcut(name)) e.preventDefault();
});

// ---- live ----

const LIVE_CHECK_FRAMES = 300;

// [u8 flags][u64 LE host_us][u8 codec_len][codec][access unit]; see live.rs.
function parseLive(buf) {
  const v = new DataView(buf);
  const flags = v.getUint8(0), codecLen = v.getUint8(13);
  return {
    key: (flags & 1) === 1,
    // The SPS changed (a new size, say): start a fresh decoder.
    newStream: (flags & 4) === 4,
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
  if (!launch.check) {
    viewSizeOn = true;
    reportViewSize();
    startInput(launch);
    await clipboardSwitch();
    disconnectButton();
  }
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
      if (!launch.check) return toStart();
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
      if (!decoder || f.codec !== codec || f.newStream) {
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
    if (info.hook) info.keyboard = i.keyboard_active ? `keys on (${i.keys_sent} sent)` : `keys off (${i.keyboard_off ?? 'unknown'})`;
    const c = i.clipboard;
    if (c) {
      const counts = `${c.sent} sent, ${c.received} received, ${c.skipped} skipped${c.failed ? `, ${c.failed} failed` : ''}`;
      info.clipboard = c.on ? `clipboard ${counts}` : `clipboard off${c.error ? ` (${c.error})` : ''}`;
    }
    const ms = (avg, max) => `${avg.toFixed(0)}/${max.toFixed(0)}`;
    if (i.key_frame?.n) {
      const shown = info.keyShown ? `, shown ${ms(info.keyShown.avg, info.keyShown.max)}` : '';
      info.keyTiming = `key→frame ${ms(i.key_frame.avg_ms, i.key_frame.max_ms)}${shown} ms avg/max (${i.key_frame.n}), send max ${i.key_send.max_ms.toFixed(1)} ms`;
    }
    draw();
  }, 1000);
}

// Shares plain-text copies with the host; on unless switched off. Only counts are shown.
async function clipboardSwitch() {
  statusEl.classList.add('interactive');
  const label = document.createElement('label');
  label.title = 'Share copied text with the host, both ways. Text that apps mark private (password managers) is never sent.';
  const box = document.createElement('input');
  box.type = 'checkbox';
  let saved = null;
  try { saved = localStorage.getItem('clipboard'); } catch {}
  box.checked = saved !== 'off';
  const apply = () => {
    try { localStorage.setItem('clipboard', box.checked ? 'on' : 'off'); } catch {}
    box.blur(); // keys belong to the host
    return invoke('live_clipboard', { on: box.checked }).catch(() => {});
  };
  box.addEventListener('change', apply);
  label.append(box, 'Clipboard ');
  statusEl.append(label);
  await apply();
}

// Ends the session and goes back to the start screen, which shows why, if the host or
// the network ended it (live_ended).
async function toStart() {
  await invoke('live_disconnect').catch(() => {});
  // A fresh page for the start screen; #start keeps --connect from opening again.
  location.hash = 'start';
  location.reload();
}

function disconnectButton() {
  statusEl.classList.add('interactive');
  const button = document.createElement('button');
  button.textContent = 'Disconnect';
  button.addEventListener('click', () => {
    button.disabled = true;
    toStart();
  });
  statusEl.append(button);
}

// ---- start screen ----

const DEFAULT_PORT = ':47900';

function savedPref(key) {
  try { return localStorage.getItem(key); } catch { return null; }
}

function savePref(key, value) {
  try { localStorage.setItem(key, value); } catch {}
}

// Connect, pair, log in, then hand over to live(). Each step is one connect_* command;
// errors come back as { kind, message } in plain words.
async function home(launch) {
  const $ = id => document.getElementById(id);
  document.body.classList.add('home');
  // Settings that must hold from the session's first moment, set before any login.
  await invoke('live_clipboard', { on: savedPref('clipboard') !== 'off' }).catch(() => {});
  if (launch.keyboard_hook) {
    const commandKey = savedPref('commandKey') === 'right_alt' ? 'right_alt' : 'win';
    await invoke('live_settings', { commandKey }).catch(() => {});
  }
  const shortName = address => address.endsWith(DEFAULT_PORT) ? address.slice(0, -DEFAULT_PORT.length) : address;
  let loginStep = null; // the login on screen
  let retryTimer = null;
  let note = null; // shown with the next login form

  const showProblem = p => {
    const el = $('problem');
    el.textContent = '';
    if (!p) return;
    el.append(p.message ?? String(p));
    if (p.expected) {
      for (const [label, fp] of [['Paired: ', p.expected], ['Now: ', p.presented]]) {
        const line = document.createElement('span');
        line.className = 'fingerprint';
        line.textContent = label + fp;
        el.append(line);
      }
    }
  };
  const show = (view, problem = null) => {
    clearInterval(retryTimer);
    for (const v of document.querySelectorAll('#home .view')) v.classList.toggle('shown', v.id === view);
    showProblem(problem);
  };
  const busy = text => { $('busy-text').textContent = text; show('busy'); };

  const list = async (problem = null) => {
    loginStep = null;
    const ul = $('hosts');
    ul.textContent = '';
    let hosts = [];
    try { hosts = await invoke('connect_hosts'); } catch (e) { problem ??= e; }
    for (const h of hosts) {
      const li = document.createElement('li');
      const who = document.createElement('div');
      who.className = 'who';
      const name = document.createElement('div');
      name.className = 'name';
      name.textContent = h.name;
      const address = document.createElement('div');
      address.className = 'address';
      address.textContent = shortName(h.address);
      who.append(name, address);
      const connect = document.createElement('button');
      connect.textContent = 'Connect';
      connect.addEventListener('click', () => open(h.address));
      const forget = document.createElement('button');
      forget.className = 'secondary';
      forget.textContent = 'Forget';
      let armed = null;
      forget.addEventListener('click', async () => {
        if (!armed) {
          forget.textContent = 'Really forget?';
          armed = setTimeout(() => { forget.textContent = 'Forget'; armed = null; }, 4000);
          return;
        }
        clearTimeout(armed);
        const problem = await invoke('connect_forget', { address: h.address }).then(() => null, e => e);
        list(problem);
      });
      li.append(who, connect, forget);
      ul.append(li);
    }
    show('list', problem);
    if (!hosts.length) $('address').focus();
  };

  const step = s => {
    if (s.step === 'pair') pair(s);
    else if (s.step === 'login') login(s);
    else if (s.step === 'live') { document.body.classList.remove('home'); resizeCanvas(); live(launch); }
    else list();
  };
  // Only the latest command's answer counts; Cancel, or a newer command, drops the rest.
  let latest = 0;
  const run = (promise, onError = list) => {
    const id = ++latest;
    return promise.then(s => { if (id === latest) step(s); }, e => { if (id === latest) onError(e); });
  };
  const open = address => {
    busy(`Connecting to ${shortName(address)}…`);
    run(invoke('connect_open', { address }));
  };

  const pair = s => {
    $('pair-title').textContent = `Pair with ${s.host_name}`;
    $('pair-text').textContent = s.already_paired
      ? `${s.host_name} already has this PC paired, but this PC hasn't saved it. Check that the host's fingerprint below matches the one its app shows, then continue.`
      : `This PC hasn't connected to ${shortName(s.address)} before. Check that the host shows these same two fingerprints before asking it to approve.`;
    $('pair-host').textContent = s.host_fingerprint;
    $('pair-this').textContent = s.this_fingerprint;
    $('pair-go').textContent = s.already_paired ? 'Continue' : 'Ask the host to approve';
    $('pair-go').onclick = () => {
      busy(s.already_paired ? 'Connecting…' : `Waiting for approval on ${s.host_name}. Approve this PC there within 60 seconds.`);
      run(invoke('connect_pair'));
    };
    show('pair');
  };

  const login = s => {
    loginStep = s;
    $('login-title').textContent = `Log in to ${s.host_name}`;
    const username = $('username');
    if (!username.value) username.value = savedPref(`user:${s.address}`) ?? '';
    $('password').value = '';
    show('login', s.failed ? { message: 'Wrong username or password.' } : note);
    note = null;
    const go = $('login-go');
    let wait = Math.ceil((s.retry_after_ms ?? 0) / 1000);
    const tick = () => {
      go.disabled = wait > 0;
      go.textContent = wait > 0 ? `Try again in ${wait} s` : 'Log in';
      if (wait-- <= 0) clearInterval(retryTimer);
    };
    tick();
    if (wait >= 0) retryTimer = setInterval(tick, 1000);
    (username.value ? $('password') : username).focus();
  };

  $('login-form').addEventListener('submit', e => {
    e.preventDefault();
    const s = loginStep;
    if (!s || $('login-go').disabled) return;
    const username = $('username').value.trim();
    const password = $('password').value;
    $('password').value = ''; // the page keeps no copy
    savePref(`user:${s.address}`, username); // not secret; the password never is saved
    busy('Logging in…');
    run(invoke('connect_login', { username, password }), e => {
      if (e.kind === 'bad_input') { login({ ...s, failed: false, retry_after_ms: 0 }); showProblem(e); }
      else if (e.kind === 'disconnected') {
        // The host gives each connection 2 minutes to log in; or the network dropped.
        note = { message: 'The connection closed before the login finished. Enter the password again.' };
        open(s.address);
      } else list(e);
    });
  });
  $('open-form').addEventListener('submit', e => {
    e.preventDefault();
    const address = $('address').value.trim();
    if (address) open(address);
  });
  for (const b of document.querySelectorAll('#home .cancel')) {
    b.addEventListener('click', async () => {
      latest++;
      await invoke('connect_cancel').catch(() => {});
      list();
    });
  }

  if (launch.connect && location.hash !== '#start') open(launch.connect);
  else list(await invoke('live_ended').catch(() => null));
}

(async () => {
  resizeCanvas();
  const launch = await invoke('launch_options');
  if (launch.live) await home(launch);
  else if (launch.check) await runCheck(launch);
  else await play(launch);
})();
