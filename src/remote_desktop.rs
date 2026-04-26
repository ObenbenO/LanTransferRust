//! 远程桌面：XRDS 协议（单连接、握手 + 二进制帧）。
//! - 截屏：**xcap**（跨平台；内部 Windows 用 GDI/WGC 等，对标/替代已弃用的 scrap）。
//! - 键鼠：**enigo**。
//! - 传输：**裸 TCP** + `TCP_NODELAY`；局域网未使用 **quinn/QUIC**（避免复杂度，低延迟已足够）。
//! - 画面：**JPEG** 帧（`image` 编码）减小带宽与解码耗时；保留 `MSG_VIDEO` RGBA 解析以兼容旧端。

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use std::thread;
use std::time::Duration;

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{ExtendedColorType, ImageEncoder, RgbaImage};
use xcap::Monitor;

use crate::api::types::{ApiError, RemotePointerEventDto, VideoFrameDto};
use crate::app_state::{diag_log_path, FileServiceHandle};
use crate::remote_input::{encode_key_msg, encode_pointer_msg, HostInputProcessor};

pub const RD_MAGIC: &[u8; 4] = b"XRDS";
const RD_VERSION: u16 = 1;
const MSG_VIDEO: u8 = 16;
/// JPEG 压缩帧（局域网默认，体积小、解码快）。
const MSG_JPEG: u8 = 17;
const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;
const MAX_PAYLOAD: u32 = 4096;
/// 局域网适当提高上限以保留清晰度（JPEG 后单帧仍远小于 RGBA）。
const CAPTURE_MAX_W: u32 = 1920;
const JPEG_QUALITY: u8 = 78;
const FRAME_INTERVAL_MS: u64 = 33;

fn read_exact(s: &mut TcpStream, buf: &mut [u8]) -> std::io::Result<()> {
    let mut off = 0;
    while off < buf.len() {
        let n = s.read(&mut buf[off..])?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "eof",
            ));
        }
        off += n;
    }
    Ok(())
}

fn write_all(s: &mut TcpStream, mut data: &[u8]) -> std::io::Result<()> {
    while !data.is_empty() {
        let n = s.write(data)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "write zero",
            ));
        }
        data = &data[n..];
    }
    Ok(())
}

fn write_u32_le(s: &mut TcpStream, v: u32) -> std::io::Result<()> {
    write_all(s, &v.to_le_bytes())
}

/// 控制端：最新一帧（由读线程写入，`try_take` 取走）。
static RD_CLIENT_FRAME: Mutex<Option<VideoFrameDto>> = Mutex::new(None);

/// 成功解析并写入 [RD_CLIENT_FRAME] 的次数（调试：区分「只解析到 1 帧」与「UI 未显示」）。
static RD_CLIENT_FRAMES_PARSED_OK: AtomicU64 = AtomicU64::new(0);

/// 被控端收到的键鼠控制帧计数（仅用于诊断日志）。
static RD_HOST_CONTROL_RX: AtomicU64 = AtomicU64::new(0);

/// 诊断日志写入应用数据目录下的 `logs/<file_name>`（release 无控制台窗口，仅写文件）。
fn rd_append_diag_log(file_name: &str, line: &str) {
    let path = diag_log_path(file_name);
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "[{ts}] {line}");
    }
}

struct ActiveClient {
    shutdown: Arc<AtomicBool>,
    input_tx: mpsc::Sender<Vec<u8>>,
    writer_join: Option<thread::JoinHandle<()>>,
    reader_join: Option<thread::JoinHandle<()>>,
}

static ACTIVE_CLIENT: OnceLock<Mutex<Option<ActiveClient>>> = OnceLock::new();

fn active_client_slot() -> &'static Mutex<Option<ActiveClient>> {
    ACTIVE_CLIENT.get_or_init(|| Mutex::new(None))
}

