//! 局域网 TCP 文件传输（协议 v1）：多文件单连接、UTF-8 元数据、流式读写。

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::api::types::{ApiError, FileReceiveEventDto, TransferProgressDto};
use crate::app_state::{
    diag_log_path, now_ms_pub, push_progress, push_receive_event, with_state_mut,
};

/// 接收目录副本：供 TCP 接收线程读取，**不得**在此路径上调用 `with_state` / `with_state_mut`。
/// 否则与主线程里 `pull_transfer_progress`、`push_prog` 等争抢 `AppState` 锁时，
/// 易出现接收端拿不到锁、无法发 ACK，发送端后续写入报 10054（连接被重置）。
static FILE_RECEIVE_DIR_CACHE: Mutex<Option<PathBuf>> = Mutex::new(None);

/// 与 `AppState.receive_path` 同步；在 [crate::api::config::set_default_receive_path] 内调用。
pub fn sync_file_receive_dir_cache(dir: Option<PathBuf>) {
    if let Ok(mut g) = FILE_RECEIVE_DIR_CACHE.lock() {
        *g = dir;
    }
}

fn receive_dir_for_ingest() -> Option<PathBuf> {
    FILE_RECEIVE_DIR_CACHE.lock().ok().and_then(|g| g.clone())
}

const MAGIC: &[u8; 4] = b"XTX1";
/// 头里的协议版本号（与首版一致为 1）。头之后必有 1 字节 ACK（见下文），避免旧实现因「仅支持 version=1」在读完版本就关连接导致发送方 10054。
const PROTO_VERSION: u16 = 1;

/// 接收端在解析完头后回复 1 字节，发送端必须先读再发文件数据。
const ACK_OK: u8 = 0;
const ACK_NO_RECEIVE_DIR: u8 = 1;

const MAX_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_NAME_BYTES: usize = 512;
const MAX_SENDER_ID_BYTES: usize = 512;
const MAX_FILE_COUNT: u32 = 512;
const IO_BUF: usize = 64 * 1024;

/// 诊断日志：写入应用数据目录下的 `logs/lan_transfer_file_transfer.log`。
const FT_DIAG_LOG: &str = "lan_transfer_file_transfer.log";

fn ft_append_diag_log(line: &str) {
    let path = diag_log_path(FT_DIAG_LOG);
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let _ = writeln!(f, "[{ts}] {line}");
    }
}

fn io_err(msg: impl Into<String>, e: io::Error) -> ApiError {
    ApiError::new("FILE_IO", format!("{}: {e}", msg.into()))
}

/// 发送端连接/读写失败时附加说明（常见于 10054 / ConnectionReset）。
fn io_err_send(msg: impl Into<String>, e: io::Error) -> ApiError {
    let head = msg.into();
    let mut detail = format!("{head}: {e}");
    if matches!(
        e.kind(),
        io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
    ) || e.raw_os_error() == Some(10054)
    {
        detail.push_str(
            " — 请核对：对端为本应用且已更新到相同版本；连接的是「文件传输」端口而非远程协助端口；\
对端已选择接收目录；防火墙放行该 TCP；若对端使用 IPv6 请两端均使用当前版本。",
        );
    }
    ApiError::new("FILE_IO", detail)
}

/// `TcpStream::connect("a:b:c:port")` 在 Rust 中会解析失败；IPv6 需 `[addr]:port`。
fn tcp_addr_for_dial(host: &str, port: u16) -> String {
    let h = host.trim();
    if h.contains(':') && !h.starts_with('[') {
        format!("[{h}]:{port}")
    } else {
        format!("{h}:{port}")
    }
}

fn write_all(stream: &mut TcpStream, mut data: &[u8]) -> Result<(), ApiError> {
    while !data.is_empty() {
        let n = stream.write(data).map_err(|e| io_err("写入套接字", e))?;
        if n == 0 {
            return Err(ApiError::new("FILE_IO", "写入套接字意外结束"));
        }
        data = &data[n..];
    }
    Ok(())
}

