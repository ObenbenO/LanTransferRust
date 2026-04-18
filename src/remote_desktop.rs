//! 远程桌面：XRDS 协议（单连接、握手 + 二进制帧）。
//! - 截屏：**xcap**（跨平台；内部 Windows 用 GDI/WGC 等，对标/替代已弃用的 scrap）。
//! - 键鼠：**enigo**。
//! - 传输：**裸 TCP** + `TCP_NODELAY`；局域网未使用 **quinn/QUIC**（避免复杂度，低延迟已足够）。
//! - 画面：**JPEG** 帧（`image` 编码）减小带宽与解码耗时；保留 `MSG_VIDEO` RGBA 解析以兼容旧端。

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// 修饰键掩码（与 `RemoteKeyEventDto.modifiers` 一致）。
const MOD_SHIFT: i32 = 1;
const MOD_CTRL: i32 = 2;
const MOD_ALT: i32 = 4;
const MOD_META: i32 = 8;
const MOD_MASK: i32 = MOD_SHIFT | MOD_CTRL | MOD_ALT | MOD_META;

/// 控制端协议：左/右 Control、Shift、Alt、Meta 共用同一编码，由被控端映射到 enigo 单键。
const RD_KEY_CONTROL: i32 = -113;
const RD_KEY_SHIFT: i32 = -114;
const RD_KEY_ALT: i32 = -115;
const RD_KEY_META: i32 = -116;

/// 被控端：当前「逻辑上」已按下的修饰键（与 enigo 同步）。
static RD_HOST_MOD_STATE: Mutex<i32> = Mutex::new(0);
use std::thread;
use std::time::Duration;

use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ExtendedColorType, ImageEncoder, RgbaImage};
use xcap::Monitor;

use crate::api::types::{ApiError, RemotePointerEventDto, VideoFrameDto};
use crate::app_state::{diag_log_path, FileServiceHandle};

pub const RD_MAGIC: &[u8; 4] = b"XRDS";
const RD_VERSION: u16 = 1;
const MSG_POINTER: u8 = 1;
const MSG_KEY: u8 = 2;
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
    writer: Mutex<TcpStream>,
    reader_join: Option<thread::JoinHandle<()>>,
}

static ACTIVE_CLIENT: OnceLock<Mutex<Option<ActiveClient>>> = OnceLock::new();

fn active_client_slot() -> &'static Mutex<Option<ActiveClient>> {
    ACTIVE_CLIENT.get_or_init(|| Mutex::new(None))
}

fn pointer_kind_byte(kind: &str) -> u8 {
    match kind {
        "down" => 1,
        "up" => 2,
        "scroll" => 3,
        _ => 0,
    }
}

pub fn encode_pointer_msg(e: &RemotePointerEventDto) -> Vec<u8> {
    let k = pointer_kind_byte(&e.kind);
    let mut v = Vec::with_capacity(34);
    v.push(MSG_POINTER);
    v.push(k);
    v.extend_from_slice(&e.x.to_le_bytes());
    v.extend_from_slice(&e.y.to_le_bytes());
    v.extend_from_slice(&e.button.to_le_bytes());
    v.extend_from_slice(&e.delta.to_le_bytes());
    v.extend_from_slice(&e.modifiers.to_le_bytes());
    v
}

