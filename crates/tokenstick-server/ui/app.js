// TokenStick web UI v0. Plain JS, no build step.
// Talks to the management API (/api/v1/*) and the OpenAI API (/v1/*).
"use strict";

const $ = (id) => document.getElementById(id);
const HISTORY_POINTS = 300; // 5 min at one snapshot per second
const THROTTLE_C = 85;
const PD_BUDGET_W = 18;

const state = {
  history: [], // {temp, power}
  messages: [], // {role, content}
  abort: null,
  lastStats: null,
};

// ---------- helpers ----------

function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === "class") node.className = v;
    else node.setAttribute(k, v);
  }
  for (const c of children) node.append(c);
  return node;
}

const fmt = (v, digits = 1) => (v == null || Number.isNaN(v) ? "–" : Number(v).toFixed(digits));
const pct = (v, max) => `${Math.max(0, Math.min(100, ((v ?? 0) / max) * 100))}%`;
const timeOf = (ms) => new Date(ms).toLocaleTimeString([], { hour12: false });

async function copyText(text, button) {
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    const ta = el("textarea");
    ta.value = text;
    document.body.append(ta);
    ta.select();
    document.execCommand("copy");
    ta.remove();
  }
  const old = button.textContent;
  button.textContent = "Copied";
  setTimeout(() => (button.textContent = old), 1200);
}

// ---------- routing ----------

const ROUTES = ["dashboard", "playground", "server"];

