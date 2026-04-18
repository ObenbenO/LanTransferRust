//! 进程内共享状态。

use std::collections::HashMap;
use std::fs;
use std::io;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use uuid::Uuid;

use crate::api::types::*;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub struct FileServiceHandle {
    shutdown: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl Drop for FileServiceHandle {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl FileServiceHandle {
    pub(crate) fn new(shutdown: Arc<AtomicBool>, join: thread::JoinHandle<()>) -> Self {
        Self {
            shutdown,
            join: Some(join),
        }
    }
}

#[derive(Default)]
pub struct AppState {
    pub initialized: bool,
    pub cache_dir: Option<PathBuf>,
    pub device_id: String,
    pub session_id: String,
    pub receive_path: Option<PathBuf>,
    pub profile: Option<LocalProfileDto>,
    pub remote_host_enabled: bool,
    pub peers: HashMap<String, PeerInfoDto>,
    pub active_peer_id: Option<String>,
    pub file_service: Option<FileServiceHandle>,
    pub file_listen_port: Option<u16>,
    pub receive_events: Vec<FileReceiveEventDto>,
    pub transfer_progress: Vec<TransferProgressDto>,
    pub remote_host_token: Option<String>,
    pub remote_host_port: Option<u16>,
    pub remote_host_service: Option<FileServiceHandle>,
    pub remote_client_peer: Option<(String, u16)>,
    pub remote_client_token: Option<String>,
}

impl AppState {
    pub fn load_or_create_device_id(cache_dir: Option<PathBuf>) -> String {
        let dir = cache_dir.unwrap_or_else(|| std::env::temp_dir().join("xtransfer_rust"));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("xtransfer_device_id");
        if let Ok(existing) = fs::read_to_string(&path) {
            let t = existing.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
        let id = Uuid::new_v4().to_string();
        let _ = fs::write(&path, &id);
        id
    }
}

static STATE: OnceLock<Mutex<AppState>> = OnceLock::new();

fn global_state() -> &'static Mutex<AppState> {
    STATE.get_or_init(|| Mutex::new(AppState::default()))
}

pub fn with_state_mut<T>(f: impl FnOnce(&mut AppState) -> T) -> T {
    let mut g = global_state()
        .lock()
        .expect("rust app state mutex poisoned");
    f(&mut g)
}

pub fn with_state<T>(f: impl FnOnce(&AppState) -> T) -> T {
    let g = global_state()
        .lock()
        .expect("rust app state mutex poisoned");
    f(&g)
}

pub fn default_portable_cache_dir() -> PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .or_else(|| std::env::current_dir().ok());
    exe_dir
        .unwrap_or_else(std::env::temp_dir)
        .join("lan_transfer_data")
}

pub fn app_data_dir() -> PathBuf {
    let dir = with_state(|s| s.cache_dir.clone()).unwrap_or_else(default_portable_cache_dir);
    let _ = fs::create_dir_all(&dir);
    dir
}

/// 诊断日志目录：`{应用数据目录}/logs/`（与可执行文件旁 `lan_transfer_data` 一致，便于打包分发）。
pub fn diag_log_path(file_name: &str) -> PathBuf {
    let logs = app_data_dir().join("logs");
    let _ = fs::create_dir_all(&logs);
    logs.join(file_name)
}

pub fn validate_receive_dir(path: &str) -> Result<PathBuf, ApiError> {
    if path.trim().is_empty() {
        return Err(ApiError::new("INVALID_PATH", "接收目录不能为空"));
    }
    let p = PathBuf::from(path);
    if !p.exists() {
        return Err(ApiError::new(
            "PATH_NOT_FOUND",
            format!("目录不存在: {}", path),
        ));
    }
    let meta =
        fs::metadata(&p).map_err(|e| ApiError::new("PATH_IO", format!("无法访问目录: {e}")))?;
    if !meta.is_dir() {
        return Err(ApiError::new(
            "NOT_A_DIRECTORY",
            format!("路径不是目录: {}", path),
        ));
    }
    Ok(p)
}

/// 启动简易 TCP 占位监听（后续替换为 QUIC/正式文件协议）。
pub fn spawn_file_listener(
    preferred_port: Option<u16>,
) -> Result<(u16, FileServiceHandle), ApiError> {
    let listener = match preferred_port {
        Some(p) => TcpListener::bind(("0.0.0.0", p)),
        None => TcpListener::bind(("0.0.0.0", 0)),
    }
    .map_err(|e| ApiError::new("FILE_BIND_FAILED", format!("文件服务绑定失败: {e}")))?;
    let actual_port = listener
        .local_addr()
        .map_err(|e| ApiError::new("FILE_BIND_FAILED", format!("{e}")))?
        .port();

    listener
        .set_nonblocking(true)
        .map_err(|e| ApiError::new("FILE_LISTENER_IO", format!("set_nonblocking: {e}")))?;

    let shutdown = Arc::new(AtomicBool::new(false));
    let sd = Arc::clone(&shutdown);
    let join = thread::spawn(move || {
        while !sd.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    std::thread::spawn(move || {
                        crate::file_transfer::handle_incoming_connection(stream);
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(_) => break,
            }
        }
    });

    let handle = FileServiceHandle {
        shutdown,
        join: Some(join),
    };
    Ok((actual_port, handle))
}

#[allow(dead_code)]
pub fn try_tcp_ping(host: &str, port: u16) -> Result<(), ApiError> {
    let addr = format!("{host}:{port}");
    TcpStream::connect(addr.as_str())
        .map_err(|e| ApiError::new("TCP_CONNECT_FAILED", format!("无法连接 {addr}: {e}")))?;
    Ok(())
}

pub fn push_receive_event(state: &mut AppState, ev: FileReceiveEventDto) {
    state.receive_events.push(ev);
}

pub fn push_progress(state: &mut AppState, p: TransferProgressDto) {
    state.transfer_progress.push(p);
}

pub fn now_ms_pub() -> i64 {
    now_ms()
}
