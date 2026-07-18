"use strict";

// ---------------- helpers ----------------
async function getJSON(url) {
  const r = await fetch(url);
  const j = await r.json();
  if (!r.ok) throw new Error(j.error || "request failed");
  return j;
}
async function postJSON(url, body) {
  const r = await fetch(url, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  const j = await r.json();
  if (!r.ok) throw new Error(j.error || "request failed");
  return j;
}
function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  return e;
}
function fmtCount(n) {
  if (n >= 1e6) return (n / 1e6).toFixed(1) + "M";
  if (n >= 1e3) return (n / 1e3).toFixed(1) + "k";
  return Math.round(n).toString();
}
function fmtBytes(b) {
  if (b >= 1 << 30) return (b / (1 << 30)).toFixed(1) + " GB";
  if (b >= 1 << 20) return (b / (1 << 20)).toFixed(1) + " MB";
  if (b >= 1 << 10) return (b / (1 << 10)).toFixed(1) + " KB";
  return b + " B";
}
function fmtTtl(ttl) {
  if (ttl === -1) return "no ttl";
  if (ttl === -2) return "gone";
  if (ttl < 60) return ttl + "s";
  if (ttl < 3600) return Math.round(ttl / 60) + "m";
  if (ttl < 86400) return Math.round(ttl / 3600) + "h";
  return Math.round(ttl / 86400) + "d";
}

const state = { info: null, stream: null, peakOps: 0 };

// ---------------- theme ----------------
const THEME_KEY = "inmem-theme";
function cssVar(name) { return getComputedStyle(document.documentElement).getPropertyValue(name).trim(); }
function applyTheme(t) {
  document.documentElement.dataset.theme = t;
  document.querySelectorAll("[data-theme-set]").forEach((b) => b.classList.toggle("active", b.dataset.themeSet === t));
  redrawCharts();
}
function initTheme() {
  applyTheme(localStorage.getItem(THEME_KEY) || "dark");
  document.querySelectorAll("[data-theme-set]").forEach((b) =>
    b.addEventListener("click", () => { localStorage.setItem(THEME_KEY, b.dataset.themeSet); applyTheme(b.dataset.themeSet); }));
}
function redrawCharts() {
  const c1 = cssVar("--chart-1"), c2 = cssVar("--chart-2"), c3 = cssVar("--chart-3");
  drawChart("chart-ops", opsSeries, c1);
  drawChart("chart-hit", hitSeries, c2, 0, 100);
  drawChart("chart-ping", pingSeries, c3);
  drawChart("chart-mon", opsSeries, c1);
}

// ---------------- navigation ----------------
function goTo(tab) {
  document.querySelectorAll(".nav-item").forEach((n) => n.classList.toggle("active", n.dataset.tab === tab));
  document.querySelectorAll(".screen").forEach((s) => s.classList.toggle("active", s.id === "screen-" + tab));
  if (tab === "keys") loadKeys();
  if (tab === "console") document.getElementById("console-in").focus();
}
document.querySelectorAll(".nav-item").forEach((n) => n.addEventListener("click", () => goTo(n.dataset.tab)));
document.querySelectorAll("[data-goto]").forEach((b) => b.addEventListener("click", () => goTo(b.dataset.goto)));

// ---------------- live metrics (WebSocket) ----------------
const opsSeries = [], hitSeries = [], pingSeries = [];
const MAX_POINTS = 60;

function connectStream() {
  const proto = location.protocol === "https:" ? "wss" : "ws";
  const ws = new WebSocket(`${proto}://${location.host}/api/stream`);
  ws.onmessage = (ev) => {
    const m = JSON.parse(ev.data);
    state.stream = m;
    state.peakOps = Math.max(state.peakOps, m.ops_per_sec);
    setConn(m.online, m.ping_ms);
    push(opsSeries, m.ops_per_sec);
    push(hitSeries, m.hit_ratio);
    push(pingSeries, m.ping_ms);
    renderTiles();
    renderStrip();
    renderMonitor();
    document.getElementById("nav-keys").textContent = fmtCount(m.dbsize);
    document.getElementById("ch-ops-v").textContent = fmtCount(m.ops_per_sec);
    document.getElementById("ch-hit-v").textContent = m.hit_ratio.toFixed(1) + "%";
    document.getElementById("ch-ping-v").textContent = m.ping_ms.toFixed(2) + "ms";
    redrawCharts();
  };
  ws.onclose = () => { setConn(false, null); setTimeout(connectStream, 2000); };
  ws.onerror = () => ws.close();
}
function push(a, v) { a.push(v); if (a.length > MAX_POINTS) a.shift(); }
function setConn(online, ping) {
  document.getElementById("conn-dot").className = "dot " + (online ? "online" : "offline");
  document.getElementById("conn-text").textContent = online ? "Connected" : "Offline";
  document.getElementById("conn-ping").textContent = ping == null ? "— ms" : ping.toFixed(2) + " ms";
}