pub fn encode_key_msg(key_code: i32, down: bool, modifiers: i32) -> Vec<u8> {
    let mut v = Vec::with_capacity(14);
    v.push(MSG_KEY);
    v.extend_from_slice(&key_code.to_le_bytes());
    v.push(if down { 1 } else { 0 });
    v.extend_from_slice(&modifiers.to_le_bytes());
    v
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

/// 控制端：发送指针事件（经当前 TCP 连接）。
pub fn client_send_pointer_bin(e: &RemotePointerEventDto) -> Result<(), ApiError> {
    let payload = encode_pointer_msg(e);
    let g = active_client_slot()
        .lock()
        .map_err(|_| ApiError::new("INTERNAL", "client mutex poisoned"))?;
    let Some(c) = g.as_ref() else {
        return Err(ApiError::new("CLIENT_NOT_CONNECTED", "控制端未连接"));
    };
    let mut w = c
        .writer
        .lock()
        .map_err(|_| ApiError::new("INTERNAL", "writer mutex poisoned"))?;
    write_framed(&mut w, &payload)
        .map_err(|e| ApiError::new("RD_IO", format!("发送指针失败: {e}")))?;
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
    let mut w = c
        .writer
        .lock()
        .map_err(|_| ApiError::new("INTERNAL", "writer mutex poisoned"))?;
    write_framed(&mut w, &payload)
        .map_err(|e| ApiError::new("RD_IO", format!("发送按键失败: {e}")))?;
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

fn encode_jpeg_from_rgba(img: &RgbaImage, quality: u8) -> Result<Vec<u8>, ApiError> {
    // image 0.25 的 JpegEncoder 仅支持 L8 / Rgb8，Rgba8 会直接 Unsupported。
    let rgb = DynamicImage::ImageRgba8(img.clone()).into_rgb8();
    let w = rgb.width();
    let h = rgb.height();
    let mut out = Vec::new();
    let enc = JpegEncoder::new_with_quality(&mut out, quality);
    enc.write_image(rgb.as_raw(), w, h, ExtendedColorType::Rgb8)
        .map_err(|e| ApiError::new("RD_JPEG", format!("JPEG 编码失败: {e}")))?;
    Ok(out)
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

fn map_pointer_to_screen(x: f64, y: f64, fw: f64, fh: f64, sw: i32, sh: i32) -> (i32, i32) {
    if fw <= 0.0 || fh <= 0.0 {
        return (0, 0);
    }
    let sx = (x / fw * sw as f64).round() as i32;
    let sy = (y / fh * sh as f64).round() as i32;
    (
        sx.clamp(0, sw.saturating_sub(1).max(0)),
        sy.clamp(0, sh.saturating_sub(1).max(0)),
    )
}

fn modifier_bit_to_enigo_key(bit: i32) -> Option<Key> {
    Some(match bit {
        x if x == MOD_SHIFT => Key::Shift,
        x if x == MOD_CTRL => Key::Control,
        x if x == MOD_ALT => Key::Alt,
        x if x == MOD_META => Key::Meta,
        _ => return None,
    })
}

fn sync_modifiers_from_mask(enigo: &mut Enigo, target: i32) -> Result<(), ApiError> {
    let target = target & MOD_MASK;
    let mut g = RD_HOST_MOD_STATE
        .lock()
        .map_err(|_| ApiError::new("INTERNAL", "修饰键状态锁"))?;
    let current = *g & MOD_MASK;
    if current == target {
        return Ok(());
    }
    // 先松开：高位优先，与常见「先松后按」顺序一致。
    for bit in [MOD_META, MOD_ALT, MOD_CTRL, MOD_SHIFT] {
        if (current & bit) != 0 && (target & bit) == 0 {
            if let Some(k) = modifier_bit_to_enigo_key(bit) {
                enigo
                    .key(k, Direction::Release)
                    .map_err(|e| ApiError::new("RD_INPUT", format!("修饰键松开: {e}")))?;
            }
        }
    }
    // 再按下：Ctrl → Shift → Alt → Meta（与 Ctrl+Shift 组合常见顺序一致）。
    for bit in [MOD_CTRL, MOD_SHIFT, MOD_ALT, MOD_META] {
        if (target & bit) != 0 && (current & bit) == 0 {
            if let Some(k) = modifier_bit_to_enigo_key(bit) {
                enigo
                    .key(k, Direction::Press)
                    .map_err(|e| ApiError::new("RD_INPUT", format!("修饰键按下: {e}")))?;
            }
        }
    }
    *g = (*g & !MOD_MASK) | target;
    Ok(())
}

fn apply_modifier_key_event(enigo: &mut Enigo, key_code: i32, down: bool) -> Result<(), ApiError> {
    let (bit, k) = match key_code {
        RD_KEY_CONTROL => (MOD_CTRL, Key::Control),
        RD_KEY_SHIFT => (MOD_SHIFT, Key::Shift),
        RD_KEY_ALT => (MOD_ALT, Key::Alt),
        RD_KEY_META => (MOD_META, Key::Meta),
        _ => return Ok(()),
    };
    let dir = if down {
        Direction::Press
    } else {
        Direction::Release
    };
    enigo
        .key(k, dir)
        .map_err(|e| ApiError::new("RD_INPUT", format!("修饰键: {e}")))?;
    let mut g = RD_HOST_MOD_STATE
        .lock()
        .map_err(|_| ApiError::new("INTERNAL", "修饰键状态锁"))?;
    if down {
        *g |= bit;
    } else {
        *g &= !bit;
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn unicode_to_enigo_key(ch: char) -> Option<Key> {
    match ch {
        'a' | 'A' => Some(Key::A),
        'b' | 'B' => Some(Key::B),
        'c' | 'C' => Some(Key::C),
        'd' | 'D' => Some(Key::D),
        'e' | 'E' => Some(Key::E),
        'f' | 'F' => Some(Key::F),
        'g' | 'G' => Some(Key::G),
        'h' | 'H' => Some(Key::H),
        'i' | 'I' => Some(Key::I),
        'j' | 'J' => Some(Key::J),
        'k' | 'K' => Some(Key::K),
        'l' | 'L' => Some(Key::L),
        'm' | 'M' => Some(Key::M),
        'n' | 'N' => Some(Key::N),
        'o' | 'O' => Some(Key::O),
        'p' | 'P' => Some(Key::P),
        'q' | 'Q' => Some(Key::Q),
        'r' | 'R' => Some(Key::R),
        's' | 'S' => Some(Key::S),
        't' | 'T' => Some(Key::T),
        'u' | 'U' => Some(Key::U),
        'v' | 'V' => Some(Key::V),
        'w' | 'W' => Some(Key::W),
        'x' | 'X' => Some(Key::X),
        'y' | 'Y' => Some(Key::Y),
        'z' | 'Z' => Some(Key::Z),
        '0' => Some(Key::Num0),
        '1' => Some(Key::Num1),
        '2' => Some(Key::Num2),
        '3' => Some(Key::Num3),
        '4' => Some(Key::Num4),
        '5' => Some(Key::Num5),
        '6' => Some(Key::Num6),
        '7' => Some(Key::Num7),
        '8' => Some(Key::Num8),
        '9' => Some(Key::Num9),
        _ => None,
    }
}

#[cfg(not(target_os = "windows"))]
fn unicode_to_enigo_key(ch: char) -> Option<Key> {
    if ch.is_ascii() && !ch.is_control() {
        Some(Key::Unicode(ch))
    } else {
        None
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_pointer(
    enigo: &mut Enigo,
    kind: u8,
    x: f64,
    y: f64,
    button: i32,
    delta: f64,
    modifiers: i32,
    fw: f64,
    fh: f64,
    sw: i32,
    sh: i32,
) -> Result<(), ApiError> {
    sync_modifiers_from_mask(enigo, modifiers)?;
    let (mx, my) = map_pointer_to_screen(x, y, fw, fh, sw, sh);
    enigo
        .move_mouse(mx, my, Coordinate::Abs)
        .map_err(|e| ApiError::new("RD_INPUT", format!("鼠标移动: {e}")))?;
    match kind {
        1 => {
            let b = if button == 2 {
                Button::Right
            } else {
                Button::Left
            };
            enigo
                .button(b, Direction::Press)
                .map_err(|e| ApiError::new("RD_INPUT", format!("鼠标按下: {e}")))?;
        }
        2 => {
            let b = if button == 2 {
                Button::Right
            } else {
                Button::Left
            };
            enigo
                .button(b, Direction::Release)
                .map_err(|e| ApiError::new("RD_INPUT", format!("鼠标松开: {e}")))?;
        }
        3 => {
            let lines = delta.clamp(-32.0, 32.0) as i32;
            enigo
                .scroll(lines, Axis::Vertical)
                .map_err(|e| ApiError::new("RD_INPUT", format!("滚轮: {e}")))?;
        }
        _ => {}
    }
    Ok(())
}

fn rd_key_to_enigo(key_code: i32) -> Option<Key> {
    Some(match key_code {
        -100 => Key::LeftArrow,
        -101 => Key::RightArrow,
        -102 => Key::UpArrow,
        -103 => Key::DownArrow,
        -104 => Key::Return,
        -105 => Key::Backspace,
        -106 => Key::Escape,
        -107 => Key::Delete,
        -108 => Key::Tab,
        -109 => Key::Home,
        -110 => Key::End,
        -111 => Key::PageUp,
        -112 => Key::PageDown,
        _ => return None,
    })
}

fn apply_key(enigo: &mut Enigo, key_code: i32, down: bool, modifiers: i32) -> Result<(), ApiError> {
    let dir = if down {
        Direction::Press
    } else {
        Direction::Release
    };

    if matches!(
        key_code,
        RD_KEY_CONTROL | RD_KEY_SHIFT | RD_KEY_ALT | RD_KEY_META
    ) {
        return apply_modifier_key_event(enigo, key_code, down);
    }

    sync_modifiers_from_mask(enigo, modifiers)?;

    if let Some(k) = rd_key_to_enigo(key_code) {
        enigo
            .key(k, dir)
            .map_err(|e| ApiError::new("RD_INPUT", format!("按键: {e}")))?;
        return Ok(());
    }

    if key_code > 0 && key_code < 0x110000 {
        if let Some(ch) = char::from_u32(key_code as u32) {
            if !ch.is_control() || ch == '\n' || ch == '\r' || ch == '\t' {
                if modifiers != 0 {
                    if let Some(k) = unicode_to_enigo_key(ch) {
                        enigo
                            .key(k, dir)
                            .map_err(|e| ApiError::new("RD_INPUT", format!("组合键: {e}")))?;
                    } else {
                        enigo
                            .key(Key::Unicode(ch), dir)
                            .map_err(|e| ApiError::new("RD_INPUT", format!("组合键: {e}")))?;
                    }
                } else if down {
                    let s = ch.to_string();
                    enigo
                        .text(&s)
                        .map_err(|e| ApiError::new("RD_INPUT", format!("字符输入: {e}")))?;
                }
            }
        }
    }
    Ok(())
}

fn dispatch_client_payload(
    enigo: &mut Enigo,
    buf: &[u8],
    fw: f64,
    fh: f64,
    sw: i32,
    sh: i32,
) -> Result<(), ApiError> {
    if buf.is_empty() {
        return Ok(());
    }
    match buf[0] {
        MSG_POINTER if buf.len() >= 30 => {
            let kind = buf[1];
            let x = f64::from_le_bytes(buf[2..10].try_into().unwrap());
            let y = f64::from_le_bytes(buf[10..18].try_into().unwrap());
            let button = i32::from_le_bytes(buf[18..22].try_into().unwrap());
            let delta = f64::from_le_bytes(buf[22..30].try_into().unwrap());
            let modifiers = if buf.len() >= 34 {
                i32::from_le_bytes(buf[30..34].try_into().unwrap())
            } else {
                0
            };
            apply_pointer(enigo, kind, x, y, button, delta, modifiers, fw, fh, sw, sh)
        }
        MSG_KEY if buf.len() >= 10 => {
            let key_code = i32::from_le_bytes(buf[1..5].try_into().unwrap());
            let down = buf[5] != 0;
            let modifiers = i32::from_le_bytes(buf[6..10].try_into().unwrap());
            apply_key(enigo, key_code, down, modifiers)
        }
        _ => Ok(()),
    }
}

/// 在推流循环内从**同一条** [TcpStream] 非阻塞拉取控制端上行数据并组帧。
/// Windows 上单独 `try_clone` 读线程在部分环境下收不到对端写入（控制端已 write 成功、被控 clone 上无 rx），
/// 故改为与发送视频共用主 socket、交错读。
fn host_drain_client_control_on_stream(
    stream: &mut TcpStream,
    acc: &mut Vec<u8>,
    enigo: &mut Option<Enigo>,
    fw: f64,
    fh: f64,
    sw: i32,
    sh: i32,
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
        if let Some(ref mut e) = enigo {
            if let Err(err) = dispatch_client_payload(e, &frame, fw, fh, sw, sh) {
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

    let mut enigo = match Enigo::new(&Settings::default()) {
        Ok(e) => Some(e),
        Err(e) => {
            rd_append_diag_log(
                "lan_transfer_rd_host.log",
                &format!("[rd-host] Enigo::new 失败，仅收包不注入: {e:?}"),
            );
            None
        }
    };

    if let Ok(mut g) = RD_HOST_MOD_STATE.lock() {
        *g = 0;
    }

    let mut inbound_acc: Vec<u8> = Vec::new();

    loop {
        match host_drain_client_control_on_stream(
            &mut stream,
            &mut inbound_acc,
            &mut enigo,
            fw,
            fh,
            sw,
            sh,
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
        let jpeg = match encode_jpeg_from_rgba(&rgba, JPEG_QUALITY) {
            Ok(j) => j,
            Err(_) => {
                thread::sleep(Duration::from_millis(FRAME_INTERVAL_MS));
                continue;
            }
        };
        let mut payload = Vec::with_capacity(1 + 12 + jpeg.len());
        payload.push(MSG_JPEG);
        payload.extend_from_slice(&w.to_le_bytes());
        payload.extend_from_slice(&h.to_le_bytes());
        payload.extend_from_slice(&(jpeg.len() as u32).to_le_bytes());
        payload.extend_from_slice(&jpeg);
        if write_framed_capped(&mut stream, &payload, MAX_FRAME_BYTES).is_err() {
            break;
        }
        thread::sleep(Duration::from_millis(FRAME_INTERVAL_MS));
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
    let sd_r = Arc::clone(&shutdown);
    let join = thread::spawn(move || {
        client_reader_loop(reader, sd_r);
    });

    {
        let mut g = active_client_slot()
            .lock()
            .map_err(|_| ApiError::new("INTERNAL", "client mutex poisoned"))?;
        *g = Some(ActiveClient {
            shutdown,
            writer: Mutex::new(stream),
            reader_join: Some(join),
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
