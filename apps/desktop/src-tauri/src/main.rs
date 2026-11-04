//! TUIC 代理客户端外壳（Tauri 2）。
//!
//! 传输层没动：真正跑隧道的是同一个 `tuic-proxy-client`（src/bin/tuic-proxy-client.rs），
//! 这里只做三件事：
//!   1. 用表单参数启动 sidecar；
//!   2. 逐行读 sidecar 的 stdout/stderr，转发成日志事件；
//!   3. 关窗/停止时收掉子进程（Windows Job Object + 退出钩子双重兜底）。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::{Emitter, Manager, State};

struct ProxyProc {
    child: Arc<Mutex<Option<Child>>>,
    run_id: Arc<AtomicU64>,
    /// 每条 QUIC 连接（slot）最近一次上报的路径统计。
    stats: Arc<Mutex<HashMap<usize, SlotStats>>>,
    link_busy: Arc<AtomicBool>,
}

#[derive(Default, Clone, Copy)]
struct SlotStats {
    rtt_ms: u64,
    sent_packets: u64,
    lost_packets: u64,
    congestion_events: u64,
    mtu: u64,
}

/// 去掉 tracing-subscriber 写入的 ANSI 颜色转义。
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if chars.next() == Some('[') {
                for end in chars.by_ref() {
                    if ('@'..='~').contains(&end) {
                        break;
                    }
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// 解析 sidecar 的 `QUIC path stats slot=… sent_packets=…` 调试行。
fn parse_quic_stats(line: &str) -> Option<(usize, SlotStats)> {
    if !line.contains("QUIC path stats") {
        return None;
    }
    let clean = strip_ansi(line);
    let mut slot = None;
    let mut stats = SlotStats::default();
    for token in clean.split_whitespace() {
        let Some((key, value)) = token.split_once('=') else {
            continue;
        };
        match key {
            "slot" => slot = value.parse().ok(),
            "rtt_ms" => stats.rtt_ms = value.parse().unwrap_or(0),
            "sent_packets" => stats.sent_packets = value.parse().unwrap_or(0),
            "lost_packets" => stats.lost_packets = value.parse().unwrap_or(0),
            "congestion_events" => stats.congestion_events = value.parse().unwrap_or(0),
            "mtu" => stats.mtu = value.parse().unwrap_or(0),
            _ => {}
        }
    }
    slot.map(|slot| (slot, stats))
}

/// 把各 slot 的累计计数汇总成给界面用的一份快照。
fn stats_snapshot(board: &HashMap<usize, SlotStats>) -> serde_json::Value {
    let mut sent = 0u64;
    let mut lost = 0u64;
    let mut congestion = 0u64;
    let mut rtt_weighted = 0f64;
    let mut weight = 0u64;
    let mut mtu = 0u64;
    for stats in board.values() {
        sent += stats.sent_packets;
        lost += stats.lost_packets;
        congestion += stats.congestion_events;
        let samples = stats.sent_packets.max(1);
        rtt_weighted += stats.rtt_ms as f64 * samples as f64;
        weight += samples;
        mtu = mtu.max(stats.mtu);
    }
    json!({
        "connections": board.len(),
        "sentPackets": sent,
        "lostPackets": lost,
        "lossPercent": if sent > 0 { lost as f64 * 100.0 / sent as f64 } else { 0.0 },
        "rttMs": if weight > 0 { rtt_weighted / weight as f64 } else { 0.0 },
        "congestionEvents": congestion,
        "mtu": mtu,
    })
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ProxyConfig {
    host: String,
    port: u16,
    key: String,
    listen: String,
    listen_port: u16,
}

fn sidecar_name() -> &'static str {
    if cfg!(windows) {
        "tuic-proxy-client.exe"
    } else {
        "tuic-proxy-client"
    }
}

fn resolve_sidecar(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("TUIC_PROXY_CLIENT_BIN") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
        return Err(format!("TUIC_PROXY_CLIENT_BIN={} 不是文件", p.display()));
    }
    if let Ok(dir) = app.path().resource_dir() {
        let p = dir.join(sidecar_name());
        if p.is_file() {
            return Ok(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join(sidecar_name());
            if p.is_file() {
                return Ok(p);
            }
        }
    }
    Err(format!(
        "找不到 {}（应与主程序同目录，或用 TUIC_PROXY_CLIENT_BIN 指定）",
        sidecar_name()
    ))
}

#[cfg(windows)]
mod reaper {
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use std::sync::OnceLock;

    type Handle = isize;

    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;

    #[repr(C)]
    #[derive(Default)]
    struct IoCounters {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    #[repr(C)]
    #[derive(Default)]
    struct BasicLimitInformation {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct ExtendedLimitInformation {
        basic: BasicLimitInformation,
        io: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateJobObjectW(attributes: *mut std::ffi::c_void, name: *const u16) -> Handle;
        fn SetInformationJobObject(
            job: Handle,
            class: i32,
            info: *mut std::ffi::c_void,
            len: u32,
        ) -> i32;
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
    }

    /// 进程内唯一的 Job。创建失败返回 0，后续 assign 静默跳过（只丢失强杀兜底）。
    fn job() -> Handle {
        static JOB: OnceLock<Handle> = OnceLock::new();
        *JOB.get_or_init(|| unsafe {
            let job = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
            if job == 0 {
                return 0;
            }
            let mut info = ExtendedLimitInformation::default();
            info.basic.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                job,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                &mut info as *mut _ as *mut std::ffi::c_void,
                std::mem::size_of::<ExtendedLimitInformation>() as u32,
            );
            if ok == 0 {
                return 0;
            }
            job
        })
    }

    pub fn assign(child: &Child) {
        let job = job();
        if job == 0 {
            return;
        }
        unsafe {
            AssignProcessToJobObject(job, child.as_raw_handle() as Handle);
        }
    }
}

#[cfg(windows)]
fn assign_to_reaper(child: &Child) {
    reaper::assign(child);
}

#[cfg(unix)]
fn assign_to_reaper(_child: &Child) {}

fn kill_child(child: &mut Child) {
    #[cfg(unix)]
    {
        use std::time::Instant;
        let pid = child.id() as i32;
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_millis(1500);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
        let _ = child.wait();
    }
}

const LATENCY_URL: &str = "https://speed.cloudflare.com/__down?bytes=0";
const DOWNLOAD_URL: &str = "https://speed.cloudflare.com/__down?bytes=25000000";
const UPLOAD_URL: &str = "https://speed.cloudflare.com/__up";
const UPLOAD_BYTES: usize = 3 * 1024 * 1024;

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct LinkResult {
    latency_min_ms: f64,
    latency_avg_ms: f64,
    latency_max_ms: f64,
    jitter_ms: f64,
    latency_ok: u32,
    latency_failed: u32,
    download_mbps: f64,
    download_bytes: u64,
    download_ok: bool,
    upload_mbps: f64,
    upload_bytes: u64,
    upload_ok: bool,
    notes: Vec<String>,
}

struct BusyReset(Arc<AtomicBool>);
impl Drop for BusyReset {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn link_progress(app: &tauri::AppHandle, stage: &str, text: &str) {
    let _ = app.emit("link-progress", json!({ "stage": stage, "text": text }));
}

fn proxy_for(socks: &str) -> Result<reqwest::Proxy, String> {
    reqwest::Proxy::all(format!("socks5h://{}", socks.trim()))
        .map_err(|e| format!("SOCKS5 地址无效：{e}"))
}

/// 经本地 SOCKS5 入口访问 Cloudflare 测速端点，测延迟、下载与上传。
#[tauri::command]
async fn run_link_test(
    app: tauri::AppHandle,
    state: State<'_, ProxyProc>,
    socks: String,
) -> Result<LinkResult, String> {
    {
        let mut guard = state.child.lock().map_err(|e| e.to_string())?;
        let running = match guard.as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        };
        if !running {
            return Err("请先启动代理，再运行链路测试".into());
        }
    }
    if socks.trim().is_empty() {
        return Err("本地 SOCKS5 地址为空".into());
    }
    if state.link_busy.swap(true, Ordering::SeqCst) {
        return Err("链路测试正在进行中".into());
    }
    let _busy = BusyReset(state.link_busy.clone());

    let latency_client = reqwest::Client::builder()
        .proxy(proxy_for(&socks)?)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(15))
        .pool_max_idle_per_host(4)
        .build()
        .map_err(|e| format!("初始化测速客户端失败：{e}"))?;
    let client = reqwest::Client::builder()
        .proxy(proxy_for(&socks)?)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(40))
        .pool_max_idle_per_host(4)
        .build()
        .map_err(|e| format!("初始化测速客户端失败：{e}"))?;

    let mut result = LinkResult::default();

    // ---- 延迟：预热一次后连续请求，复用同一条 TCP/TLS 连接 ----
    link_progress(&app, "latency", "正在测试延迟…");
    let _ = latency_client.get(LATENCY_URL).send().await;
    let mut samples: Vec<f64> = Vec::new();
    let mut last_error = String::new();
    for _ in 0..8 {
        let started = std::time::Instant::now();
        match latency_client.get(LATENCY_URL).send().await {
            Ok(resp) if resp.status().is_success() => match resp.bytes().await {
                Ok(_) => samples.push(started.elapsed().as_secs_f64() * 1000.0),
                Err(e) => {
                    result.latency_failed += 1;
                    last_error = e.to_string();
                }
            },
            Ok(resp) => {
                result.latency_failed += 1;
                last_error = format!("HTTP {}", resp.status());
            }
            Err(e) => {
                result.latency_failed += 1;
                last_error = e.to_string();
            }
        }
    }
    result.latency_ok = samples.len() as u32;
    if !samples.is_empty() {
        let min = samples.iter().copied().fold(f64::INFINITY, f64::min);
        let max = samples.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let avg = samples.iter().sum::<f64>() / samples.len() as f64;
        let jitter = if samples.len() > 1 {
            let total: f64 = samples
                .windows(2)
                .map(|pair| (pair[1] - pair[0]).abs())
                .sum();
            total / (samples.len() - 1) as f64
        } else {
            0.0
        };
        result.latency_min_ms = min;
        result.latency_avg_ms = avg;
        result.latency_max_ms = max;
        result.jitter_ms = jitter;
    }

    // ---- 下载：最多读 12 秒，按实际收到的字节数算速率 ----
    link_progress(&app, "download", "正在测试下载速度…");
    let download_start = std::time::Instant::now();
    let download_deadline = download_start + Duration::from_secs(12);
    match tokio::time::timeout(Duration::from_secs(15), client.get(DOWNLOAD_URL).send()).await {
        Ok(Ok(mut resp)) if resp.status().is_success() => {
            let mut bytes = 0u64;
            loop {
                let now = std::time::Instant::now();
                if now >= download_deadline {
                    break;
                }
                let remaining = download_deadline.saturating_duration_since(now);
                match tokio::time::timeout(remaining, resp.chunk()).await {
                    Ok(Ok(Some(chunk))) => bytes += chunk.len() as u64,
                    Ok(Ok(None)) => break,
                    Ok(Err(e)) => {
                        result.notes.push(format!("下载中断：{e}"));
                        break;
                    }
                    Err(_) => break,
                }
            }
            if bytes > 0 {
                let elapsed = download_start.elapsed().as_secs_f64().max(0.001);
                result.download_ok = true;
                result.download_bytes = bytes;
                result.download_mbps = bytes as f64 * 8.0 / elapsed / 1_000_000.0;
            } else {
                result.notes.push("下载测速没有收到数据".into());
            }
        }
        Ok(Ok(resp)) => result
            .notes
            .push(format!("下载测速失败：HTTP {}", resp.status())),
        Ok(Err(e)) => result.notes.push(format!("下载测速失败：{e}")),
        Err(_) => result.notes.push("下载测速超时".into()),
    }

    // ---- 上传：固定 3 MiB 载荷 ----
    link_progress(&app, "upload", "正在测试上传速度…");
    let payload = vec![0u8; UPLOAD_BYTES];
    let upload_start = std::time::Instant::now();
    match tokio::time::timeout(
        Duration::from_secs(25),
        client.post(UPLOAD_URL).body(payload).send(),
    )
    .await
    {
        Ok(Ok(resp)) if resp.status().is_success() => {
            let elapsed = upload_start.elapsed().as_secs_f64().max(0.001);
            result.upload_ok = true;
            result.upload_bytes = UPLOAD_BYTES as u64;
            result.upload_mbps = UPLOAD_BYTES as f64 * 8.0 / elapsed / 1_000_000.0;
        }
        Ok(Ok(resp)) => result
            .notes
            .push(format!("上传测速失败：HTTP {}", resp.status())),
        Ok(Err(e)) => result.notes.push(format!("上传测速失败：{e}")),
        Err(_) => result.notes.push("上传测速超时（链路较慢或不可用）".into()),
    }

    if result.latency_ok == 0 && !result.download_ok && !result.upload_ok {
        let detail = if last_error.is_empty() {
            result.notes.join("；")
        } else {
            last_error
        };
        return Err(format!("无法通过本地代理访问测速服务：{detail}"));
    }

    link_progress(&app, "done", "测试完成");
    Ok(result)
}

#[tauri::command]
fn proxy_status(state: State<'_, ProxyProc>) -> String {
    let mut guard = match state.child.lock() {
        Ok(g) => g,
        Err(_) => return "idle".into(),
    };
    if let Some(child) = guard.as_mut() {
        match child.try_wait() {
            Ok(None) => return "running".into(),
            _ => {
                if let Some(mut done) = guard.take() {
                    let _ = done.wait();
                }
            }
        }
    }
    "idle".into()
}

#[tauri::command]
fn stop_proxy(app: tauri::AppHandle, state: State<'_, ProxyProc>) -> Result<(), String> {
    let mut guard = state.child.lock().map_err(|e| e.to_string())?;
    if let Some(mut child) = guard.take() {
        state.run_id.fetch_add(1, Ordering::SeqCst);
        kill_child(&mut child);
    }
    drop(guard);
    if let Ok(mut board) = state.stats.lock() {
        board.clear();
    }
    let _ = app.emit("proxy-stats", stats_snapshot(&HashMap::new()));
    Ok(())
}

#[tauri::command]
fn start_proxy(
    app: tauri::AppHandle,
    state: State<'_, ProxyProc>,
    cfg: ProxyConfig,
) -> Result<(), String> {
    if cfg.key.trim().is_empty() {
        return Err("预共享密钥不能为空".into());
    }
    if cfg.host.trim().is_empty() {
        return Err("服务器地址不能为空".into());
    }
    if cfg.port == 0 || cfg.listen_port == 0 {
        return Err("端口不能为 0".into());
    }

    let bin = resolve_sidecar(&app)?;
    let mut cmd = Command::new(&bin);
    cmd.arg(format!("--server={}:{}", cfg.host.trim(), cfg.port))
        .arg(format!("--socks={}:{}", cfg.listen.trim(), cfg.listen_port))
        .arg(format!("--psk={}", cfg.key.trim()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // QUIC 路径统计只在 debug 级别输出；界面的链路测试页依赖这些行。
    // 保留用户已有的 RUST_LOG，再追加 tunnel 的 debug 指令。
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into());
    cmd.env("RUST_LOG", format!("{filter},tuic_proxy::tunnel=debug"));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut guard = state.child.lock().map_err(|e| e.to_string())?;
    if let Some(child) = guard.as_mut() {
        match child.try_wait() {
            Ok(None) => return Err("代理已在运行".into()),
            Ok(Some(_)) | Err(_) => *guard = None,
        }
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("启动 {} 失败：{e}", bin.display()))?;
    assign_to_reaper(&child);
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err("sidecar 没有 stdout".into());
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err("sidecar 没有 stderr".into());
        }
    };

    let run_id = state.run_id.fetch_add(1, Ordering::SeqCst) + 1;
    let child_arc = state.child.clone();
    let run_arc = state.run_id.clone();
    let stats_arc = state.stats.clone();
    if let Ok(mut board) = stats_arc.lock() {
        board.clear();
    }
    *guard = Some(child);
    drop(guard);
    let _ = app.emit("proxy-state", json!({ "state": "running" }));
    let _ = app.emit("proxy-stats", stats_snapshot(&HashMap::new()));

    // stdout：日志行照常上报；QUIC 路径统计行汇总后单独发事件。
    {
        let app = app.clone();
        let run_arc = run_arc.clone();
        let stats_arc = stats_arc.clone();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if run_arc.load(Ordering::SeqCst) != run_id {
                    break;
                }
                if let Some((slot, stats)) = parse_quic_stats(&line) {
                    let snapshot = match stats_arc.lock() {
                        Ok(mut board) => {
                            board.insert(slot, stats);
                            stats_snapshot(&board)
                        }
                        Err(_) => continue,
                    };
                    let _ = app.emit("proxy-stats", snapshot);
                    continue;
                }
                let _ = app.emit("proxy-log", json!({ "stream": "out", "line": line }));
            }
        });
    }
    // stderr：全部按日志行上报（告警/错误都在这里）。
    {
        let app = app.clone();
        let run_arc = run_arc.clone();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if run_arc.load(Ordering::SeqCst) != run_id {
                    break;
                }
                let _ = app.emit("proxy-log", json!({ "stream": "err", "line": line }));
            }
        });
    }
    // 进程退出观察者：把自己从槽位摘掉，再把退出状态发给界面。
    {
        let app = app.clone();
        let child_arc = child_arc.clone();
        let run_arc = run_arc.clone();
        let stats_arc = stats_arc.clone();
        thread::spawn(move || loop {
            thread::sleep(Duration::from_millis(200));
            let mut guard = match child_arc.lock() {
                Ok(g) => g,
                Err(_) => break,
            };
            let Some(child) = guard.as_mut() else { break };
            match child.try_wait() {
                Ok(Some(status)) => {
                    *guard = None;
                    drop(guard);
                    if let Ok(mut board) = stats_arc.lock() {
                        board.clear();
                    }
                    let _ = app.emit("proxy-stats", stats_snapshot(&HashMap::new()));
                    if run_arc.load(Ordering::SeqCst) == run_id {
                        let _ = app.emit(
                            "proxy-state",
                            json!({ "state": "idle", "code": status.code() }),
                        );
                    }
                    break;
                }
                Ok(None) => {}
                Err(_) => {
                    let mut dead = guard.take();
                    drop(guard);
                    if let Some(mut child) = dead.take() {
                        kill_child(&mut child);
                    }
                    break;
                }
            }
        });
    }
    Ok(())
}