// ---------------- canvas sparkline (gradient fill) ----------------
function drawChart(id, data, color, fixedMin, fixedMax) {
  const cv = document.getElementById(id);
  if (!cv) return;
  const dpr = window.devicePixelRatio || 1;
  const w = cv.clientWidth, h = cv.clientHeight || 74;
  cv.width = w * dpr; cv.height = h * dpr;
  const ctx = cv.getContext("2d");
  ctx.scale(dpr, dpr);
  ctx.clearRect(0, 0, w, h);
  if (data.length < 2) return;
  const pad = 5;
  const min = fixedMin != null ? fixedMin : Math.min(...data);
  const max = fixedMax != null ? fixedMax : Math.max(...data);
  const range = max - min || 1;
  const x = (i) => pad + (i / (MAX_POINTS - 1)) * (w - 2 * pad);
  const y = (v) => h - pad - ((v - min) / range) * (h - 2 * pad);
  const grad = ctx.createLinearGradient(0, 0, 0, h);
  grad.addColorStop(0, color + "55");
  grad.addColorStop(1, color + "00");
  ctx.beginPath();
  ctx.moveTo(x(0), y(data[0]));
  data.forEach((v, i) => ctx.lineTo(x(i), y(v)));
  ctx.lineTo(x(data.length - 1), h - pad);
  ctx.lineTo(x(0), h - pad);
  ctx.closePath();
  ctx.fillStyle = grad;
  ctx.fill();
  ctx.beginPath();
  ctx.moveTo(x(0), y(data[0]));
  data.forEach((v, i) => ctx.lineTo(x(i), y(v)));
  ctx.strokeStyle = color;
  ctx.lineWidth = 1.8;
  ctx.stroke();
}

// ---------------- Overview ----------------
function flat(sections) {
  const m = {};
  Object.values(sections || {}).forEach((s) => Object.assign(m, s));
  return m;
}
async function loadInfo() {
  try {
    const info = await getJSON("/api/info");
    state.info = info;
    const s = flat(info.sections);
    if (s.inmem_version) document.getElementById("brand-sub").textContent = "v" + s.inmem_version + " · " + (s.role || "primary");
    document.getElementById("ov-sub").textContent = "127.0.0.1:6380 · " + info.dbsize.toLocaleString() + " keys";
    renderTiles();
    renderInfoGrid(info.sections);
    renderMonitor();
  } catch (e) {
    document.getElementById("tiles").innerHTML =
      `<div class="tile"><div class="t-label">error</div><div class="t-value" style="font-size:15px">${e.message}</div></div>`;
  }
}
function tile(label, value, unit, subHtml) {
  return `<div class="tile"><div class="t-label">${label}</div>` +
    `<div class="t-value">${value}${unit ? ' <small>' + unit + '</small>' : ''}</div>` +
    (subHtml || "") + `</div>`;
}
function renderTiles() {
  const live = state.stream, info = state.info;
  if (!live && !info) return;
  const s = flat(info && info.sections);
  const keys = live ? live.dbsize : info ? info.dbsize : 0;
  const used = live ? live.used_memory : parseInt(s.used_memory || "0", 10);
  const maxm = live ? live.maxmemory : parseInt(s.maxmemory || "0", 10);
  const hit = live ? live.hit_ratio : 0;
  const ops = live ? live.ops_per_sec : 0;
  const evicted = live ? live.evicted : 0;

  let memValue, memSub;
  if (maxm > 0) {
    memValue = fmtBytes(used).replace(/ (GB|MB|KB|B)/, "");
    memSub = `<div class="membar"><i style="width:${Math.min(100, used / maxm * 100).toFixed(0)}%"></i></div>`;
    memValue = `${fmtBytes(used)}<small> / ${fmtBytes(maxm)}</small>`;
  } else {
    memValue = fmtBytes(used);
    memSub = `<div class="t-sub muted">unbounded</div>`;
  }
  const html =
    tile("Keys", keys.toLocaleString(), "",
      `<div class="t-sub ${evicted ? 'muted' : 'accent-green'} mono">${evicted ? evicted.toLocaleString() + ' evicted' : '0 evicted'}</div>`) +
    tile("Throughput", fmtCount(ops), "ops/s",
      `<div class="t-sub muted mono">peak ${fmtCount(state.peakOps)}</div>`) +
    tile("Hit ratio", hit.toFixed(1), "%",
      `<div class="t-sub muted mono">${(live ? live.hits : 0).toLocaleString()} hits</div>`) +
    `<div class="tile"><div class="t-label">Memory</div><div class="t-value">${memValue}</div>${maxm > 0 ? memSub : memSub}</div>`;
  document.getElementById("tiles").innerHTML = html;
}
function renderInfoGrid(sections) {
  const box = document.getElementById("info-grid");
  box.innerHTML = "";
  const order = ["Server", "Memory", "Keyspace", "Stats", "Clients", "Replication"];
  const names = Object.keys(sections).sort((a, b) => order.indexOf(a) - order.indexOf(b));
  for (const name of names) {
    box.appendChild(el("div", "section", name));
    for (const [k, v] of Object.entries(sections[name])) {
      const row = el("div", "row");
      row.appendChild(el("span", "k", k));
      row.appendChild(el("span", "v", v));
      box.appendChild(row);
    }
  }
}