fn write_all_send(stream: &mut TcpStream, mut data: &[u8]) -> Result<(), ApiError> {
    while !data.is_empty() {
        let n = stream.write(data).map_err(|e| {
            ft_append_diag_log(&format!(
                "[ft-send] write_all_send FAILED len_remain={} err={e}",
                data.len()
            ));
            io_err_send("写入套接字", e)
        })?;
        if n == 0 {
            return Err(ApiError::new("FILE_IO", "写入套接字意外结束"));
        }
        data = &data[n..];
    }
    Ok(())
}

fn read_exact(stream: &mut TcpStream, buf: &mut [u8]) -> Result<(), ApiError> {
    let mut off = 0;
    while off < buf.len() {
        let n = stream
            .read(&mut buf[off..])
            .map_err(|e| io_err("读取套接字", e))?;
        if n == 0 {
            return Err(ApiError::new("FILE_IO", "连接对端提前关闭"));
        }
        off += n;
    }
    Ok(())
}

fn read_exact_send(stream: &mut TcpStream, buf: &mut [u8]) -> Result<(), ApiError> {
    let mut off = 0;
    while off < buf.len() {
        let n = stream.read(&mut buf[off..]).map_err(|e| {
            ft_append_diag_log(&format!(
                "[ft-send] read_exact_send FAILED need={} got_off={off} err={e}",
                buf.len(),
            ));
            io_err_send("读取套接字", e)
        })?;
        if n == 0 {
            return Err(ApiError::new("FILE_IO", "连接对端提前关闭"));
        }
        off += n;
    }
    Ok(())
}

fn sanitize_saved_name(name: &str) -> String {
    let t = name.trim();
    if t.is_empty() {
        return "unnamed".to_string();
    }
    let bad = |c: char| c == '/' || c == '\\' || c == ':' || c == '\0' || c == '\r' || c == '\n';
    if t.contains("..") || t.chars().any(bad) {
        "unsafe_name".to_string()
    } else {
        t.to_string()
    }
}