fn write_framed_capped(
    stream: &mut TcpStream,
    payload: &[u8],
    max_len: u32,
) -> std::io::Result<()> {
    let len = payload.len() as u32;
    if len > max_len {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "payload too large",
        ));
    }
    write_u32_le(stream, len)?;
    write_all(stream, payload)?;
    Ok(())
}

fn write_framed(stream: &mut TcpStream, payload: &[u8]) -> std::io::Result<()> {
    write_framed_capped(stream, payload, MAX_PAYLOAD)
}

fn client_writer_loop(
    mut w: TcpStream,
    shutdown: Arc<AtomicBool>,
    rx: mpsc::Receiver<Vec<u8>>,
) {
    let _ = w.set_nodelay(true);
    while !shutdown.load(Ordering::SeqCst) {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(payload) => {
                if write_framed(&mut w, &payload).is_err() {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = w.shutdown(std::net::Shutdown::Both);
}

/// 控制端：发送指针事件（经当前 TCP 连接）。
pub fn client_send_pointer_bin(e: &RemotePointerEventDto) -> Result<(), ApiError> {
    let payload = encode_pointer_msg(e);
    let g = active_client_slot()
        .lock()
        .map_err(|_| ApiError::new("INTERNAL", "client mutex poisoned"))?;
    let Some(c) = g.as_ref() else {
        return Err(ApiError::new("CLIENT_NOT_CONNECTED", "控制端未连接"));
    };
    c.input_tx
        .send(payload)
        .map_err(|_| ApiError::new("RD_IO", "发送队列已关闭"))?;
    Ok(())
}

/// 控制端：发送按键。
pub fn client_send_key_bin(key_code: i32, down: bool, modifiers: i32) -> Result<(), ApiError> {
    let payload = encode_key_msg(key_code, down, modifiers);
    let g = active_client_slot()
        .lock()
        .map_err(|_| ApiError::new("INTERNAL", "client mutex poisoned"))?;
    let Some(c) = g.as_ref() else {
        return Err(ApiError::new("CLIENT_NOT_CONNECTED", "控制端未连接"));
    };
    c.input_tx
        .send(payload)
        .map_err(|_| ApiError::new("RD_IO", "发送队列已关闭"))?;
    Ok(())
}

fn parse_rgba_video_payload(p: &[u8]) -> Option<VideoFrameDto> {
    if p.len() < 12 {
        return None;
    }
    let w = u32::from_le_bytes(p[0..4].try_into().ok()?);
    let h = u32::from_le_bytes(p[4..8].try_into().ok()?);
    let len = u32::from_le_bytes(p[8..12].try_into().ok()?);
    let need = len as usize;
    if p.len() < 12 + need {
        return None;
    }
    if w == 0 || h == 0 {
        return None;
    }
    if need != (w as usize).saturating_mul(h as usize).saturating_mul(4) {
        return None;
    }
    Some(VideoFrameDto {
        width: w,
        height: h,
        rgba: p[12..12 + need].to_vec(),
    })
}

fn parse_jpeg_video_payload(p: &[u8]) -> Option<VideoFrameDto> {
    if p.len() < 12 {
        return None;
    }
    let w = u32::from_le_bytes(p[0..4].try_into().ok()?);
    let h = u32::from_le_bytes(p[4..8].try_into().ok()?);
    let jl = u32::from_le_bytes(p[8..12].try_into().ok()?);
    let jlen = jl as usize;
    if w == 0 || h == 0 || p.len() < 12 + jlen {
        return None;
    }
    Some(VideoFrameDto {
        width: w,
        height: h,
        rgba: p[12..12 + jlen].to_vec(),
    })
}

fn parse_frame_payload(payload: &[u8]) -> Option<VideoFrameDto> {
    let tag = *payload.first()?;
    match tag {
        MSG_VIDEO if payload.len() > 1 => parse_rgba_video_payload(&payload[1..]),
        MSG_JPEG if payload.len() > 1 => parse_jpeg_video_payload(&payload[1..]),
        _ => None,
    }
}

fn encode_jpeg_from_rgba_reuse(
    img: &RgbaImage,
    quality: u8,
    rgb_buf: &mut Vec<u8>,
    out: &mut Vec<u8>,
) -> Result<(), ApiError> {
    let w = img.width();
    let h = img.height();
    let need = (w as usize)
        .saturating_mul(h as usize)
        .saturating_mul(3);
    if rgb_buf.len() != need {
        rgb_buf.resize(need, 0);
    }
    let rgba = img.as_raw();
    let mut j = 0usize;
    for px in rgba.chunks_exact(4) {
        rgb_buf[j] = px[0];
        rgb_buf[j + 1] = px[1];
        rgb_buf[j + 2] = px[2];
        j += 3;
    }

    out.clear();
    let enc = JpegEncoder::new_with_quality(out, quality);
    enc.write_image(rgb_buf.as_slice(), w, h, ExtendedColorType::Rgb8)
        .map_err(|e| ApiError::new("RD_JPEG", format!("JPEG 编码失败: {e}")))?;
    Ok(())
}

fn client_reader_loop(mut r: TcpStream, shutdown: Arc<AtomicBool>) {
    let mut hdr = [0u8; 4];
    while !shutdown.load(Ordering::SeqCst) {
        // 仅等待帧头：间隔由被控端 sleep 决定，放宽超时避免误杀连接。
        r.set_read_timeout(Some(Duration::from_secs(60))).ok();
        match r.read_exact(&mut hdr) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(_) => break,
        }
        let len = u32::from_le_bytes(hdr);
        if len == 0 || len > MAX_FRAME_BYTES {
            break;
        }
        // 单帧可达数 MB：短 read_timeout 会导致 read_exact 在 Windows 上中途 TimedOut 并退出读线程。
        r.set_read_timeout(None).ok();
        let mut buf = vec![0u8; len as usize];
        let body = read_exact(&mut r, &mut buf);
        r.set_read_timeout(Some(Duration::from_secs(60))).ok();
        if body.is_err() {
            break;
        }
        if let Some(frame) = parse_frame_payload(&buf) {
            let n = RD_CLIENT_FRAMES_PARSED_OK.fetch_add(1, Ordering::Relaxed) + 1;
            if n == 1 || n.is_multiple_of(60) {
                rd_append_diag_log(
                    "lan_transfer_rd_client.log",
                    &format!(
                        "[rd-client] video frames parsed (ok): {n} (~{}ms host interval; log file in %TEMP%)",
                        FRAME_INTERVAL_MS
                    ),
                );
            }
            if let Ok(mut g) = RD_CLIENT_FRAME.lock() {
                *g = Some(frame);
            }
        }
    }
}

/// 建立 XRDS 会话：握手后启动读屏线程，返回监听端口与句柄。
pub fn spawn_remote_desktop_listener(
    preferred_port: Option<u16>,
    expected_token: String,
) -> Result<(u16, FileServiceHandle), ApiError> {
    let listener = match preferred_port {
        Some(p) => TcpListener::bind(("0.0.0.0", p)),
        None => TcpListener::bind(("0.0.0.0", 0)),
    }
    .map_err(|e| ApiError::new("RD_BIND_FAILED", format!("远程桌面端口绑定失败: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| ApiError::new("RD_BIND_FAILED", format!("{e}")))?
        .port();

    listener
        .set_nonblocking(true)
        .map_err(|e| ApiError::new("RD_LISTENER_IO", format!("set_nonblocking: {e}")))?;

    let shutdown = Arc::new(AtomicBool::new(false));
    let sd = Arc::clone(&shutdown);
    let token = expected_token;
    let join = thread::spawn(move || {
        while !sd.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let tok = token.clone();
                    thread::spawn(move || {
                        let _ = run_host_session(stream, tok);
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(50));
                }
                Err(_) => break,
            }
        }
    });

    Ok((port, FileServiceHandle::new(shutdown, join)))
}

fn read_handshake_token(s: &mut TcpStream) -> std::io::Result<String> {
    let mut magic = [0u8; 4];
    read_exact(s, &mut magic)?;
    if &magic != RD_MAGIC {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "bad magic",
        ));
    }
    let mut ver = [0u8; 2];
    read_exact(s, &mut ver)?;
    if u16::from_le_bytes(ver) != RD_VERSION {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "bad version",
        ));
    }
    let mut tl = [0u8; 2];
    read_exact(s, &mut tl)?;
    let tlen = u16::from_le_bytes(tl) as usize;
    if tlen > 2048 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "token len",
        ));
    }
    let mut tb = vec![0u8; tlen];
    read_exact(s, &mut tb)?;
    String::from_utf8(tb)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "token utf8"))
}