function route() {
  const name = (location.hash.match(/^#\/(\w+)/) || [])[1];
  const current = ROUTES.includes(name) ? name : "dashboard";
  for (const r of ROUTES) $(`view-${r}`).hidden = r !== current;
  document.querySelectorAll("[data-route]").forEach((a) => {
    if (a.dataset.route === current) a.setAttribute("aria-current", "page");
    else a.removeAttribute("aria-current");
  });
  document.title = `TokenStick · ${current[0].toUpperCase()}${current.slice(1)}`;
  if (current === "playground") $("msg").focus();
}

// ---------- dashboard ----------

function renderDevices(devices) {
  const box = $("devices");
  box.replaceChildren();
  $("dev-count").textContent = `${devices.length} device${devices.length === 1 ? "" : "s"} connected`;
  if (!devices.length) {
    box.append(el("div", { class: "card empty" }, "No device found. Plug in a TokenStick or check the backend."));
    return;
  }
  for (const d of devices) {
    const meta = el("div", { class: "device-meta" });
    const add = (label, value, mono) => {
      const s = el("strong", mono ? { class: "mono" } : {}, value ?? "–");
      meta.append(el("span", {}, `${label} `, s));
    };
    add("Chip", d.chip);
    add("Memory", `${(d.memory_mib / 1024).toFixed(d.memory_mib % 1024 ? 1 : 0)} GiB`);
    add("Firmware", d.firmware, true);
    add("Backend", d.backend, true);
    if (d.telemetry.pd_contract) add("USB PD", d.telemetry.pd_contract, true);
    box.append(
      el("section", { class: "card device", "aria-label": `Device ${d.id}` },
        el("div", { class: "device-id" },
          el("span", { class: "dot", "aria-hidden": "true" }),
          el("span", { class: "device-name" }, d.id)),
        meta));
  }
}

function setTile(id, value, digits, barId, max) {
  $(id).textContent = fmt(value, digits);
  if (barId) $(barId).style.width = pct(value, max);
}

function renderTelemetry(devices, stats) {
  const t = devices[0]?.telemetry ?? {};
  setTile("t-temp", t.temperature_c, 1, "t-temp-bar", THROTTLE_C);
  setTile("t-power", t.power_w, 1, "t-power-bar", PD_BUDGET_W);
  setTile("t-npu", t.npu_load_pct, 0, "t-npu-bar", 100);
  const n = stats.active_requests;
  $("t-active").textContent = n === 0 ? "No active requests" : `${n} active request${n === 1 ? "" : "s"}`;

  $("m-temp").textContent = t.temperature_c == null ? "–" : `${fmt(t.temperature_c)} °C`;
  $("m-power").textContent = t.power_w == null ? "–" : `${fmt(t.power_w)} W`;
  $("m-npu").textContent = t.npu_load_pct == null ? "–" : `${fmt(t.npu_load_pct, 0)} %`;

  const last = stats.recent[0];
  if (last) {
    $("t-tps").textContent = fmt(last.tokens_per_s);
    $("t-ttft").textContent = `${fmt(last.ttft_ms, 0)} ms`;
    $("m-tps").textContent = fmt(last.tokens_per_s);
  }
}

// Until the 5 min buffer fills, stretch what we have (at least 60 s) across
// the chart instead of leaving it squeezed against the right edge.
const MIN_WINDOW = 60;
const chartWindow = () => Math.max(MIN_WINDOW, Math.min(HISTORY_POINTS, state.history.length));

function polyline(values, lo, hi) {
  const win = chartWindow();
  const n = win - 1;
  const offset = win - values.length;
  return values
    .map((v, i) => {
      if (v == null) return null;
      const x = ((offset + i) / n) * 520;
      const y = 160 - ((Math.max(lo, Math.min(hi, v)) - lo) / (hi - lo)) * 150 - 5;
      return `${x.toFixed(1)},${y.toFixed(1)}`;
    })
    .filter(Boolean)
    .join(" ");
}

function renderCharts(stats) {
  $("line-temp").setAttribute("points", polyline(state.history.map((h) => h.temp), 30, 90));
  $("line-power").setAttribute("points", polyline(state.history.map((h) => h.power), 0, 20));
  const win = chartWindow();
  $("thermal-window").textContent = win >= HISTORY_POINTS ? "last 5 min" : win < 120 ? `last ${win} s` : `last ${Math.round(win / 60)} min`;

  const recent = stats.recent.slice(0, 12).reverse();
  const g = $("tps-bars");
  g.replaceChildren();
  if (!recent.length) return;
  const max = Math.max(...recent.map((r) => r.tokens_per_s), 1);
  const slot = 520 / 12;
  recent.forEach((r, i) => {
    const h = Math.max(2, (r.tokens_per_s / max) * 150);
    const rect = document.createElementNS("http://www.w3.org/2000/svg", "rect");
    rect.setAttribute("x", (i * slot + 6).toFixed(1));
    rect.setAttribute("y", (160 - h).toFixed(1));
    rect.setAttribute("width", (slot - 12).toFixed(1));
    rect.setAttribute("height", h.toFixed(1));
    rect.setAttribute("rx", "3");
    const title = document.createElementNS("http://www.w3.org/2000/svg", "title");
    title.textContent = `${r.model}: ${fmt(r.tokens_per_s)} tok/s`;
    rect.append(title);
    g.append(rect);
  });
  $("tps-scale").textContent = `Tallest bar: ${fmt(max)} tok/s`;
}

function renderRecent(stats) {
  const body = $("recent");
  body.replaceChildren();
  $("recent-empty").hidden = stats.recent.length > 0;
  for (const r of stats.recent) {
    const td = (text, cls) => el("td", cls ? { class: cls } : {}, text);
    body.append(
      el("tr", {},
        td(timeOf(r.finished_at_ms)),
        td(r.model),
        td(`${r.client}${r.stream ? " · stream" : ""}`, "client"),
        td(String(r.completion_tokens), "r"),
        td(`${fmt(r.ttft_ms, 0)} ms`, "r"),
        td(fmt(r.tokens_per_s), "r"),
        td(r.finish_reason, `fin-${r.finish_reason}`)));
  }
}

function onSnapshot(snap) {
  const t = snap.devices[0]?.telemetry ?? {};
  state.history.push({ temp: t.temperature_c, power: t.power_w });
  if (state.history.length > HISTORY_POINTS) state.history.shift();
  renderDevices(snap.devices);
  renderTelemetry(snap.devices, snap.stats);
  renderCharts(snap.stats);
  renderRecent(snap.stats);
}

function connectEvents() {
  const conn = $("nav-conn");
  const es = new EventSource("/api/v1/events");
  es.addEventListener("snapshot", (e) => {
    conn.textContent = "live";
    conn.classList.add("live");
    onSnapshot(JSON.parse(e.data));
  });
  es.onerror = () => {
    conn.textContent = "reconnecting…";
    conn.classList.remove("live");
  };
}

// ---------- playground ----------

function addUserMessage(text) {
  $("messages-empty").hidden = true;
  $("messages").append(el("div", { class: "msg-user" }, text));
}

function addBotMessage(model) {
  $("messages-empty").hidden = true;
  const body = el("div", { class: "msg-body streaming" });
  const stats = el("div", { class: "msg-stats" });
  const wrap = el("div", { class: "msg-bot" },
    el("div", { class: "msg-head" }, el("span", { class: "mono" }, model), "streaming"),
    body, stats);
  $("messages").append(wrap);
  wrap.scrollIntoView({ block: "end" });
  return { wrap, body, stats, head: wrap.firstChild };
}

function setBusy(busy) {
  $("send").disabled = busy;
  $("stop").hidden = !busy;
  $("model").disabled = busy;
}

// Parse an OpenAI SSE response body, calling onDelta for each content piece.
async function readStream(response, onDelta) {
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buf = "";
  let finish = null;
  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    buf += decoder.decode(value, { stream: true });
    let idx;
    while ((idx = buf.indexOf("\n\n")) >= 0) {
      const event = buf.slice(0, idx);
      buf = buf.slice(idx + 2);
      for (const line of event.split("\n")) {
        if (!line.startsWith("data:")) continue;
        const data = line.slice(5).trim();
        if (data === "[DONE]") return finish;
        const json = JSON.parse(data);
        if (json.error) throw new Error(json.error.message);
        const choice = json.choices?.[0];
        if (choice?.delta?.content) onDelta(choice.delta.content);
        if (choice?.finish_reason) finish = choice.finish_reason;
      }
    }
  }
  return finish;
}

async function send(text) {
  const model = $("model").value;
  if (!model || !text.trim()) return;
  addUserMessage(text);
  state.messages.push({ role: "user", content: text });
  const ui = addBotMessage(model);
  setBusy(true);

  const system = $("system").value.trim();
  const messages = system ? [{ role: "system", content: system }, ...state.messages] : state.messages;
  const ctrl = new AbortController();
  state.abort = ctrl;
  const started = performance.now();
  let firstAt = null;
  let pieces = 0;
  let text_ = "";
  let finish = null;

  try {
    const resp = await fetch("/v1/chat/completions", {
      method: "POST",
      headers: { "content-type": "application/json", "x-tokenstick-client": "playground" },
      body: JSON.stringify({
        model,
        messages,
        stream: true,
        temperature: Number($("temperature").value),
        top_p: Number($("top_p").value),
        max_tokens: Number($("max_tokens").value) || 256,
      }),
      signal: ctrl.signal,
    });
    if (!resp.ok) {
      const err = await resp.json().catch(() => ({}));
      throw new Error(err.error?.message || `HTTP ${resp.status}`);
    }
    finish = await readStream(resp, (delta) => {
      firstAt ??= performance.now();
      pieces += 1;
      text_ += delta;
      ui.body.textContent = text_;
      ui.wrap.scrollIntoView({ block: "end" });
    });
  } catch (e) {
    if (e.name === "AbortError") {
      finish = "cancelled";
    } else {
      ui.body.classList.add("error");
      ui.body.textContent = text_ ? `${text_}\n\n${e.message}` : e.message;
    }
  } finally {
    state.abort = null;
    setBusy(false);
    ui.body.classList.remove("streaming");
    ui.head.lastChild.textContent = "";
  }

  if (text_) state.messages.push({ role: "assistant", content: text_ });
  const end = performance.now();
  const ttft = firstAt ? firstAt - started : null;
  const tps = firstAt && pieces > 1 ? (pieces - 1) / ((end - firstAt) / 1000) : null;
  ui.stats.replaceChildren(
    el("span", {}, `${pieces} tokens`),
    el("span", {}, `TTFT ${fmt(ttft, 0)} ms`),
    el("span", {}, `${fmt(tps)} tok/s`),
    el("span", {}, finish ?? "–"));
  ui.stats.title = "Measured in the browser, includes network time";
}

function setupPlayground() {
  $("composer").addEventListener("submit", (e) => {
    e.preventDefault();
    const text = $("msg").value;
    if (state.abort || !text.trim()) return;
    $("msg").value = "";
    send(text);
  });
  $("msg").addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey && !e.isComposing) {
      e.preventDefault();
      $("composer").requestSubmit();
    }
  });
  $("stop").addEventListener("click", () => state.abort?.abort());
  $("new-chat").addEventListener("click", () => {
    state.abort?.abort();
    state.messages = [];
    $("messages").replaceChildren($("messages-empty"));
    $("messages-empty").hidden = false;
    $("msg").focus();
  });
  for (const id of ["temperature", "top_p"]) {
    $(id).addEventListener("input", () => ($(`${id}-out`).textContent = Number($(id).value).toFixed(2)));
  }
}