// ---------------- Console metric strip ----------------
function strip(label, value, cls) {
  return `<div class="m"><div class="m-label">${label}</div><div class="m-value ${cls || ''}">${value}</div></div>`;
}
function renderStrip() {
  const m = state.stream;
  if (!m) return;
  document.getElementById("metric-strip").innerHTML =
    strip("Keys", m.dbsize.toLocaleString()) +
    strip("Ops/sec", fmtCount(m.ops_per_sec), "accent-blue") +
    strip("Hit ratio", m.hit_ratio.toFixed(1) + "%", "accent-purple") +
    strip("Ping", m.ping_ms.toFixed(2) + "ms", "accent-green") +
    strip("Memory", fmtBytes(m.used_memory));
}

// ---------------- Monitor ----------------
function stat(n, label, green) {
  return `<div class="stat"><div class="n ${green ? 'green' : ''} mono">${n}</div><div class="l">${label}</div></div>`;
}
function renderMonitor() {
  const m = state.stream, info = state.info;
  if (m) {
    document.getElementById("mon-ops").textContent = Math.round(m.ops_per_sec).toLocaleString();
    document.getElementById("mon-peak").textContent = fmtCount(state.peakOps);
    document.getElementById("mon-hit").textContent = m.hit_ratio.toFixed(1) + "%";
    document.getElementById("mon-ping").textContent = m.ping_ms.toFixed(2) + "ms";
    const maxm = m.maxmemory;
    const memPct = maxm > 0 ? Math.min(100, m.used_memory / maxm * 100).toFixed(0) + "%" : "—";
    document.getElementById("mon-stats").innerHTML =
      stat(m.dbsize.toLocaleString(), "keys") +
      stat(fmtBytes(m.used_memory), "used memory") +
      stat(maxm > 0 ? fmtBytes(maxm) : "∞", "maxmemory") +
      stat(m.total_commands.toLocaleString(), "commands");
    document.getElementById("mon-evict").innerHTML =
      stat(m.evicted.toLocaleString(), "evicted keys") +
      stat(memPct, "mem used", true) +
      stat(m.hits.toLocaleString(), "keyspace hits");
  }
  // Per-command mix is not tracked by the server; be honest instead of faking it.
  document.getElementById("mix-note").textContent = "not tracked by server";
  document.getElementById("cmd-mix").innerHTML =
    `<div class="muted" style="font-size:12.5px; line-height:1.6;">The server exposes aggregate stats (hits, misses, evictions, total commands) but not a per-command breakdown or slow log yet. Throughput and hit ratio above are computed live from <span class="mono">INFO</span> deltas.</div>`;
}