fn scale_capture(img: RgbaImage) -> RgbaImage {
    let (w, h) = (img.width(), img.height());
    if w <= CAPTURE_MAX_W {
        return img;
    }
    let nw = CAPTURE_MAX_W;
    let nh = ((h as u64 * nw as u64) / w as u64).max(1) as u32;
    image::imageops::resize(&img, nw, nh, FilterType::Triangle)
}

fn capture_primary_rgba() -> Result<(RgbaImage, i32, i32), ApiError> {
    let mons =
        Monitor::all().map_err(|e| ApiError::new("RD_CAPTURE", format!("枚举显示器失败: {e}")))?;
    if mons.is_empty() {
        return Err(ApiError::new("RD_CAPTURE", "未找到可用显示器"));
    }
    let m = mons
        .iter()
        .find(|mo| mo.is_primary().unwrap_or(false))
        .unwrap_or(&mons[0]);
    let rw = m
        .width()
        .map_err(|e| ApiError::new("RD_CAPTURE", format!("{e}")))?;
    let rh = m
        .height()
        .map_err(|e| ApiError::new("RD_CAPTURE", format!("{e}")))?;
    let img = m
        .capture_image()
        .map_err(|e| ApiError::new("RD_CAPTURE", format!("截屏失败: {e}")))?;
    let rgba = scale_capture(img);
    Ok((rgba, rw as i32, rh as i32))
}