async function loadModels() {
  try {
    const resp = await fetch("/v1/models");
    const { data } = await resp.json();
    $("model").replaceChildren(...data.map((m) => el("option", { value: m.id }, m.id)));
  } catch {
    $("model").replaceChildren(el("option", { value: "" }, "Could not load models"));
  }
}

// ---------- server ----------

function snippets(base, model) {
  return {
    curl: `curl -N ${base}/v1/chat/completions \\
  -H 'content-type: application/json' \\
  -d '{"model":"${model}",
       "messages":[{"role":"user","content":"hi"}],
       "stream":true}'`,
    python: `from openai import OpenAI

client = OpenAI(base_url="${base}/v1", api_key="not-needed")
stream = client.chat.completions.create(
    model="${model}",
    messages=[{"role": "user", "content": "hi"}],
    stream=True,
)
for chunk in stream:
    print(chunk.choices[0].delta.content or "", end="", flush=True)`,
    js: `import OpenAI from "openai";

const client = new OpenAI({ baseURL: "${base}/v1", apiKey: "not-needed" });
const stream = await client.chat.completions.create({
  model: "${model}",
  messages: [{ role: "user", content: "hi" }],
  stream: true,
});
for await (const chunk of stream) {
  process.stdout.write(chunk.choices[0]?.delta?.content ?? "");
}`,
  };
}