// ---------------- Keys ----------------
let allKeys = [], activeType = "all", activeKey = null;
async function loadKeys() {
  const match = document.getElementById("key-match").value || "*";
  const list = document.getElementById("keys-list");
  list.innerHTML = `<div class="muted" style="padding:12px">scanning…</div>`;
  try {
    const data = await getJSON("/api/keys?limit=500&match=" + encodeURIComponent(match));
    allKeys = data.keys;
    renderChips(data.total, data.truncated);
    renderKeyList();
  } catch (e) {
    list.innerHTML = `<div class="err" style="padding:12px; color:var(--red)">${e.message}</div>`;
  }
}
function renderChips(total, truncated) {
  const counts = {};
  allKeys.forEach((k) => (counts[k.type] = (counts[k.type] || 0) + 1));
  const types = ["string", "list", "hash", "set", "zset"].filter((t) => counts[t]);
  const box = document.getElementById("key-chips");
  box.innerHTML = "";
  const mk = (t, label, n) => {
    const c = el("span", "chip" + (activeType === t ? " active" : ""), `${label} ${n}`);
    c.addEventListener("click", () => { activeType = t; renderChips(total, truncated); renderKeyList(); });
    return c;
  };
  box.appendChild(mk("all", "All", allKeys.length));
  types.forEach((t) => box.appendChild(mk(t, t, counts[t])));
  document.getElementById("keys-count").textContent =
    `${allKeys.length} shown of ${total.toLocaleString()}` + (truncated ? " (truncated)" : "");
}
function renderKeyList() {
  const list = document.getElementById("keys-list");
  list.innerHTML = "";
  const shown = allKeys.filter((k) => activeType === "all" || k.type === activeType);
  if (!shown.length) { list.innerHTML = `<div class="muted" style="padding:12px">no keys match</div>`; return; }
  shown.forEach((k) => {
    const row = el("div", "key-row" + (activeKey === k.name ? " active" : ""));
    row.appendChild(el("span", "type-badge type-" + k.type, k.type));
    row.appendChild(el("span", "name", k.name));
    const ttl = el("span", "ttl" + (k.ttl >= 0 ? " live" : ""), fmtTtl(k.ttl));
    row.appendChild(ttl);
    row.addEventListener("click", () => { activeKey = k.name; renderKeyList(); loadValue(k.name); });
    list.appendChild(row);
  });
}
async function loadValue(key) {
  const box = document.getElementById("key-inspector");
  box.innerHTML = `<div class="muted" style="padding:24px">loading…</div>`;
  try {
    const d = await getJSON("/api/value?key=" + encodeURIComponent(key));
    box.innerHTML = "";
    const head = el("div", "insp-head");
    const left = el("div");
    const title = el("div", "insp-title");
    title.appendChild(el("span", "type-badge type-" + d.type, d.type));
    title.appendChild(el("h3", null, d.key));
    left.appendChild(title);
    left.appendChild(el("div", "insp-meta", valueMeta(d)));
    head.appendChild(left);
    const actions = el("div", "insp-actions");
    const copy = el("button", "btn", "Copy");
    copy.addEventListener("click", () => navigator.clipboard && navigator.clipboard.writeText(d.key));
    const del = el("button", "btn btn-danger", "Delete");
    del.addEventListener("click", () => deleteKey(d.key));
    actions.appendChild(copy); actions.appendChild(del);
    head.appendChild(actions);
    box.appendChild(head);
    box.appendChild(renderValue(d));
    box.appendChild(el("div", "insp-cmd", inspectCmd(d)));
  } catch (e) {
    box.innerHTML = `<div style="padding:24px; color:var(--red)">${e.message}</div>`;
  }
}
function valueMeta(d) {
  const ttl = d.ttl === -1 ? "no expiry" : fmtTtl(d.ttl);
  if (d.value == null) return ttl;
  if (d.type === "string") return `${d.value.length} bytes · ${ttl}`;
  if (Array.isArray(d.value)) return `${d.value.length} ${d.type === "hash" || d.type === "zset" ? "fields" : "items"} · ${ttl}`;
  return ttl;
}
function inspectCmd(d) {
  return ({ string: "GET", list: "LRANGE 0 -1", set: "SMEMBERS", hash: "HGETALL", zset: "ZRANGE 0 -1 WITHSCORES" }[d.type] || "TYPE")
    .replace(/^(\S+)/, "$1 " + d.key);
}
function renderValue(d) {
  if (d.value == null) return el("div", "muted", "(nil)");
  if (d.type === "string") {
    const card = el("div", "value-card");
    const pre = el("pre", "value-pre"); pre.textContent = d.value; card.appendChild(pre); return card;
  }
  const card = el("div", "value-card");
  const t = el("table");
  if (d.type === "list" || d.type === "set") {
    d.value.forEach((v, i) => {
      const tr = el("tr");
      tr.appendChild(el("td", "idx", d.type === "list" ? i : "•"));
      tr.appendChild(el("td", "val", v));
      t.appendChild(tr);
    });
  } else {
    d.value.forEach((p) => {
      const tr = el("tr");
      tr.appendChild(el("td", "field", p.field));
      tr.appendChild(el("td", "val", p.value));
      t.appendChild(tr);
    });
  }
  card.appendChild(t);
  return card;
}
async function deleteKey(key) {
  try {
    await postJSON("/api/command", { args: ["DEL", key] });
    activeKey = null;
    document.getElementById("key-inspector").innerHTML = `<div class="inspector-empty muted">Deleted <span class="mono">${key}</span>.</div>`;
    loadKeys();
  } catch (e) { /* surfaced by reload */ }
}
document.getElementById("key-scan").addEventListener("click", () => { activeType = "all"; loadKeys(); });
document.getElementById("key-refresh").addEventListener("click", () => { activeType = "all"; loadKeys(); });
document.getElementById("key-match").addEventListener("keydown", (e) => { if (e.key === "Enter") { activeType = "all"; loadKeys(); } });