/// 在推流循环内从**同一条** [TcpStream] 非阻塞拉取控制端上行数据并组帧。
/// Windows 上单独 `try_clone` 读线程在部分环境下收不到对端写入（控制端已 write 成功、被控 clone 上无 rx），
/// 故改为与发送视频共用主 socket、交错读。
fn host_drain_client_control_on_stream(
    stream: &mut TcpStream,
    acc: &mut Vec<u8>,
    input: &mut Option<HostInputProcessor>,
) -> Result<bool, ApiError> {
    stream
        .set_nonblocking(true)
        .map_err(|e| ApiError::new("RD_IO", format!("set_nonblocking: {e}")))?;
    let mut tmp = [0u8; 4096];
    let mut peer_closed = false;
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => {
                peer_closed = true;
                break;
            }
            Ok(n) => acc.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                let _ = stream.set_nonblocking(false);
                return Err(ApiError::new("RD_IO", format!("drain control read: {e}")));
            }
        }
    }
    stream
        .set_nonblocking(false)
        .map_err(|e| ApiError::new("RD_IO", format!("clear nonblocking: {e}")))?;

    loop {
        if acc.len() < 4 {
            break;
        }
        let len = u32::from_le_bytes(acc[..4].try_into().unwrap()) as usize;
        if len == 0 || len > MAX_PAYLOAD as usize {
            rd_append_diag_log(
                "lan_transfer_rd_host.log",
                &format!(
                    "[rd-host] 异常控制帧长度 {len}（max={MAX_PAYLOAD}） acc_len={}",
                    acc.len()
                ),
            );
            return Err(ApiError::new("RD_PROTO", "控制帧长度非法"));
        }
        if acc.len() < 4 + len {
            break;
        }
        let frame = acc[4..4 + len].to_vec();
        acc.drain(..4 + len);

        let n = RD_HOST_CONTROL_RX.fetch_add(1, Ordering::Relaxed) + 1;
        if n == 1 || n.is_multiple_of(40) {
            rd_append_diag_log(
                "lan_transfer_rd_host.log",
                &format!(
                    "[rd-host] rx control #{n} len={} tag={}",
                    frame.len(),
                    frame.first().copied().unwrap_or(0)
                ),
            );
        }
        if let Some(ref mut e) = input {
            if let Err(err) = e.dispatch_framed_payload(&frame) {
                rd_append_diag_log(
                    "lan_transfer_rd_host.log",
                    &format!("[rd-host] 处理键鼠包失败: {} — {}", err.code, err.message),
                );
            }
        }
    }

    Ok(!peer_closed)
}