fn unique_save_path(dir: &Path, original: &str) -> PathBuf {
    let safe = sanitize_saved_name(original);
    let path = dir.join(&safe);
    if !path.exists() {
        return path;
    }
    let p = Path::new(&safe);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    for i in 1_u32.. {
        let candidate = dir.join(format!("{stem}_{i}{ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{stem}_{}{ext}", u32::MAX))
}

fn basename_for_send(src: &Path) -> String {
    src.file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unnamed".to_string())
}

fn push_prog(
    transfer_id: &str,
    bytes_sent: i64,
    total_bytes: i64,
    phase: &str,
    err: Option<String>,
) {
    with_state_mut(|s| {
        push_progress(
            s,
            TransferProgressDto {
                transfer_id: transfer_id.to_string(),
                bytes_sent,
                total_bytes,
                phase: phase.to_string(),
                error: err,
            },
        );
    });
}

/// 阻塞发送：单 TCP 连接内依次写入头与多个文件正文。
pub fn send_files_blocking(
    host: &str,
    port: u16,
    sender_id: &str,
    message: &str,
    file_paths: &[String],
    transfer_id: &str,
    total_bytes: i64,
) -> Result<(), ApiError> {
    let sid = sender_id.as_bytes();
    if sid.len() > MAX_SENDER_ID_BYTES {
        return Err(ApiError::new(
            "INVALID_SENDER",
            format!("sender id 过长 (>{MAX_SENDER_ID_BYTES})"),
        ));
    }
    let msg_bytes = message.as_bytes();
    if msg_bytes.len() > MAX_MESSAGE_BYTES {
        return Err(ApiError::new(
            "MESSAGE_TOO_LONG",
            format!("留言过长 (>{MAX_MESSAGE_BYTES} 字节)"),
        ));
    }

    let addr = tcp_addr_for_dial(host, port);
    ft_append_diag_log(&format!(
        "[ft-send] start tid={transfer_id} dial={addr} nfiles={} total_bytes={total_bytes} sender_id_len={}",
        file_paths.len(),
        sid.len(),
    ));
    push_prog(transfer_id, 0, total_bytes, "connecting", None);
    let mut stream = TcpStream::connect(addr.as_str()).map_err(|e| {
        ft_append_diag_log(&format!("[ft-send] TCP_CONNECT_FAILED {addr}: {e}"));
        ApiError::new("TCP_CONNECT_FAILED", format!("无法连接 {addr}: {e}"))
    })?;
    let _ = stream.set_nodelay(true);
    let local = stream
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "?".into());
    let peer = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "?".into());
    ft_append_diag_log(&format!(
        "[ft-send] tcp_ok local={local} peer={peer} nodelay=on"
    ));

    let nfiles = file_paths.len() as u32;
    if nfiles > MAX_FILE_COUNT {
        return Err(ApiError::new(
            "TOO_MANY_FILES",
            format!("一次最多发送 {MAX_FILE_COUNT} 个文件"),
        ));
    }

    let mut header = Vec::with_capacity(32 + sid.len() + msg_bytes.len());
    header.extend_from_slice(MAGIC);
    header.extend_from_slice(&PROTO_VERSION.to_le_bytes());
    header.extend_from_slice(&(sid.len() as u16).to_le_bytes());
    header.extend_from_slice(sid);
    header.extend_from_slice(&(msg_bytes.len() as u32).to_le_bytes());
    header.extend_from_slice(msg_bytes);
    header.extend_from_slice(&nfiles.to_le_bytes());
    write_all_send(&mut stream, &header)?;
    ft_append_diag_log(&format!(
        "[ft-send] header_written bytes={} (magic+ver+sid+msg+nfiles)",
        header.len()
    ));

    let mut ack = [0u8; 1];
    read_exact_send(&mut stream, &mut ack)?;
    ft_append_diag_log(&format!("[ft-send] ack_byte={}", ack[0]));
    match ack[0] {
        ACK_OK => {}
        ACK_NO_RECEIVE_DIR => {
            ft_append_diag_log("[ft-send] abort PEER_NO_RECEIVE_DIR");
            return Err(ApiError::new(
                "PEER_NO_RECEIVE_DIR",
                "对方未设置接收目录，请让对方在「设置」中选择接收文件夹",
            ));
        }
        b => {
            ft_append_diag_log(&format!("[ft-send] abort FILE_PROTO code={b}"));
            return Err(ApiError::new(
                "FILE_PROTO",
                format!("对端拒绝接收（代码 {b}）"),
            ));
        }
    }

    let mut sent: i64 = 0;
    push_prog(transfer_id, sent, total_bytes, "sending", None);

    for fp in file_paths {
        let path = Path::new(fp);
        let name = basename_for_send(path);
        let nb = name.as_bytes();
        if nb.len() > MAX_NAME_BYTES {
            return Err(ApiError::new(
                "NAME_TOO_LONG",
                format!("文件名过长: {name}"),
            ));
        }
        let mut f = File::open(path).map_err(|e| io_err(format!("打开 {fp}"), e))?;
        let size = f
            .metadata()
            .map_err(|e| io_err(format!("元数据 {fp}"), e))?
            .len();

        let mut meta = Vec::with_capacity(2 + nb.len() + 8);
        meta.extend_from_slice(&(nb.len() as u16).to_le_bytes());
        meta.extend_from_slice(nb);
        meta.extend_from_slice(&size.to_le_bytes());
        write_all_send(&mut stream, &meta)?;
        ft_append_diag_log(&format!(
            "[ft-send] file_meta_sent name={name} size={size} meta_len={}",
            meta.len()
        ));

        let mut buf = vec![0u8; IO_BUF];
        let mut remaining = size;
        let mut chunk_idx: u64 = 0;
        while remaining > 0 {
            let to_read = (remaining as usize).min(IO_BUF);
            let n = f
                .read(&mut buf[..to_read])
                .map_err(|e| io_err(format!("读取 {fp}"), e))?;
            if n == 0 {
                ft_append_diag_log(&format!(
                    "[ft-send] ERROR file_truncated path={fp} remaining={remaining}"
                ));
                return Err(ApiError::new("FILE_IO", format!("文件提前结束: {fp}")));
            }
            chunk_idx += 1;
            write_all_send(&mut stream, &buf[..n])?;
            if chunk_idx == 1 || remaining <= n as u64 {
                ft_append_diag_log(&format!(
                    "[ft-send] chunk_sent_ok name={name} i={chunk_idx} n={n} remaining_after={}",
                    remaining.saturating_sub(n as u64)
                ));
            }
            remaining -= n as u64;
            sent += n as i64;
            push_prog(transfer_id, sent, total_bytes, "sending", None);
        }
        ft_append_diag_log(&format!(
            "[ft-send] file_done name={name} bytes={size} total_sent_so_far={sent}"
        ));
    }

    push_prog(transfer_id, total_bytes, total_bytes, "completed", None);
    ft_append_diag_log(&format!(
        "[ft-send] completed_ok tid={transfer_id} total_sent={total_bytes}"
    ));
    Ok(())
}