// ---------------- Console ----------------
const out = document.getElementById("console-out");
const input = document.getElementById("console-in");
const history = [];
let histIdx = 0;

function print(text, cls) { out.appendChild(el("div", "line " + (cls || ""), text)); out.scrollTop = out.scrollHeight; }
function replyInline(r) {
  if (r.type === "bulk") return JSON.stringify(r.str);
  if (r.type === "int") return "(integer) " + r.int;
  if (r.type === "nil") return "(nil)";
  if (r.type === "status") return r.str;
  if (r.type === "error") return "(error) " + r.str;
  return JSON.stringify(r);
}
function renderReply(reply, depth) {
  const pad = "  ".repeat(depth || 0);
  switch (reply.type) {
    case "status": return print(pad + reply.str, "ok");
    case "error": return print(pad + "(error) " + reply.str, "err");
    case "int": return print(pad + "(integer) " + reply.int, "int");
    case "nil": return print(pad + "(nil)", "dim");
    case "bulk":
      // Multi-line bulk (e.g. INFO) prints as raw lines, redis-cli style; single line stays quoted.
      if (reply.str.indexOf("\n") >= 0) {
        reply.str.replace(/\r/g, "").replace(/\n$/, "").split("\n")
          .forEach((l) => print(pad + l, l.startsWith("#") ? "dim" : "str"));
        return;
      }
      return print(pad + JSON.stringify(reply.str), "str");
    case "array":
      if (!reply.items.length) return print(pad + "(empty array)", "dim");
      reply.items.forEach((it, i) => {
        if (it.type === "array") { print(pad + (i + 1) + ")", "dim"); renderReply(it, depth + 1); }
        else print(pad + (i + 1) + ") " + replyInline(it), it.type === "int" ? "int" : it.type === "error" ? "err" : "str");
      });
      return;
    default: return print(pad + JSON.stringify(reply));
  }
}
function tokenize(s) {
  const out = [], re = /"([^"]*)"|(\S+)/g; let m;
  while ((m = re.exec(s)) !== null) out.push(m[1] !== undefined ? m[1] : m[2]);
  return out;
}
async function runCommand(text) {
  const args = tokenize(text.trim());
  if (!args.length) return;
  print("> " + text, "cmd");
  history.push(text); histIdx = history.length;
  try { renderReply((await postJSON("/api/command", { args })).reply, 0); }
  catch (e) { print("(error) " + e.message, "err"); }
}
input.addEventListener("keydown", (e) => {
  if (e.key === "Enter") { const v = input.value; input.value = ""; if (v.trim()) runCommand(v); }
  else if (e.key === "ArrowUp") { if (histIdx > 0) { histIdx--; input.value = history[histIdx]; } e.preventDefault(); }
  else if (e.key === "ArrowDown") { if (histIdx < history.length - 1) { histIdx++; input.value = history[histIdx]; } else { histIdx = history.length; input.value = ""; } e.preventDefault(); }
});
document.querySelectorAll(".chip-cmd").forEach((c) => c.addEventListener("click", () => {
  const cmd = c.dataset.cmd;
  if (cmd === "FLUSHALL" && !confirm("Run FLUSHALL? This clears all keys.")) return;
  runCommand(cmd);
}));
print("inmem console — connected via the bridge. Type a command, or press ⌘K.", "dim");

