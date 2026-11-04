//! PSK 代理客户端外壳（Tauri 2）。
//!
//! 传输层没动：真正跑隧道的是同一个 `psk-client`（src/bin/psk-client.rs），
//! 这里只做三件事：
//!   1. 用表单参数启动 sidecar；
//!   2. 逐行读 sidecar 的 stdout/stderr，转发成日志事件；
//!   3. 关窗/停止时收掉子进程（Windows Job Object + 退出钩子双重兜底）。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use tauri::{Emitter, Manager, State};

struct ProxyProc {
    child: Arc<Mutex<Option<Child>>>,
    run_id: Arc<AtomicU64>,
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
        "psk-client.exe"
    } else {
        "psk-client"
    }
}

fn resolve_sidecar(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("PSKPROXY_CLIENT_BIN") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
        return Err(format!("PSKPROXY_CLIENT_BIN={} 不是文件", p.display()));
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
        "找不到 {}（应与主程序同目录，或用 PSKPROXY_CLIENT_BIN 指定）",
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
fn stop_proxy(state: State<'_, ProxyProc>) -> Result<(), String> {
    let mut guard = state.child.lock().map_err(|e| e.to_string())?;
    if let Some(mut child) = guard.take() {
        state.run_id.fetch_add(1, Ordering::SeqCst);
        kill_child(&mut child);
    }
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
    *guard = Some(child);
    drop(guard);
    let _ = app.emit("proxy-state", json!({ "state": "running" }));

    // stdout：全部按日志行上报。
    {
        let app = app.clone();
        let run_arc = run_arc.clone();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if run_arc.load(Ordering::SeqCst) != run_id {
                    break;
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
    };

    let app = tauri::Builder::default()
        .manage(proc_state)
        .invoke_handler(tauri::generate_handler![
            start_proxy,
            stop_proxy,
            proxy_status
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