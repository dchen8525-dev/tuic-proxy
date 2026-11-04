// TUIC 代理桌面客户端前端（无打包器，直接由 Tauri 加载静态文件）。
// 通过 window.__TAURI__.core.invoke 调用 Rust 侧 command。

/* global window */
const T = window.__TAURI__;
if (!T) {
  document.write('<div>需要 Tauri 运行时才能启动，请用打包后的 exe 运行。</div>');
  throw new Error("no tauri");
}
const invoke = T.core.invoke;
const listen = T.event.listen;

const $ = (id) => document.getElementById(id);
const storage_key = "tuic-proxy-config";

// ---------- 导航 ----------
const pages = {};
document.querySelectorAll("#nav button").forEach((btn) => {
  pages[btn.dataset.page] = btn;
});
function show(page) {
  document.querySelectorAll(".page").forEach((el) => el.classList.remove("active"));
  document.querySelectorAll("#nav button").forEach((el) => el.classList.remove("active"));
  document.getElementById("page-" + page).classList.add("active");
  pages[page].classList.add("active");
}
function bindNav() {
  document.querySelectorAll("#nav button").forEach((btn) => {
    btn.addEventListener("click", () => show(btn.dataset.page));
  });
}

// ---------- 配置存取 ----------
const state = {
  running: false,
  proxy: null,
};

function readForm() {
  return {
    host: $("host").value.trim(),
    port: parseInt($("port").value || "0", 10),
    key: $("key").value.trim(),
    listen: $("listen").value.trim() || "127.0.0.1",
    listenPort: parseInt($("listenPort").value || "0", 10),
  };
}
function saveForm(cfg) {
  const data = Object.assign({}, cfg);
  if (!data.key) data.key = localStorage.getItem(storage_key + ":key") || "";
  localStorage.setItem(storage_key + ":key", data.key || "");
  delete data.key;
  localStorage.setItem(storage_key, JSON.stringify(data));
}
function restore() {
  try {
    const data = JSON.parse(localStorage.getItem(storage_key) || "{}");
    if (data.host) $("host").value = data.host;
    if (data.port) $("port").value = data.port;
    if (data.listen) $("listen").value = data.listen;
    if (data.listenPort) $("listenPort").value = data.listenPort;
    $("key").value = localStorage.getItem(storage_key + ":key") || "";
  } catch (e) {}
}

// ---------- 横幅 ----------
let bannerTimer = null;
function banner(kind, text, sticky) {
  const el = $("banner");
  el.className = "banner show " + kind;
  el.textContent = text;
  clearTimeout(bannerTimer);
  if (!sticky) bannerTimer = setTimeout(() => (el.className = "banner"), 4000);
}

// ---------- 日志 ----------
let logCount = 0;
function addLog(stream, line) {
  const box = $("log-box");
  const div = document.createElement("div");
  div.className = "log-line " + (stream === "err" ? "err" : "none");
  div.textContent = line;
  box.appendChild(div);
  logCount++;
  $("log-count").textContent = logCount + " 行";
  box.scrollTop = box.scrollHeight;
  // 物理行过多时截断，避免内存无限增长。
  while (box.childElementCount > 4000) box.removeChild(box.firstChild);
}

// ---------- 状态刷新 ----------
function setRunning(on) {
  state.running = on;
  $("rail-dot").classList.toggle("on", on);
  $("rail-text").textContent = on ? "运行中" : "未运行";
  $("st1-pill").textContent = on ? "运行中" : "未运行";
  $("st1-pill").className = "pill " + (on ? "on" : "off");
  $("start-btn").disabled = on;
  $("start-btn").classList.toggle("disabled", on);
  $("stop-btn").disabled = !on;
  $("stop-btn").classList.toggle("disabled", !on);
}

async function refreshStatus() {
  const st = await invoke("proxy_status");
  setRunning(st === "running");
}
function syncStatusMeta() {
  if (state.proxy) {
    $("st1-socks").textContent = state.proxy.listen + ":" + state.proxy.listenPort;
    $("st1-server").textContent = state.proxy.host + ":" + state.proxy.port;
  }
}

// ---------- 主流程 ----------
async function startProxy() {
  const cfg = readForm();
  if (!cfg.host) return banner("err", "请填写服务端地址");
  if (!cfg.key) return banner("err", "请填写预共享密钥");
  $("start-btn").disabled = true;
  try {
    await invoke("start_proxy", { cfg });
    saveForm(cfg);
    state.proxy = cfg;
    banner("ok", "代理已启动", true);
  } catch (err) {
    banner("err", String(err));
    $("start-btn").disabled = false;
    return;
  }
  syncStatusMeta();
  show("status");
  refreshStatus();
}
async function stopProxy() {
  try {
    await invoke("stop_proxy");
    banner("info", "已停止");
  } catch (err) {
    banner("err", String(err));
  }
  refreshStatus();
}

// ---------- 事件 ----------
async function onState(ev) {
  const s = ev.payload;
  setRunning(s.state === "running");
  if (s.state === "running") {
    syncStatusMeta();
    show("status");
  }
  if (s.state === "idle" && s.code == null) {
    addLog("err", "代理进程已退出");
  }
}
function onLog(ev) {
  addLog(ev.payload.stream, ev.payload.line);
}

// ---------- init ----------
document.addEventListener("DOMContentLoaded", async () => {
  bindNav();
  restore();
  $("start-btn").addEventListener("click", startProxy);
  $("stop-btn").addEventListener("click", stopProxy);
  $("clear-log").addEventListener("click", () => {
    $("log-box").textContent = "";
    logCount = 0;
    $("log-count").textContent = "0 行";
  });
  try {
    await listen("proxy-state", onState);
    await listen("proxy-log", onLog);
  } catch (e) {
    banner("err", "无法订阅事件: " + String(e), true);
  }
  try {
    const st = await invoke("proxy_status");
    if (st === "running") {
      state.proxy = readForm();
      setRunning(true);
      syncStatusMeta();
    }
  } catch (e) {
    // 启动时的状态读取失败不阻塞界面。
  }
});