fn run_host_session(mut stream: TcpStream, expected_token: String) -> Result<(), ApiError> {
    stream.set_nodelay(true).ok();
    let token = read_handshake_token(&mut stream)
        .map_err(|e| ApiError::new("RD_HANDSHAKE", format!("读握手失败: {e}")))?;
    if token != expected_token {
        let _ = stream.write_all(&[1u8]);
        return Err(ApiError::new("RD_AUTH", "令牌不匹配"));
    }
    stream
        .write_all(&[0u8])
        .map_err(|e| ApiError::new("RD_HANDSHAKE", format!("写握手应答: {e}")))?;

    let log_path = diag_log_path("lan_transfer_rd_host.log");
    rd_append_diag_log(
        "lan_transfer_rd_host.log",
        &format!(
            "[rd-host] 会话开始；控制帧与视频共用主 TcpStream 非阻塞拉取（见 {}）",
            log_path.display()
        ),
    );

    let (rgba, sw, sh) = capture_primary_rgba()?;
    let fw = rgba.width() as f64;
    let fh = rgba.height() as f64;

    let mut input = match HostInputProcessor::new(fw, fh, sw, sh) {
        Ok(x) => Some(x),
        Err(e) => {
            rd_append_diag_log(
                "lan_transfer_rd_host.log",
                &format!("[rd-host] 输入注入初始化失败，仅收包不注入: {} — {}", e.code, e.message),
            );
            None
        }
    };

    let mut inbound_acc: Vec<u8> = Vec::new();
    let mut rgb_buf: Vec<u8> = Vec::new();
    let mut jpeg_buf: Vec<u8> = Vec::new();

    loop {
        let tick = Instant::now();
        match host_drain_client_control_on_stream(
            &mut stream,
            &mut inbound_acc,
            &mut input,
        ) {
            Ok(true) => {}
            Ok(false) => break,
            Err(e) => {
                rd_append_diag_log(
                    "lan_transfer_rd_host.log",
                    &format!("[rd-host] drain 结束: {} — {}", e.code, e.message),
                );
                break;
            }
        }

        let (rgba, _, _) = match capture_primary_rgba() {
            Ok(x) => x,
            Err(_) => {
                thread::sleep(Duration::from_millis(200));
                continue;
            }
        };
        let w = rgba.width();
        let h = rgba.height();
        if encode_jpeg_from_rgba_reuse(&rgba, JPEG_QUALITY, &mut rgb_buf, &mut jpeg_buf).is_err() {
            thread::sleep(Duration::from_millis(FRAME_INTERVAL_MS));
            continue;
        }
        let mut payload = Vec::with_capacity(1 + 12 + jpeg_buf.len());
        payload.push(MSG_JPEG);
        payload.extend_from_slice(&w.to_le_bytes());
        payload.extend_from_slice(&h.to_le_bytes());
        payload.extend_from_slice(&(jpeg_buf.len() as u32).to_le_bytes());
        payload.extend_from_slice(&jpeg_buf);
        if write_framed_capped(&mut stream, &payload, MAX_FRAME_BYTES).is_err() {
            break;
        }
        let frame_budget = Duration::from_millis(FRAME_INTERVAL_MS);
        let elapsed = tick.elapsed();
        if elapsed < frame_budget {
            thread::sleep(frame_budget - elapsed);
        }
    }

    let _ = stream.shutdown(std::net::Shutdown::Both);
    drop(stream);
    Ok(())
}

