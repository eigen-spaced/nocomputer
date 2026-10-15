// Host panel page. Contract: docs/host-panel.md. The shell owns every state and
// decision; this page renders what it is sent and reports clicks. Text is only ever
// set through textContent: device names and log lines come from remote devices.
"use strict";

const VERSION = 2;
const LOG_LINES = 200;
const $ = (id) => document.getElementById(id);

let outdated = false;
let last = null; // the latest state, so a click the shell ignores is undone

function panelSend(action) {
  if (outdated) return;
  const message = { v: VERSION, ...action };
  const webkit = window.webkit && window.webkit.messageHandlers && window.webkit.messageHandlers.panel;
  if (webkit) webkit.postMessage(message);
  else if (window.__TAURI__) window.__TAURI__.core.invoke("panel_action", { action: message });
  else console.debug("panel action (no shell)", message); // opened as a plain file
}

// Leave a node alone when nothing changed, so a selection in it survives.
function setText(el, text) {
  if (el.textContent !== text) el.textContent = text;
}

function show(el, shown) {
  if (el.hidden === shown) el.hidden = !shown;
}

function str(value) {
  return typeof value === "string" ? value : "";
}

function renderPermissions(list) {
  const items = Array.isArray(list) ? list : [];
  show($("permissions"), items.length > 0);
  const ul = $("permission-list");
  // Rebuilt only when something in it changed.
  const key = JSON.stringify(items.map((p) => [str(p.id), str(p.label), !!p.granted, !!p.canRequest]));
  if (ul.dataset.key === key) return;
  ul.dataset.key = key;
  ul.replaceChildren(...items.map((p) => {
    const li = document.createElement("li");
    const label = document.createElement("span");
    label.textContent = `${str(p.label)}: ${p.granted ? "granted" : "not granted"}`;
    li.append(label);
    if (!p.granted && p.canRequest) {
      const button = document.createElement("button");
      button.type = "button";
      button.textContent = "Request";
      const id = str(p.id);
      button.addEventListener("click", () => panelSend({ type: "requestPermission", id }));
      li.append(button);
    }
    return li;
  }));
}

function renderDevices(list, on, connected) {
  const devices = on && Array.isArray(list) ? list.filter((d) => d && typeof d === "object" && str(d.fingerprint)) : [];
  show($("devices-off"), !on);
  show($("devices-none"), on && devices.length === 0);
  const ul = $("device-list");
  // Rebuilt only when something in it changed, so a selected fingerprint survives.
  const key = JSON.stringify([connected, devices.map((d) => [str(d.name), str(d.fingerprint), d.pairedAt, !!d.removed])]);
  if (ul.dataset.key === key) return;
  ul.dataset.key = key;
  ul.replaceChildren(...devices.map((d) => {
    const fingerprint = str(d.fingerprint);
    const li = document.createElement("li");
    const about = document.createElement("div");
    const name = document.createElement("p");
    const title = document.createElement("span");
    title.className = "name";
    title.textContent = str(d.name) || "unnamed device";
    name.append(title);
    const badges = [];
    if (d.removed) badges.push(["Removed", "badge removed"]);
    else if (fingerprint === connected) badges.push(["Connected", "badge connected"]);
    for (const [text, className] of badges) {
      const badge = document.createElement("span");
      badge.className = className;
      badge.textContent = text;
      name.append(" ", badge);
    }
    const fp = document.createElement("p");
    fp.className = "fingerprint";
    fp.textContent = fingerprint;
    about.append(name, fp);
    const when = typeof d.pairedAt === "number" && Number.isFinite(d.pairedAt) ? new Date(d.pairedAt * 1000) : null;
    if (when && !Number.isNaN(when.getTime())) {
      const paired = document.createElement("p");
      paired.className = "muted";
      paired.textContent = `Paired ${when.toLocaleDateString(undefined, { dateStyle: "medium" })}`;
      about.append(paired);
    }
    const button = document.createElement("button");
    button.type = "button";
    button.className = "secondary";
    // The shell asks for confirmation natively; the page only reports the click.
    if (d.removed) {
      button.textContent = "Allow pairing again";
      button.addEventListener("click", () => panelSend({ type: "allowPairingAgain", fingerprint }));
    } else {
      button.textContent = "Remove";
      button.classList.add("danger");
      button.addEventListener("click", () => panelSend({ type: "removeDevice", fingerprint }));
    }
    li.append(about, button);
    return li;
  }));
}

function render(state) {
  last = state;
  renderPermissions(state.permissions);

  const remote = state.remoteAccess || {};
  $("remote").checked = !!remote.on;
  $("remote").disabled = !!remote.busy;
  setText($("remote-error"), str(remote.error));
  show($("remote-error"), !!str(remote.error));
  $("clipboard").checked = !!(state.clipboard && state.clipboard.on);
  $("clipboard").disabled = false; // both switches stay disabled until the first state

  const host = state.host && typeof state.host === "object" ? state.host : null;
  show($("host"), !!host);
  show($("host-off"), !host);
  setText($("host-name"), host ? `"${str(host.name)}"` : "");
  setText($("host-fingerprint"), host ? str(host.fingerprint) : "");

  setText($("pairing-notice"), str(state.pairingNotice));
  show($("pairing-notice"), !!str(state.pairingNotice));

  const session = state.session && typeof state.session === "object" ? state.session : null;
  show($("session-box"), !!remote.on);
  show($("session"), !!session);
  show($("session-none"), !session);
  setText($("session-device"), session ? `"${str(session.device)}"` : "");
  setText($("session-fingerprint"), session ? str(session.deviceFingerprint) : "");

  renderDevices(state.devices, !!remote.on, session ? str(session.deviceFingerprint) : "");
}

function appendLog(line) {
  const log = $("log");
  const atEnd = log.scrollTop + log.clientHeight >= log.scrollHeight - 4;
  const lines = log.textContent ? log.textContent.split("\n") : [];
  lines.push(line);
  log.textContent = lines.slice(-LOG_LINES).join("\n");
  if (atEnd) log.scrollTop = log.scrollHeight;
}

window.panel = {
  receive(msg) {
    if (outdated || !msg || typeof msg !== "object") return;
    if (msg.v !== VERSION) {
      if (typeof msg.v === "number" && msg.v > VERSION) {
        outdated = true;
        $("outdated").hidden = false;
        for (const el of document.querySelectorAll("button, input")) el.disabled = true;
      }
      return;
    }
    if (msg.type === "state" && msg.state && typeof msg.state === "object") render(msg.state);
    else if (msg.type === "status") setText($("status"), str(msg.text));
    else if (msg.type === "log") appendLog(str(msg.line));
  },
};

// A switch shows the shell's state, not the click: put it back, and the next
// state from the shell moves it if the change was taken.
function onSwitch(id, type) {
  $(id).addEventListener("change", (event) => {
    const on = event.target.checked;
    if (last) render(last);
    panelSend({ type, on });
  });
}
onSwitch("remote", "setRemoteAccess");
onSwitch("clipboard", "setClipboard");
$("refresh").addEventListener("click", () => panelSend({ type: "refreshPermissions" }));

if (window.__TAURI__) window.__TAURI__.event.listen("panel", (event) => window.panel.receive(event.payload));
panelSend({ type: "ready" });