fn main() {
    let child = Arc::new(Mutex::new(None::<Child>));
    let run_id = Arc::new(AtomicU64::new(0));
    let proc_state = ProxyProc {
        child: child.clone(),
        run_id,
        stats: Arc::new(Mutex::new(HashMap::new())),
        link_busy: Arc::new(AtomicBool::new(false)),
    };

    let app = tauri::Builder::default()
        .manage(proc_state)
        .invoke_handler(tauri::generate_handler![
            start_proxy,
            stop_proxy,
            proxy_status,
            run_link_test
        ])
        .build(tauri::generate_context!())
        .expect("failed to build tauri application");

    app.run(move |_handle, event| {
        if let tauri::RunEvent::ExitRequested { .. } = event {
            if let Ok(mut guard) = child.lock() {
                if let Some(mut c) = guard.take() {
                    kill_child(&mut c);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANSI_STATS: &str = "\u{1b}[2m2026-11-04T07:29:02Z\u{1b}[0m \
\u{1b}[34mDEBUG\u{1b}[0m \u{1b}[2mtuic_proxy::tunnel\u{1b}[0m\u{1b}[2m:\u{1b}[0m \
QUIC path stats \u{1b}[3mslot\u{1b}[0m\u{1b}[2m=\u{1b}[0m1 \
\u{1b}[3mrtt_ms\u{1b}[0m\u{1b}[2m=\u{1b}[0m224 \
\u{1b}[3msent_packets\u{1b}[0m\u{1b}[2m=\u{1b}[0m23 \
\u{1b}[3mlost_packets\u{1b}[0m\u{1b}[2m=\u{1b}[0m2 \
\u{1b}[3mcongestion_events\u{1b}[0m\u{1b}[2m=\u{1b}[0m1 \
\u{1b}[3mmtu\u{1b}[0m\u{1b}[2m=\u{1b}[0m1452";

    #[test]
    fn parses_ansi_stats_line() {
        let (slot, stats) = parse_quic_stats(ANSI_STATS).expect("stats line should parse");
        assert_eq!(slot, 1);
        assert_eq!(stats.rtt_ms, 224);
        assert_eq!(stats.sent_packets, 23);
        assert_eq!(stats.lost_packets, 2);
        assert_eq!(stats.congestion_events, 1);
        assert_eq!(stats.mtu, 1452);
    }

    #[test]
    fn ignores_other_log_lines() {
        assert!(parse_quic_stats("QUIC tunnel established slot=0").is_none());
        assert!(parse_quic_stats("DEBUG tuic_proxy::tunnel: QUIC path stats rtt_ms=1").is_none());
    }

    #[test]
    fn snapshot_aggregates_slots() {
        let mut board = HashMap::new();
        board.insert(
            0,
            SlotStats {
                rtt_ms: 100,
                sent_packets: 100,
                lost_packets: 1,
                congestion_events: 0,
                mtu: 1452,
            },
        );
        board.insert(
            1,
            SlotStats {
                rtt_ms: 200,
                sent_packets: 300,
                lost_packets: 5,
                congestion_events: 2,
                mtu: 1400,
            },
        );
        let snapshot = stats_snapshot(&board);
        assert_eq!(snapshot["connections"], 2);
        assert_eq!(snapshot["sentPackets"], 400);
        assert_eq!(snapshot["lostPackets"], 6);
        assert_eq!(snapshot["congestionEvents"], 2);
        assert_eq!(snapshot["mtu"], 1452);
        let loss = snapshot["lossPercent"].as_f64().expect("loss percent");
        assert!((loss - 1.5).abs() < f64::EPSILON);
        let rtt = snapshot["rttMs"].as_f64().expect("rtt");
        assert!((rtt - 175.0).abs() < f64::EPSILON);
    }
}