fn read_u16(stream: &mut TcpStream) -> Result<u16, ApiError> {
    let mut b = [0u8; 2];
    read_exact(stream, &mut b)?;
    Ok(u16::from_le_bytes(b))
}

fn read_u32(stream: &mut TcpStream) -> Result<u32, ApiError> {
    let mut b = [0u8; 4];
    read_exact(stream, &mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_u64(stream: &mut TcpStream) -> Result<u64, ApiError> {
    let mut b = [0u8; 8];
    read_exact(stream, &mut b)?;
    Ok(u64::from_le_bytes(b))
}

fn read_string(stream: &mut TcpStream, max_len: usize, label: &str) -> Result<String, ApiError> {
    let len = read_u32(stream)? as usize;
    if len > max_len {
        return Err(ApiError::new(
            "PROTO_BAD",
            format!("{label} 长度非法: {len}"),
        ));
    }
    let mut v = vec![0u8; len];
    read_exact(stream, &mut v)?;
    String::from_utf8(v).map_err(|_| ApiError::new("PROTO_BAD", format!("{label} 非 UTF-8")))
}

/// 处理一条入站连接：解析协议、落盘、推送接收事件。
pub fn handle_incoming_connection(mut stream: TcpStream) {
    let _ = stream.set_nodelay(true);
    // `spawn_file_listener` 将 TcpListener 设为非阻塞以便轮询 accept；在 Windows 上，
    // 由此 accept 得到的 TcpStream 可能仍为非阻塞，随后 read 会立刻返回 WSAEWOULDBLOCK (10035)，
    // 而不是阻塞等待对端发送文件 meta，表现为「刚发完 ACK 就读失败」与大文件首包后连接被关。
    if let Err(e) = stream.set_nonblocking(false) {
        ft_append_diag_log(&format!(
            "[ft-recv] set_nonblocking(false) failed: {e} — 尝试继续处理"
        ));
        // 即使设置阻塞模式失败，也继续处理连接，避免发送端遇到连接重置
    }
    let peer = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "?".to_string());
    ft_append_diag_log(&format!("[ft-recv] accept peer={peer}"));
    // 注意：handle_incoming_inner 在多数协议/落盘错误路径上仍返回 Ok（仅 push_recv_err），
    // 成功与否请看 [ft-recv] header_ok / recv_file_* / all_files_ok 与 UI 事件，勿依赖 Result。
    let _ = handle_incoming_inner(&mut stream);
    ft_append_diag_log(&format!("[ft-recv] session_end peer={peer}"));
}

fn push_recv_err(file_name: String, message: String, sender_peer_id: String, err: String) {
    with_state_mut(|s| {
        push_receive_event(
            s,
            FileReceiveEventDto {
                file_name,
                message,
                sender_peer_id,
                saved_path: None,
                error: Some(err),
                timestamp_ms: now_ms_pub(),
            },
        );
    });
}

fn handle_incoming_inner(stream: &mut TcpStream) -> Result<(), ApiError> {
    let mut magic = [0u8; 4];
    if let Err(e) = read_exact(stream, &mut magic) {
        ft_append_diag_log(&format!("[ft-recv] read_magic_fail: {}", e.message));
        push_recv_err(String::new(), String::new(), String::new(), e.message);
        return Ok(());
    }
    if &magic != MAGIC {
        ft_append_diag_log(&format!(
            "[ft-recv] bad_magic got={magic:?} expected={MAGIC:?}"
        ));
        push_recv_err(
            String::new(),
            String::new(),
            String::new(),
            "非本应用文件协议（魔数不匹配）".to_string(),
        );
        return Ok(());
    }
    let ver = match read_u16(stream) {
        Ok(v) => v,
        Err(e) => {
            push_recv_err(String::new(), String::new(), String::new(), e.message);
            return Ok(());
        }
    };
    if ver != PROTO_VERSION {
        push_recv_err(
            String::new(),
            String::new(),
            String::new(),
            format!("协议版本不支持: {ver}（需要 {PROTO_VERSION}）"),
        );
        return Ok(());
    }

    let sid_len = match read_u16(stream) {
        Ok(v) => v as usize,
        Err(e) => {
            push_recv_err(String::new(), String::new(), String::new(), e.message);
            return Ok(());
        }
    };
    if sid_len > MAX_SENDER_ID_BYTES {
        push_recv_err(
            String::new(),
            String::new(),
            String::new(),
            "发送方 ID 过长".to_string(),
        );
        return Ok(());
    }
    let mut sid_buf = vec![0u8; sid_len];
    if let Err(e) = read_exact(stream, &mut sid_buf) {
        push_recv_err(String::new(), String::new(), String::new(), e.message);
        return Ok(());
    }
    let sender_id = match String::from_utf8(sid_buf) {
        Ok(s) => s,
        Err(_) => {
            push_recv_err(
                String::new(),
                String::new(),
                String::new(),
                "发送方 ID 非 UTF-8".to_string(),
            );
            return Ok(());
        }
    };

    let message = match read_string(stream, MAX_MESSAGE_BYTES, "留言") {
        Ok(m) => m,
        Err(e) => {
            push_recv_err(String::new(), String::new(), sender_id, e.message);
            return Ok(());
        }
    };

    let nfiles = match read_u32(stream) {
        Ok(n) => n,
        Err(e) => {
            push_recv_err(String::new(), message, sender_id, e.message);
            return Ok(());
        }
    };
    if nfiles > MAX_FILE_COUNT {
        push_recv_err(
            String::new(),
            message.clone(),
            sender_id.clone(),
            "文件数量异常".to_string(),
        );
        return Ok(());
    }

    let receive_dir = receive_dir_for_ingest();
    ft_append_diag_log(&format!(
        "[ft-recv] header_ok ver={ver} nfiles={nfiles} sender_id={sender_id} msg_len={} receive_dir={:?}",
        message.len(),
        receive_dir.as_ref().map(|p| p.display().to_string())
    ));

    // 先发 1 字节 ACK，发送端必须先读再发文件；避免「未设接收目录」时直接关连接导致对端写入 10054。
    let ack = if receive_dir.is_none() {
        ACK_NO_RECEIVE_DIR
    } else {
        ACK_OK
    };
    ft_append_diag_log(&format!("[ft-recv] sending_ack={ack}"));
    if let Err(e) = write_all(stream, &[ack]) {
        ft_append_diag_log(&format!(
            "[ft-recv] write_ack_failed ack={ack} err={}",
            e.message
        ));
        push_recv_err(String::new(), message.clone(), sender_id.clone(), e.message);
        return Ok(());
    }
    if let Err(e) = stream.flush() {
        ft_append_diag_log(&format!("[ft-recv] flush_ack_failed ack={ack} err={e}"));
        // flush 失败不放弃连接，继续处理
    }
    if ack != ACK_OK {
        push_recv_err(
            String::new(),
            message.clone(),
            sender_id.clone(),
            "未设置接收目录，无法保存文件".to_string(),
        );
        return Ok(());
    }

    let Some(dir) = receive_dir else {
        return Ok(());
    };

    ft_append_diag_log(&format!(
        "[ft-recv] recv_files_start nfiles={nfiles} peer_meta_next=read_name_len"
    ));
    for _ in 0..nfiles {
        let name_len = match read_u16(stream) {
            Ok(v) => v as usize,
            Err(e) => {
                ft_append_diag_log(&format!("[ft-recv] recv_name_len_fail err={}", e.message));
                push_recv_err(String::new(), message.clone(), sender_id.clone(), e.message);
                return Ok(());
            }
        };
        if name_len > MAX_NAME_BYTES {
            ft_append_diag_log(&format!(
                "[ft-recv] recv_name_len_bad name_len={name_len} max={MAX_NAME_BYTES}"
            ));
            push_recv_err(
                String::new(),
                message.clone(),
                sender_id.clone(),
                "文件名过长".to_string(),
            );
            return Ok(());
        }
        let mut nb = vec![0u8; name_len];
        if let Err(e) = read_exact(stream, &mut nb) {
            ft_append_diag_log(&format!(
                "[ft-recv] recv_file_name_bytes_fail len={name_len} err={}",
                e.message
            ));
            push_recv_err(String::new(), message.clone(), sender_id.clone(), e.message);
            return Ok(());
        }
        let raw_name = match String::from_utf8(nb) {
            Ok(s) => s,
            Err(_) => {
                ft_append_diag_log("[ft-recv] recv_file_name_utf8_fail");
                push_recv_err(
                    String::new(),
                    message.clone(),
                    sender_id.clone(),
                    "文件名非 UTF-8".to_string(),
                );
                return Ok(());
            }
        };
        let size = match read_u64(stream) {
            Ok(s) => s,
            Err(e) => {
                ft_append_diag_log(&format!(
                    "[ft-recv] recv_file_size_fail name={raw_name} err={}",
                    e.message
                ));
                push_recv_err(
                    raw_name.clone(),
                    message.clone(),
                    sender_id.clone(),
                    e.message,
                );
                return Ok(());
            }
        };

        let save_path = unique_save_path(&dir, &raw_name);
        ft_append_diag_log(&format!(
            "[ft-recv] recv_file_begin name={raw_name} bytes={size} save_path={}",
            save_path.display()
        ));
        let copy_result = copy_n_from_stream(stream, &save_path, size);
        match copy_result {
            Ok(()) => {
                ft_append_diag_log(&format!(
                    "[ft-recv] recv_file_done name={raw_name} bytes={size}"
                ));
                let saved = save_path.to_string_lossy().into_owned();
                with_state_mut(|s| {
                    push_receive_event(
                        s,
                        FileReceiveEventDto {
                            file_name: raw_name.clone(),
                            message: message.clone(),
                            sender_peer_id: sender_id.clone(),
                            saved_path: Some(saved),
                            error: None,
                            timestamp_ms: now_ms_pub(),
                        },
                    );
                });
            }
            Err(e) => {
                push_recv_err(raw_name, message.clone(), sender_id.clone(), e.message);
                return Ok(());
            }
        }
    }

    ft_append_diag_log("[ft-recv] all_files_ok");
    Ok(())
}

fn copy_n_from_stream(stream: &mut TcpStream, path: &Path, total: u64) -> Result<(), ApiError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            ft_append_diag_log(&format!(
                "[ft-recv] copy_mkdir_fail path={} err={e}",
                path.display()
            ));
            io_err("创建接收子目录", e)
        })?;
    }
    let mut out = File::create(path).map_err(|e| {
        ft_append_diag_log(&format!(
            "[ft-recv] copy_file_create_fail path={} err={e}",
            path.display()
        ));
        io_err("创建接收文件", e)
    })?;
    let mut remaining = total;
    let mut buf = vec![0u8; IO_BUF];
    let mut read_round: u64 = 0;
    let mut copied: u64 = 0;
    while remaining > 0 {
        let chunk = (remaining as usize).min(IO_BUF);
        read_round += 1;
        let n = stream.read(&mut buf[..chunk]).map_err(|e| {
            ft_append_diag_log(&format!(
                "[ft-recv] copy_read_fail path={} remaining={remaining} err={e}",
                path.display()
            ));
            io_err("接收文件数据", e)
        })?;
        if n == 0 {
            ft_append_diag_log(&format!(
                "[ft-recv] copy_eof path={} copied={copied} expected={total}",
                path.display()
            ));
            return Err(ApiError::new("FILE_IO", "对端提前结束，文件不完整"));
        }
        if read_round == 1 || remaining <= n as u64 {
            ft_append_diag_log(&format!(
                "[ft-recv] copy_chunk path={} round={read_round} n={n} remaining_before={remaining}",
                path.display()
            ));
        }
        out.write_all(&buf[..n]).map_err(|e| {
            ft_append_diag_log(&format!(
                "[ft-recv] copy_disk_write_fail path={} n={n} copied_so_far={copied} err={e}",
                path.display()
            ));
            io_err("写入接收文件", e)
        })?;
        remaining -= n as u64;
        copied += n as u64;
    }
    Ok(())
}