// ---------------- Export INFO ----------------
document.getElementById("export-info").addEventListener("click", () => {
  const info = state.info; if (!info) return;
  let text = "";
  for (const [name, kv] of Object.entries(info.sections)) {
    text += "# " + name + "\r\n";
    for (const [k, v] of Object.entries(kv)) text += k + ":" + v + "\r\n";
  }
  const url = URL.createObjectURL(new Blob([text], { type: "text/plain" }));
  const a = el("a"); a.href = url; a.download = "inmem-info.txt"; a.click();
  URL.revokeObjectURL(url);
});

// ---------------- Command palette (⌘K) ----------------
const palette = document.getElementById("palette");
const palInput = document.getElementById("palette-in");
const PAL_CMDS = [
  { verb: "SET", color: "#6ea8fe", desc: "Set a string value with optional TTL" },
  { verb: "GET", color: "#6ea8fe", desc: "Read a string value" },
  { verb: "EXPIRE", color: "#b98cff", desc: "Set a key's time-to-live" },
  { verb: "SCAN", color: "#39d98a", desc: "Browse keys by pattern" },
  { verb: "INFO", color: "#39d98a", desc: "Server stats and configuration" },
];
const PAL_NAV = [
  { verb: "Overview", tab: "overview" }, { verb: "Keys", tab: "keys" },
  { verb: "Console", tab: "console" }, { verb: "Monitor", tab: "monitor" },
];
function openPalette() { palette.hidden = false; palInput.value = ""; renderPalette(""); palInput.focus(); }
function closePalette() { palette.hidden = true; }
function renderPalette(q) {
  q = q.trim().toLowerCase();
  const body = document.getElementById("palette-body");
  body.innerHTML = "";
  const cmds = PAL_CMDS.filter((c) => c.verb.toLowerCase().includes(q) || c.desc.toLowerCase().includes(q));
  const navs = PAL_NAV.filter((n) => n.verb.toLowerCase().includes(q));
  if (cmds.length) {
    body.appendChild(el("div", "pal-label", "Commands"));
    cmds.forEach((c, i) => {
      const it = el("div", "pal-item" + (i === 0 ? " sel" : ""));
      const v = el("span", "verb", c.verb); v.style.color = c.color;
      it.appendChild(v); it.appendChild(el("span", "desc", c.desc));
      if (i === 0) it.appendChild(el("span", "hint", "↵ to console"));
      it.addEventListener("click", () => palRunCmd(c.verb));
      body.appendChild(it);
    });
  }
  if (navs.length) {
    body.appendChild(el("div", "pal-label", "Go to"));
    navs.forEach((n) => {
      const it = el("div", "pal-item");
      it.appendChild(el("span", "desc", n.verb));
      it.addEventListener("click", () => { closePalette(); goTo(n.tab); });
      body.appendChild(it);
    });
  }
}
function palRunCmd(verb) {
  closePalette();
  goTo("console");
  input.value = verb + " ";
  input.focus();
}
palInput.addEventListener("input", () => renderPalette(palInput.value));
palInput.addEventListener("keydown", (e) => {
  if (e.key === "Escape") closePalette();
  else if (e.key === "Enter") {
    const q = palInput.value.trim();
    const navHit = PAL_NAV.find((n) => n.verb.toLowerCase() === q.toLowerCase());
    if (navHit) { closePalette(); goTo(navHit.tab); return; }
    if (q.includes(" ")) { closePalette(); goTo("console"); runCommand(q); return; }
    const first = PAL_CMDS.filter((c) => c.verb.toLowerCase().includes(q.toLowerCase()))[0];
    if (first) palRunCmd(first.verb);
  }
});
document.getElementById("search-btn").addEventListener("click", openPalette);
document.addEventListener("keydown", (e) => {
  if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") { e.preventDefault(); palette.hidden ? openPalette() : closePalette(); }
  else if (e.key === "Escape" && !palette.hidden) closePalette();
});
palette.addEventListener("click", (e) => { if (e.target === palette) closePalette(); });

// ---------------- boot ----------------
initTheme();
loadInfo();
connectStream();
setInterval(loadInfo, 10000);