function setupServer() {
  const base = location.origin;
  $("api-addr").textContent = location.host;
  $("openai-url").textContent = `${base}/v1`;
  let current = "curl";
  const render = () => {
    const model = $("model").value || "qwen2.5-1.5b-instruct-w4a16";
    $("snippet").textContent = snippets(base, model)[current];
  };
  document.querySelectorAll("[data-snippet]").forEach((tab) =>
    tab.addEventListener("click", () => {
      current = tab.dataset.snippet;
      document.querySelectorAll("[data-snippet]").forEach((t) => t.setAttribute("aria-selected", String(t === tab)));
      render();
    }));
  document.querySelectorAll("[data-copy]").forEach((btn) =>
    btn.addEventListener("click", () => copyText($(btn.dataset.copy).textContent, btn)));
  $("model").addEventListener("change", render);
  render();
  return render;
}

// ---------- boot ----------

async function boot() {
  $("t-throttle").textContent = THROTTLE_C;
  $("t-power-budget").textContent = PD_BUDGET_W;
  window.addEventListener("hashchange", route);
  route();
  setupPlayground();
  const renderSnippet = setupServer();

  fetch("/api/v1/info")
    .then((r) => r.json())
    .then((info) => {
      $("nav-backend").textContent = info.backend;
      $("nav-version").textContent = `${info.name} ${info.version}`;
    })
    .catch(() => {});

  await loadModels();
  renderSnippet();
  connectEvents();
}

boot();