/// 控制端：连接 XRDS 并启动读线程。
pub fn client_connect(host: &str, port: u16, session_token: &str) -> Result<(), ApiError> {
    {
        let g = active_client_slot()
            .lock()
            .map_err(|_| ApiError::new("INTERNAL", "client mutex poisoned"))?;
        if g.is_some() {
            return Err(ApiError::new("CLIENT_BUSY", "已有远程连接，请先断开"));
        }
    }

    let addr = format!("{host}:{port}");
    let mut stream = TcpStream::connect(&addr)
        .map_err(|e| ApiError::new("TCP_CONNECT_FAILED", format!("无法连接 {addr}: {e}")))?;
    stream.set_nodelay(true).ok();

    let tok = session_token.as_bytes();
    if tok.len() > 2048 {
        return Err(ApiError::new("INVALID_TOKEN", "token 过长"));
    }
    let mut hs = Vec::with_capacity(4 + 2 + 2 + tok.len());
    hs.extend_from_slice(RD_MAGIC);
    hs.extend_from_slice(&RD_VERSION.to_le_bytes());
    hs.extend_from_slice(&(tok.len() as u16).to_le_bytes());
    hs.extend_from_slice(tok);
    write_all(&mut stream, &hs)
        .map_err(|e| ApiError::new("RD_HANDSHAKE", format!("写握手: {e}")))?;

    let mut status = [0u8; 1];
    read_exact(&mut stream, &mut status)
        .map_err(|e| ApiError::new("RD_HANDSHAKE", format!("读握手结果: {e}")))?;
    if status[0] != 0 {
        return Err(ApiError::new("RD_AUTH", "被控端拒绝连接（请核对会话令牌）"));
    }

    let reader = stream
        .try_clone()
        .map_err(|e| ApiError::new("RD_IO", format!("clone: {e}")))?;
    let shutdown = Arc::new(AtomicBool::new(false));

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let sd_w = Arc::clone(&shutdown);
    let writer_join = thread::spawn(move || {
        client_writer_loop(stream, sd_w, rx);
    });

    let sd_r = Arc::clone(&shutdown);
    let reader_join = thread::spawn(move || {
        client_reader_loop(reader, sd_r);
    });

    {
        let mut g = active_client_slot()
            .lock()
            .map_err(|_| ApiError::new("INTERNAL", "client mutex poisoned"))?;
        *g = Some(ActiveClient {
            shutdown,
            input_tx: tx,
            writer_join: Some(writer_join),
            reader_join: Some(reader_join),
        });
    }

    Ok(())
}

/// 控制端：断开并清理。
pub fn client_disconnect() {
    let taken = {
        let mut g = match active_client_slot().lock() {
            Ok(x) => x,
            Err(_) => return,
        };
        g.take()
    };
    if let Some(c) = taken {
        c.shutdown.store(true, Ordering::SeqCst);
        if let Some(j) = c.writer_join {
            let _ = j.join();
        }
        if let Some(j) = c.reader_join {
            let _ = j.join();
        }
    }
    if let Ok(mut f) = RD_CLIENT_FRAME.lock() {
        *f = None;
    }
}

/// 取走最新一帧（非阻塞）。
pub fn client_try_take_frame() -> Option<VideoFrameDto> {
    RD_CLIENT_FRAME.lock().ok().and_then(|mut g| g.take())
}
