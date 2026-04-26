#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use chrono::{Local, TimeZone};
use eframe::egui;
use eframe::egui::{ColorImage, TextureHandle, TextureOptions};
use get_if_addrs::{get_if_addrs, IfAddr};
use lan_transfer::api;
use lan_transfer::api::types::{
    FileReceiveEventDto, LocalProfileDto, PeerInfoDto, RemoteKeyEventDto, RemotePointerEventDto,
    SendFilesRequestDto, TransferProgressDto, VideoFrameDto,
};
use rfd::FileDialog;

const LAN_PORT: u16 = 45678;
const LAN_MAGIC: &[u8; 4] = b"XTR1";
const LAN_REMOTE_SESSION_TOKEN: &str = "xtransfer-lan";

fn try_load_system_cjk_font_bytes() -> Option<Vec<u8>> {
    let windir = std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into());
    let font_dir = PathBuf::from(windir).join("Fonts");
    let candidates = [
        "NotoSansSC-VF.ttf",
        "NotoSerifSC-VF.ttf",
        "simhei.ttf",
        "Deng.ttf",
        "simsunb.ttf",
        "SimsunExtG.ttf",
        "HYZhongHeiTi-197.ttf",
    ];
    for name in candidates {
        let p = font_dir.join(name);
        if let Ok(b) = std::fs::read(&p) {
            if !b.is_empty() {
                return Some(b);
            }
        }
    }
    None
}

fn configure_fonts(ctx: &egui::Context) {
    let mut defs = egui::FontDefinitions::default();
    if let Some(bytes) = try_load_system_cjk_font_bytes() {
        defs.font_data
            .insert("cjk".to_string(), egui::FontData::from_owned(bytes).into());
        if let Some(p) = defs.families.get_mut(&egui::FontFamily::Proportional) {
            p.insert(0, "cjk".to_string());
        }
        if let Some(m) = defs.families.get_mut(&egui::FontFamily::Monospace) {
            m.insert(0, "cjk".to_string());
        }
    }
    ctx.set_fonts(defs);
}

fn is_private_ipv4(a: Ipv4Addr) -> bool {
    let o = a.octets();
    matches!(o, [10, _, _, _] | [172, 16..=31, _, _] | [192, 168, _, _])
}

fn bind_preference(a: Ipv4Addr) -> i32 {
    let o = a.octets();
    if o[0] == 192 && o[1] == 168 {
        return 0;
    }
    if o[0] == 10 {
        return 1;
    }
    if o[0] == 172 && o[1] == 17 {
        return 90;
    }
    if o[0] == 172 && (16..=31).contains(&o[1]) {
        return 50;
    }
    200
}

fn ordered_bind_addrs() -> Vec<Ipv4Addr> {
    let mut specific = Vec::new();
    if let Ok(ifs) = get_if_addrs() {
        for ni in ifs {
            let IfAddr::V4(v4) = ni.addr else { continue };
            let ip = v4.ip;
            if ip.is_loopback() {
                continue;
            }
            if !is_private_ipv4(ip) {
                continue;
            }
            specific.push(ip);
        }
    }
    specific.sort_by(|a, b| {
        let c = bind_preference(*a).cmp(&bind_preference(*b));
        if c != std::cmp::Ordering::Equal {
            return c;
        }
        a.octets().cmp(&b.octets())
    });
    specific.push(Ipv4Addr::UNSPECIFIED);
    specific
}

fn enumerate_broadcast_targets() -> Vec<Ipv4Addr> {
    let mut out = vec![Ipv4Addr::new(255, 255, 255, 255)];
    if let Ok(ifs) = get_if_addrs() {
        for ni in ifs {
            let IfAddr::V4(v4) = ni.addr else { continue };
            let ip = v4.ip;
            if ip.is_loopback() {
                continue;
            }
            let o = ip.octets();
            out.push(Ipv4Addr::new(o[0], o[1], o[2], 255));
        }
    }
    out.sort();
    out.dedup();
    out
}

fn coerce_port(v: &serde_json::Value) -> Option<u16> {
    match v {
        serde_json::Value::Number(n) => n.as_u64().and_then(|x| u16::try_from(x).ok()),
        serde_json::Value::String(s) => s.trim().parse::<u16>().ok(),
        _ => None,
    }
}

fn try_parse_lan_packet(src_ip: IpAddr, data: &[u8]) -> Option<PeerInfoDto> {
    if data.len() < 4 || &data[..4] != LAN_MAGIC {
        return None;
    }
    let json = serde_json::from_slice::<serde_json::Value>(&data[4..]).ok()?;
    let did = json.get("did")?.as_str()?.trim().to_string();
    if did.is_empty() {
        return None;
    }
    let nick = json
        .get("nick")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let inst = json
        .get("inst")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let tags_raw = json.get("tags").and_then(|v| v.as_str()).unwrap_or("");
    let tags = tags_raw
        .split("|||")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    let fport = json.get("fport").and_then(coerce_port)?;
    if fport == 0 {
        return None;
    }
    let rport = json.get("rport").and_then(coerce_port).unwrap_or(0);
    let host = match src_ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => v6.to_string(),
    };
    Some(PeerInfoDto {
        peer_id: did,
        instance_id: inst,
        nickname: nick,
        tags,
        host,
        file_service_port: fport,
        remote_desktop_port: rport,
    })
}

fn spawn_lan_discovery(
    device_id: String,
    shutdown: Arc<AtomicBool>,
    payload: Arc<std::sync::Mutex<LanPayload>>,
) -> Option<thread::JoinHandle<()>> {
    let socket = ordered_bind_addrs()
        .into_iter()
        .find_map(|ip| UdpSocket::bind(SocketAddr::new(IpAddr::V4(ip), LAN_PORT)).ok())?;
    let _ = socket.set_broadcast(true);
    let _ = socket.set_read_timeout(Some(Duration::from_millis(200)));
    let _ = socket.set_write_timeout(Some(Duration::from_millis(200)));

    Some(thread::spawn(move || {
        let mut last_announce_at = Instant::now() - Duration::from_secs(10);
        let mut last_refresh_targets = Instant::now() - Duration::from_secs(10);
        let mut targets = enumerate_broadcast_targets();
        let mut buf = vec![0u8; 4096];

        while !shutdown.load(Ordering::SeqCst) {
            if last_refresh_targets.elapsed() > Duration::from_secs(15) {
                targets = enumerate_broadcast_targets();
                last_refresh_targets = Instant::now();
            }

            match socket.recv_from(&mut buf) {
                Ok((n, addr)) => {
                    if let Some(p) = try_parse_lan_packet(addr.ip(), &buf[..n]) {
                        if p.peer_id != device_id {
                            let _ = api::peer::register_discovered_peer(p);
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(_) => {}
            }

            if last_announce_at.elapsed() > Duration::from_millis(1200) {
                let m = payload.lock().ok().map(|p| p.clone());
                if let Some(m) = m {
                    let body = serde_json::to_vec(&m.to_json()).unwrap_or_default();
                    if !body.is_empty() {
                        let mut pkt = Vec::with_capacity(4 + body.len());
                        pkt.extend_from_slice(LAN_MAGIC);
                        pkt.extend_from_slice(&body);
                        for t in &targets {
                            let _ = socket.send_to(&pkt, SocketAddr::new(IpAddr::V4(*t), LAN_PORT));
                            let _ = socket.send_to(&pkt, SocketAddr::new(IpAddr::V4(*t), LAN_PORT));
                        }
                    }
                }
                last_announce_at = Instant::now();
            }
        }
    }))
}

#[derive(Clone, Default)]
struct LanPayload {
    inst: String,
    nick: String,
    tags: String,
    fport: u16,
    rport: u16,
}

impl LanPayload {
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "v": 1,
            "did": api::config::device_id().unwrap_or_default(),
            "inst": self.inst,
            "nick": self.nick,
            "tags": self.tags,
            "fport": self.fport,
            "rport": self.rport,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Home,
    Settings,
    Troubleshooting,
    About,
    RemoteDesktop,
}

#[derive(Default)]
struct PeerTree {
    children: BTreeMap<String, PeerTree>,
    peers: Vec<PeerInfoDto>,
}

impl PeerTree {
    fn insert(&mut self, peer: PeerInfoDto) {
        let mut node = self;
        for tag in &peer.tags {
            node = node.children.entry(tag.clone()).or_default();
        }
        node.peers.push(peer);
    }
}

#[derive(Clone)]
struct BulkTarget {
    peer_id: String,
    label: String,
}

#[derive(Clone)]
struct TransferMeta {
    icon: String,
    title: String,
}

#[derive(Clone)]
struct PendingDrop {
    paths: Vec<String>,
    pos: egui::Pos2,
}

#[derive(Clone, Copy)]
struct RemoteViewState {
    rect: egui::Rect,
    draw_size: (f32, f32),
    frame_size: (f32, f32),
    has_focus: bool,
}

#[derive(Clone, Default)]
enum SelectedTarget {
    #[default]
    None,
    User {
        peer_id: String,
        label: String,
    },
    Tag {
        key: String,
        label: String,
        targets: Vec<BulkTarget>,
    },
}

struct LanTransferApp {
    rt: tokio::runtime::Runtime,
    screen: Screen,
    last_screen: Screen,
    fullscreen_active: bool,
    viewport_init_phase: u8,

    device_id: String,

    nickname: String,
    tags: Vec<String>,
    receive_dir: String,

    dropped_paths: Vec<String>,
    hovered_file_count: usize,
    pointer_latest_pos: Option<egui::Pos2>,
    pending_drop: Option<PendingDrop>,
    selected_target: SelectedTarget,

    send_dialog_open: bool,
    send_target_peer_id: String,
    send_target_label: String,
    send_message: String,
    send_paths: Vec<String>,
    send_path_input: String,

    bulk_send_dialog_open: bool,
    bulk_send_target_label: String,
    bulk_send_targets: Vec<BulkTarget>,
    bulk_send_message: String,
    bulk_send_paths: Vec<String>,
    bulk_send_path_input: String,

    events: Vec<FileReceiveEventDto>,
    progresses: Vec<TransferProgressDto>,
    transfer_meta: BTreeMap<String, TransferMeta>,

    remote_host_port: Option<u16>,
    remote_host_enabled: bool,

    remote_token: String,
    remote_connect_host: String,
    remote_connect_port: String,
    remote_pending_connect: Option<(String, u16, String)>,
    remote_connected: bool,
    remote_texture: Option<TextureHandle>,
    remote_tex_size: (u32, u32),
    remote_left_down: bool,
    remote_right_down: bool,
    remote_last_sent_pos: Option<(i32, i32)>,
    remote_last_move_sent_at: Instant,
    remote_pending_move: Option<(f64, f64, i32)>,
    remote_last_mod_mask: i32,
    remote_prev_maximized: Option<bool>,
    remote_ime_buffer: String,

    status: String,

    lan_payload: Arc<std::sync::Mutex<LanPayload>>,
    lan_shutdown: Arc<AtomicBool>,
    lan_join: Option<thread::JoinHandle<()>>,
}

impl Drop for LanTransferApp {
    fn drop(&mut self) {
        self.lan_shutdown.store(true, Ordering::SeqCst);
        if let Some(j) = self.lan_join.take() {
            let _ = j.join();
        }
        let _ = api::config::shutdown_rust_lib();
    }
}

impl LanTransferApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        configure_fonts(&cc.egui_ctx);
        api::config::init_rust_lib();
        let device_id = api::config::device_id().unwrap_or_default();
        let session_id = api::config::session_id().unwrap_or_default();
        let profile = api::config::get_local_profile().unwrap_or_default();
        let receive_dir = api::config::get_default_receive_path().unwrap_or_default();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .build()
            .expect("tokio runtime");

        let lan_payload = Arc::new(std::sync::Mutex::new(LanPayload {
            inst: session_id.clone(),
            nick: if profile.nickname.is_empty() {
                "LanTransfer".to_string()
            } else {
                profile.nickname.clone()
            },
            tags: profile.tags.join("|||"),
            fport: 0,
            rport: 0,
        }));
        let lan_shutdown = Arc::new(AtomicBool::new(false));
        let lan_join = spawn_lan_discovery(
            device_id.clone(),
            Arc::clone(&lan_shutdown),
            Arc::clone(&lan_payload),
        );

        let mut status = String::new();
        match rt.block_on(api::transfer::file_service_start(None)) {
            Ok(port) => {
                if let Ok(mut g) = lan_payload.lock() {
                    g.fport = port;
                }
            }
            Err(e) => status = format!("启动文件服务失败: {} {}", e.code, e.message),
        }

        let mut remote_host_enabled = api::config::get_remote_host_enabled();
        let mut remote_host_port = None;
        if remote_host_enabled {
            let tok = LAN_REMOTE_SESSION_TOKEN.to_string();
            match rt.block_on(api::remote_host::remote_host_start(tok, None)) {
                Ok(port) => {
                    remote_host_port = Some(port);
                    if let Ok(mut g) = lan_payload.lock() {
                        g.rport = port;
                    }
                }
                Err(e) => {
                    remote_host_enabled = false;
                    let _ = api::config::set_remote_host_enabled(false);
                    status = format!("远程协助启动失败: {} {}", e.code, e.message);
                }
            }
        }

        Self {
            rt,
            screen: Screen::Home,
            last_screen: Screen::Home,
            fullscreen_active: false,
            viewport_init_phase: 0,
            device_id,
            nickname: profile.nickname,
            receive_dir,
            tags: profile.tags,
            dropped_paths: Vec::new(),
            hovered_file_count: 0,
            pointer_latest_pos: None,
            pending_drop: None,
            selected_target: SelectedTarget::None,
            send_dialog_open: false,
            send_target_peer_id: String::new(),
            send_target_label: String::new(),
            send_message: String::new(),
            send_paths: Vec::new(),
            send_path_input: String::new(),
            bulk_send_dialog_open: false,
            bulk_send_target_label: String::new(),
            bulk_send_targets: Vec::new(),
            bulk_send_message: String::new(),
            bulk_send_paths: Vec::new(),
            bulk_send_path_input: String::new(),
            events: Vec::new(),
            progresses: Vec::new(),
            transfer_meta: BTreeMap::new(),
            remote_host_port,
            remote_host_enabled,
            remote_token: LAN_REMOTE_SESSION_TOKEN.to_string(),
            remote_connect_host: String::new(),
            remote_connect_port: "0".to_string(),
            remote_pending_connect: None,
            remote_connected: false,
            remote_texture: None,
            remote_tex_size: (0, 0),
            remote_left_down: false,
            remote_right_down: false,
            remote_last_sent_pos: None,
            remote_last_move_sent_at: Instant::now() - Duration::from_secs(1),
            remote_pending_move: None,
            remote_last_mod_mask: 0,
            remote_prev_maximized: None,
            remote_ime_buffer: String::new(),
            status,
            lan_payload,
            lan_shutdown,
            lan_join,
        }
    }

    fn save_profile(&mut self) {
        let p = LocalProfileDto {
            nickname: self.nickname.clone(),
            tags: self.tags.clone(),
        };
        match api::config::set_local_profile(p) {
            Ok(()) => {
                if let Ok(mut g) = self.lan_payload.lock() {
                    g.nick = self.nickname.clone();
                    g.tags = self.tags.join("|||");
                }
                self.status = "已保存本机身份".to_string();
            }
            Err(e) => self.status = format!("保存失败: {} {}", e.code, e.message),
        }
    }

    fn apply_receive_dir(&mut self) {
        match api::config::set_default_receive_path(self.receive_dir.clone()) {
            Ok(()) => self.status = "已更新接收目录".to_string(),
            Err(e) => self.status = format!("设置接收目录失败: {} {}", e.code, e.message),
        }
    }

    fn sync_runtime_state(&mut self, ctx: &egui::Context) {
        self.dropped_paths.clear();
        self.hovered_file_count = ctx.input(|i| i.raw.hovered_files.len());
        self.pointer_latest_pos = ctx.input(|i| i.pointer.hover_pos().or(i.pointer.latest_pos()));

        let dropped_files = ctx.input(|i| i.raw.dropped_files.clone());
        let mut dropped_missing_path = 0usize;
        for f in dropped_files {
            if let Some(p) = f.path {
                self.dropped_paths.push(p.to_string_lossy().into_owned());
            } else {
                dropped_missing_path += 1;
            }
        }
        if !self.dropped_paths.is_empty() {
            let pos = self
                .pointer_latest_pos
                .unwrap_or_else(|| egui::pos2(f32::NAN, f32::NAN));
            self.pending_drop = Some(PendingDrop {
                paths: self.dropped_paths.clone(),
                pos,
            });
        } else if dropped_missing_path > 0 {
            self.status = "未能获取拖拽文件的路径：请确认本程序未以管理员运行，且尽量从资源管理器拖拽文件（不要跨权限拖拽）。".to_string();
        }

        let evs = api::transfer::pull_file_receive_events();
        if !evs.is_empty() {
            self.events.extend(evs);
            if self.events.len() > 200 {
                self.events.drain(..self.events.len() - 200);
            }
        }

        let ps = api::transfer::pull_transfer_progress();
        if !ps.is_empty() {
            self.progresses.extend(ps);
            if self.progresses.len() > 200 {
                self.progresses.drain(..self.progresses.len() - 200);
            }
        }
    }

    fn profile_line(&self) -> String {
        if self.nickname.trim().is_empty() && self.tags.is_empty() {
            return "未设置（请打开设置）".to_string();
        }
        if self.tags.is_empty() {
            return self.nickname.trim().to_string();
        }
        format!("{}（{}）", self.nickname.trim(), self.tags.join("/"))
    }

    fn sender_display_line(
        &self,
        peers_by_id: &BTreeMap<String, PeerInfoDto>,
        sender_id: &str,
    ) -> String {
        if let Some(p) = peers_by_id.get(sender_id) {
            let nick = if p.nickname.trim().is_empty() {
                "（对方未设置昵称）".to_string()
            } else {
                p.nickname.clone()
            };
            if p.tags.is_empty() {
                return nick;
            }
            return format!("{nick} · {}", p.tags.join(" / "));
        }
        let id = sender_id;
        let short = if id.len() <= 14 {
            id.to_string()
        } else {
            format!("{}…", &id[..10])
        };
        format!("未在列表中的用户 · {short}")
    }

    fn sender_nickname(
        &self,
        peers_by_id: &BTreeMap<String, PeerInfoDto>,
        sender_id: &str,
    ) -> String {
        if let Some(p) = peers_by_id.get(sender_id) {
            if !p.nickname.trim().is_empty() {
                return p.nickname.clone();
            }
            if !p.host.trim().is_empty() {
                return p.host.clone();
            }
        }
        let id = sender_id;
        if id.len() <= 10 {
            id.to_string()
        } else {
            format!("{}…", &id[..10])
        }
    }

    fn one_line_text(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn format_bytes(n: i64) -> String {
        let n = n.max(0) as f64;
        const KB: f64 = 1024.0;
        const MB: f64 = 1024.0 * KB;
        const GB: f64 = 1024.0 * MB;
        if n >= GB {
            return format!("{:.2} GB", n / GB);
        }
        if n >= MB {
            return format!("{:.2} MB", n / MB);
        }
        if n >= KB {
            return format!("{:.1} KB", n / KB);
        }
        format!("{:.0} B", n)
    }

    fn file_icon_label(path: &str) -> &'static str {
        let ext = Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        match ext.as_str() {
            "pdf" => "PDF",
            "ppt" | "pptx" => "PPT",
            "doc" | "docx" => "DOC",
            "xls" | "xlsx" => "XLS",
            "txt" | "md" | "log" => "TXT",
            "zip" | "rar" | "7z" => "ZIP",
            "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" => "IMG",
            "mp4" | "mov" | "avi" | "mkv" => "VID",
            "mp3" | "wav" | "flac" => "AUD",
            "exe" | "msi" => "EXE",
            _ => "FILE",
        }
    }

    fn basename(path: &str) -> String {
        Path::new(path)
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| path.to_string())
    }

    fn record_transfer_meta(&mut self, transfer_id: &str, file_paths: &[String]) {
        if self.transfer_meta.contains_key(transfer_id) {
            return;
        }
        let first = file_paths.first().map(|s| s.as_str()).unwrap_or("");
        let (title, icon) = if file_paths.len() <= 1 {
            (
                Self::basename(first),
                Self::file_icon_label(first).to_string(),
            )
        } else {
            (format!("{} 个文件", file_paths.len()), "FILE".to_string())
        };
        self.transfer_meta
            .insert(transfer_id.to_string(), TransferMeta { icon, title });
    }

    fn ui_section_title(ui: &mut egui::Ui, label: &str) {
        ui.add_space(2.0);
        ui.label(egui::RichText::new(label).strong().size(16.0));
        ui.add_space(6.0);
    }

    fn card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
        let frame = egui::Frame::group(ui.style())
            .rounding(egui::Rounding::same(12.0))
            .inner_margin(egui::Margin::symmetric(12.0, 12.0));
        frame.show(ui, add);
    }

    fn paint_drop_feedback(&self, ui: &egui::Ui, rect: egui::Rect) {
        if self.hovered_file_count == 0 {
            return;
        }
        let Some(pos) = self.pointer_latest_pos else {
            return;
        };
        if !pos.x.is_finite() || !pos.y.is_finite() {
            return;
        }
        if !rect.contains(pos) {
            return;
        }
        let c = ui.visuals().selection.bg_fill;
        let fill = egui::Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 40);
        ui.painter()
            .rect_filled(rect.expand(2.0), egui::Rounding::same(8.0), fill);
        ui.painter().rect_stroke(
            rect.expand(2.0),
            egui::Rounding::same(8.0),
            egui::Stroke::new(1.0, c),
        );
    }

    fn paint_selected_feedback(&self, ui: &egui::Ui, rect: egui::Rect) {
        let c = ui.visuals().selection.bg_fill;
        let fill = egui::Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 24);
        ui.painter()
            .rect_filled(rect.expand(2.0), egui::Rounding::same(8.0), fill);
        ui.painter().rect_stroke(
            rect.expand(2.0),
            egui::Rounding::same(8.0),
            egui::Stroke::new(1.0, c),
        );
    }

    fn ui_app_bar(&mut self, ctx: &egui::Context) {
        if self.screen == Screen::RemoteDesktop {
            return;
        }
        egui::TopBottomPanel::top("appbar")
            .exact_height(48.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if self.screen != Screen::Home && ui.button("←").clicked() {
                        self.screen = Screen::Home;
                    }
                    ui.label(egui::RichText::new(self.profile_line()).strong().size(16.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.screen == Screen::Home {
                            if ui.button("关于").clicked() {
                                self.screen = Screen::About;
                            }
                            if ui.button("连接问题说明").clicked() {
                                self.screen = Screen::Troubleshooting;
                            }
                            if ui.button("设置").clicked() {
                                self.screen = Screen::Settings;
                            }
                        }
                        if !self.status.is_empty() {
                            ui.label(self.status.clone());
                        }
                    });
                });
            });
    }

    fn init_root_viewport(&mut self, ctx: &egui::Context) {
        if self.viewport_init_phase == 0 {
            if let Some(ms) = ctx.input(|i| i.viewport().monitor_size) {
                if 1.0 < ms.x && 1.0 < ms.y {
                    let mut w = ms.x * 0.5;
                    let mut h = ms.y * (2.0 / 3.0);
                    w = w.clamp(900.0, ms.x - 80.0);
                    h = h.clamp(600.0, ms.y - 120.0);
                    ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(w, h)));
                    self.viewport_init_phase = 1;
                }
            }
        } else if self.viewport_init_phase == 1 {
            if let Some(cmd) = egui::ViewportCommand::center_on_screen(ctx) {
                ctx.send_viewport_cmd(cmd);
                self.viewport_init_phase = 2;
            }
        }
    }

    fn open_send_dialog_for_user(&mut self, peer_id: String, label: String, paths: Vec<String>) {
        if self.send_dialog_open || self.bulk_send_dialog_open {
            return;
        }
        self.send_target_peer_id = peer_id;
        self.send_target_label = label;
        self.send_paths = paths;
        self.send_message.clear();
        self.send_path_input.clear();
        self.send_dialog_open = true;
    }

    fn open_bulk_send_dialog_for_tag(
        &mut self,
        tag_label: String,
        targets: Vec<BulkTarget>,
        paths: Vec<String>,
    ) {
        if self.send_dialog_open || self.bulk_send_dialog_open {
            return;
        }
        self.bulk_send_target_label = tag_label;
        self.bulk_send_targets = targets;
        self.bulk_send_paths = paths;
        self.bulk_send_message.clear();
        self.bulk_send_path_input.clear();
        self.bulk_send_dialog_open = true;
    }

    fn ui_send_dialog(&mut self, ctx: &egui::Context) {
        if !self.send_dialog_open {
            return;
        }
        let mut open = self.send_dialog_open;
        let mut want_close = false;
        let mut action: Option<SendFilesRequestDto> = None;
        egui::Window::new("发送文件")
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .show(ctx, |ui| {
                ui.label(format!("发送至：{}", self.send_target_label));
                ui.add_space(8.0);
                ui.label("发送留言（可选）");
                ui.add(
                    egui::TextEdit::multiline(&mut self.send_message)
                        .desired_width(f32::INFINITY)
                        .desired_rows(3),
                );
                ui.add_space(10.0);

                ui.label("文件列表");
                Self::card(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("追加路径");
                        ui.text_edit_singleline(&mut self.send_path_input);
                        if ui.button("添加").clicked() {
                            let p = self.send_path_input.trim().to_string();
                            if !p.is_empty() {
                                self.send_paths.push(p);
                                self.send_path_input.clear();
                            } else if let Some(files) = FileDialog::new().pick_files() {
                                for f in files {
                                    self.send_paths.push(f.to_string_lossy().into_owned());
                                }
                            }
                        }
                        if ui.button("清空").clicked() {
                            self.send_paths.clear();
                        }
                    });
                    egui::ScrollArea::vertical()
                        .max_height(140.0)
                        .show(ui, |ui| {
                            let mut remove_idx = None;
                            for (idx, p) in self.send_paths.iter().enumerate() {
                                ui.horizontal(|ui| {
                                    ui.label(p);
                                    if ui.small_button("移除").clicked() {
                                        remove_idx = Some(idx);
                                    }
                                });
                            }
                            if let Some(i) = remove_idx {
                                self.send_paths.remove(i);
                            }
                        });
                });

                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("取消").clicked() {
                        want_close = true;
                    }
                    if ui.button("确定").clicked() {
                        if self.send_paths.is_empty() {
                            self.status = "请先添加要发送的文件".to_string();
                        } else {
                            action = Some(SendFilesRequestDto {
                                target_peer_id: self.send_target_peer_id.clone(),
                                file_paths: self.send_paths.clone(),
                                message: self.send_message.clone(),
                            });
                            want_close = true;
                        }
                    }
                });
            });
        if want_close {
            open = false;
        }
        self.send_dialog_open = open;

        if let Some(req) = action {
            let paths = req.file_paths.clone();
            let target_label = self.send_target_label.clone();
            match self.rt.block_on(api::transfer::send_files(req)) {
                Ok(tid) => {
                    self.record_transfer_meta(&tid, &paths);
                    self.status = format!(
                        "已开始发送 {} 个文件 → {} ({})",
                        paths.len(),
                        target_label,
                        tid
                    );
                }
                Err(e) => {
                    self.status = format!("发送失败: {} {}", e.code, e.message);
                }
            }
        }
    }

    fn ui_bulk_send_dialog(&mut self, ctx: &egui::Context) {
        if !self.bulk_send_dialog_open {
            return;
        }
        let mut open = self.bulk_send_dialog_open;
        let mut want_close = false;
        let mut action: Option<(Vec<BulkTarget>, Vec<String>, String)> = None;
        egui::Window::new("批量发送")
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .show(ctx, |ui| {
                ui.label(format!(
                    "发送至标签：{}（{} 个用户）",
                    self.bulk_send_target_label,
                    self.bulk_send_targets.len()
                ));
                ui.add_space(8.0);

                egui::CollapsingHeader::new("接收方列表")
                    .default_open(false)
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .max_height(120.0)
                            .show(ui, |ui| {
                                for t in &self.bulk_send_targets {
                                    ui.label(&t.label);
                                }
                            });
                    });

                ui.add_space(8.0);
                ui.label("发送留言（可选）");
                ui.add(
                    egui::TextEdit::multiline(&mut self.bulk_send_message)
                        .desired_width(f32::INFINITY)
                        .desired_rows(3),
                );
                ui.add_space(10.0);

                ui.label("文件列表");
                Self::card(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("追加路径");
                        ui.text_edit_singleline(&mut self.bulk_send_path_input);
                        if ui.button("添加").clicked() {
                            let p = self.bulk_send_path_input.trim().to_string();
                            if !p.is_empty() {
                                self.bulk_send_paths.push(p);
                                self.bulk_send_path_input.clear();
                            } else if let Some(files) = FileDialog::new().pick_files() {
                                for f in files {
                                    self.bulk_send_paths.push(f.to_string_lossy().into_owned());
                                }
                            }
                        }
                        if ui.button("清空").clicked() {
                            self.bulk_send_paths.clear();
                        }
                    });
                    egui::ScrollArea::vertical()
                        .max_height(140.0)
                        .show(ui, |ui| {
                            let mut remove_idx = None;
                            for (idx, p) in self.bulk_send_paths.iter().enumerate() {
                                ui.horizontal(|ui| {
                                    ui.label(p);
                                    if ui.small_button("移除").clicked() {
                                        remove_idx = Some(idx);
                                    }
                                });
                            }
                            if let Some(i) = remove_idx {
                                self.bulk_send_paths.remove(i);
                            }
                        });
                });

                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("取消").clicked() {
                        want_close = true;
                    }
                    if ui.button("确定").clicked() {
                        if self.bulk_send_paths.is_empty() {
                            self.status = "请先添加要发送的文件".to_string();
                        } else if self.bulk_send_targets.is_empty() {
                            self.status = "该标签下没有可发送的用户".to_string();
                        } else {
                            action = Some((
                                self.bulk_send_targets.clone(),
                                self.bulk_send_paths.clone(),
                                self.bulk_send_message.clone(),
                            ));
                            want_close = true;
                        }
                    }
                });
            });
        if want_close {
            open = false;
        }
        self.bulk_send_dialog_open = open;

        if let Some((targets, paths, message)) = action {
            let mut ok = 0;
            let mut fail = 0;
            let mut last_err = None;
            for t in targets {
                let req = SendFilesRequestDto {
                    target_peer_id: t.peer_id,
                    file_paths: paths.clone(),
                    message: message.clone(),
                };
                match self.rt.block_on(api::transfer::send_files(req)) {
                    Ok(tid) => {
                        ok += 1;
                        self.record_transfer_meta(&tid, &paths);
                    }
                    Err(e) => {
                        fail += 1;
                        last_err = Some(format!("{} {}", e.code, e.message));
                    }
                }
            }
            if let Some(e) = last_err {
                self.status = format!("批量发送已提交：成功 {ok}，失败 {fail}（最近错误：{e}）");
            } else {
                self.status = format!("批量发送已提交：成功 {ok}，失败 {fail}");
            }
        }
    }

    fn collect_peers_under_tree(&self, tree: &PeerTree, out: &mut Vec<PeerInfoDto>) {
        out.extend(tree.peers.iter().cloned());
        for child in tree.children.values() {
            self.collect_peers_under_tree(child, out);
        }
    }

    fn modifiers_to_mask(m: egui::Modifiers) -> i32 {
        (if m.shift { 1 } else { 0 })
            | (if m.ctrl { 2 } else { 0 })
            | (if m.alt { 4 } else { 0 })
            | (if m.mac_cmd || m.command { 8 } else { 0 })
    }

    fn egui_key_to_rd_code(k: egui::Key) -> Option<i32> {
        Some(match k {
            egui::Key::ArrowLeft => -100,
            egui::Key::ArrowRight => -101,
            egui::Key::ArrowUp => -102,
            egui::Key::ArrowDown => -103,
            egui::Key::Enter => -104,
            egui::Key::Backspace => -105,
            egui::Key::Escape => -106,
            egui::Key::Delete => -107,
            egui::Key::Tab => -108,
            egui::Key::Home => -109,
            egui::Key::End => -110,
            egui::Key::PageUp => -111,
            egui::Key::PageDown => -112,
            _ => return None,
        })
    }

    fn egui_key_to_char(key: egui::Key, modifiers: egui::Modifiers) -> Option<char> {
        let shift = modifiers.shift;
        Some(match key {
            egui::Key::Space => ' ',
            egui::Key::Comma => {
                if shift {
                    '<'
                } else {
                    ','
                }
            }
            egui::Key::Period => {
                if shift {
                    '>'
                } else {
                    '.'
                }
            }
            egui::Key::Slash => {
                if shift {
                    '?'
                } else {
                    '/'
                }
            }
            egui::Key::Backslash => {
                if shift {
                    '|'
                } else {
                    '\\'
                }
            }
            egui::Key::Pipe => '|',
            egui::Key::Questionmark => '?',
            egui::Key::OpenBracket => {
                if shift {
                    '{'
                } else {
                    '['
                }
            }
            egui::Key::CloseBracket => {
                if shift {
                    '}'
                } else {
                    ']'
                }
            }
            egui::Key::Backtick => {
                if shift {
                    '~'
                } else {
                    '`'
                }
            }
            egui::Key::Minus => {
                if shift {
                    '_'
                } else {
                    '-'
                }
            }
            egui::Key::Equals => {
                if shift {
                    '+'
                } else {
                    '='
                }
            }
            egui::Key::Plus => '+',
            egui::Key::Semicolon => {
                if shift {
                    ':'
                } else {
                    ';'
                }
            }
            egui::Key::Colon => ':',
            egui::Key::Quote => {
                if shift {
                    '"'
                } else {
                    '\''
                }
            }
            egui::Key::Num0 => {
                if shift {
                    ')'
                } else {
                    '0'
                }
            }
            egui::Key::Num1 => {
                if shift {
                    '!'
                } else {
                    '1'
                }
            }
            egui::Key::Num2 => {
                if shift {
                    '@'
                } else {
                    '2'
                }
            }
            egui::Key::Num3 => {
                if shift {
                    '#'
                } else {
                    '3'
                }
            }
            egui::Key::Num4 => {
                if shift {
                    '$'
                } else {
                    '4'
                }
            }
            egui::Key::Num5 => {
                if shift {
                    '%'
                } else {
                    '5'
                }
            }
            egui::Key::Num6 => {
                if shift {
                    '^'
                } else {
                    '6'
                }
            }
            egui::Key::Num7 => {
                if shift {
                    '&'
                } else {
                    '7'
                }
            }
            egui::Key::Num8 => {
                if shift {
                    '*'
                } else {
                    '8'
                }
            }
            egui::Key::Num9 => {
                if shift {
                    '('
                } else {
                    '9'
                }
            }
            egui::Key::A => {
                if shift {
                    'A'
                } else {
                    'a'
                }
            }
            egui::Key::B => {
                if shift {
                    'B'
                } else {
                    'b'
                }
            }
            egui::Key::C => {
                if shift {
                    'C'
                } else {
                    'c'
                }
            }
            egui::Key::D => {
                if shift {
                    'D'
                } else {
                    'd'
                }
            }
            egui::Key::E => {
                if shift {
                    'E'
                } else {
                    'e'
                }
            }
            egui::Key::F => {
                if shift {
                    'F'
                } else {
                    'f'
                }
            }
            egui::Key::G => {
                if shift {
                    'G'
                } else {
                    'g'
                }
            }
            egui::Key::H => {
                if shift {
                    'H'
                } else {
                    'h'
                }
            }
            egui::Key::I => {
                if shift {
                    'I'
                } else {
                    'i'
                }
            }
            egui::Key::J => {
                if shift {
                    'J'
                } else {
                    'j'
                }
            }
            egui::Key::K => {
                if shift {
                    'K'
                } else {
                    'k'
                }
            }
            egui::Key::L => {
                if shift {
                    'L'
                } else {
                    'l'
                }
            }
            egui::Key::M => {
                if shift {
                    'M'
                } else {
                    'm'
                }
            }
            egui::Key::N => {
                if shift {
                    'N'
                } else {
                    'n'
                }
            }
            egui::Key::O => {
                if shift {
                    'O'
                } else {
                    'o'
                }
            }
            egui::Key::P => {
                if shift {
                    'P'
                } else {
                    'p'
                }
            }
            egui::Key::Q => {
                if shift {
                    'Q'
                } else {
                    'q'
                }
            }
            egui::Key::R => {
                if shift {
                    'R'
                } else {
                    'r'
                }
            }
            egui::Key::S => {
                if shift {
                    'S'
                } else {
                    's'
                }
            }
            egui::Key::T => {
                if shift {
                    'T'
                } else {
                    't'
                }
            }
            egui::Key::U => {
                if shift {
                    'U'
                } else {
                    'u'
                }
            }
            egui::Key::V => {
                if shift {
                    'V'
                } else {
                    'v'
                }
            }
            egui::Key::W => {
                if shift {
                    'W'
                } else {
                    'w'
                }
            }
            egui::Key::X => {
                if shift {
                    'X'
                } else {
                    'x'
                }
            }
            egui::Key::Y => {
                if shift {
                    'Y'
                } else {
                    'y'
                }
            }
            egui::Key::Z => {
                if shift {
                    'Z'
                } else {
                    'z'
                }
            }
            _ => return None,
        })
    }

    fn disconnect_remote(&mut self) {
        let _ = self
            .rt
            .block_on(api::remote_client::remote_client_disconnect());
        self.remote_connected = false;
        self.remote_pending_connect = None;
        self.remote_texture = None;
        self.remote_tex_size = (0, 0);
        self.remote_left_down = false;
        self.remote_right_down = false;
        self.remote_last_sent_pos = None;
        self.remote_last_move_sent_at = Instant::now() - Duration::from_secs(1);
        self.remote_pending_move = None;
        self.remote_last_mod_mask = 0;
        self.remote_ime_buffer.clear();
    }

    fn ensure_remote_connected(&mut self) {
        if self.remote_connected {
            return;
        }
        let Some((host, port, tok)) = self.remote_pending_connect.clone() else {
            return;
        };
        let res = self
            .rt
            .block_on(api::remote_client::remote_client_connect(host, port, tok));
        match res {
            Ok(()) => {
                self.remote_connected = true;
                self.remote_pending_connect = None;
            }
            Err(e) => {
                self.remote_connected = false;
                self.remote_pending_connect = None;
                self.remote_texture = None;
                self.remote_tex_size = (0, 0);
                self.status = format!("连接失败: {} {}", e.code, e.message);
                self.screen = Screen::Home;
            }
        }
    }

    fn ui_remote_desktop_fullscreen(&mut self, ctx: &egui::Context) {
        if !self.fullscreen_active {
            self.remote_prev_maximized = ctx.input(|i| i.viewport().maximized);
            ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
            self.fullscreen_active = true;
        }

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.disconnect_remote();
            if self.fullscreen_active {
                ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(
                    self.remote_prev_maximized.unwrap_or(false),
                ));
                self.fullscreen_active = false;
                self.remote_prev_maximized = None;
            }
            self.screen = Screen::Home;
            return;
        }

        self.ensure_remote_connected();

        if self.remote_connected {
            let tab_plain =
                ctx.input_mut(|i| i.count_and_consume_key(egui::Modifiers::NONE, egui::Key::Tab));
            let tab_shift = ctx.input_mut(|i| {
                i.count_and_consume_key(
                    egui::Modifiers {
                        shift: true,
                        ..Default::default()
                    },
                    egui::Key::Tab,
                )
            });
            for _ in 0..tab_plain {
                let _ = api::remote_client::remote_client_send_key(RemoteKeyEventDto {
                    key_code: -108,
                    down: true,
                    modifiers: 0,
                });
                let _ = api::remote_client::remote_client_send_key(RemoteKeyEventDto {
                    key_code: -108,
                    down: false,
                    modifiers: 0,
                });
            }
            for _ in 0..tab_shift {
                let mods = Self::modifiers_to_mask(egui::Modifiers {
                    shift: true,
                    ..Default::default()
                });
                let _ = api::remote_client::remote_client_send_key(RemoteKeyEventDto {
                    key_code: -108,
                    down: true,
                    modifiers: mods,
                });
                let _ = api::remote_client::remote_client_send_key(RemoteKeyEventDto {
                    key_code: -108,
                    down: false,
                    modifiers: mods,
                });
            }

            let up = ctx
                .input_mut(|i| i.count_and_consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp));
            let down = ctx.input_mut(|i| {
                i.count_and_consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown)
            });
            let left = ctx.input_mut(|i| {
                i.count_and_consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft)
            });
            let right = ctx.input_mut(|i| {
                i.count_and_consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight)
            });
            for _ in 0..up {
                let _ = api::remote_client::remote_client_send_key(RemoteKeyEventDto {
                    key_code: -102,
                    down: true,
                    modifiers: 0,
                });
                let _ = api::remote_client::remote_client_send_key(RemoteKeyEventDto {
                    key_code: -102,
                    down: false,
                    modifiers: 0,
                });
            }
            for _ in 0..down {
                let _ = api::remote_client::remote_client_send_key(RemoteKeyEventDto {
                    key_code: -103,
                    down: true,
                    modifiers: 0,
                });
                let _ = api::remote_client::remote_client_send_key(RemoteKeyEventDto {
                    key_code: -103,
                    down: false,
                    modifiers: 0,
                });
            }
            for _ in 0..left {
                let _ = api::remote_client::remote_client_send_key(RemoteKeyEventDto {
                    key_code: -100,
                    down: true,
                    modifiers: 0,
                });
                let _ = api::remote_client::remote_client_send_key(RemoteKeyEventDto {
                    key_code: -100,
                    down: false,
                    modifiers: 0,
                });
            }
            for _ in 0..right {
                let _ = api::remote_client::remote_client_send_key(RemoteKeyEventDto {
                    key_code: -101,
                    down: true,
                    modifiers: 0,
                });
                let _ = api::remote_client::remote_client_send_key(RemoteKeyEventDto {
                    key_code: -101,
                    down: false,
                    modifiers: 0,
                });
            }
        }

        egui::TopBottomPanel::top("rd_top")
            .exact_height(44.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("← 返回").clicked() {
                        self.disconnect_remote();
                        self.screen = Screen::Home;
                        return;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("断开").clicked() {
                            self.disconnect_remote();
                            self.screen = Screen::Home;
                        }
                        if !self.remote_connect_host.trim().is_empty()
                            && self.remote_connect_port.trim() != "0"
                        {
                            ui.label(format!(
                                "{}:{}",
                                self.remote_connect_host, self.remote_connect_port
                            ));
                        }
                    });
                });
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            self.ui_remote_view(ui, ctx);
        });
    }

    fn ui_remote_view(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let mut frame: Option<VideoFrameDto> = None;
        for _ in 0..2 {
            if let Some(f) = api::remote_client::remote_client_try_take_rgba_frame() {
                frame = Some(f);
            } else {
                break;
            }
        }

        if let Some(f) = frame {
            let img = self.decode_frame_to_image(&f);
            if let Some(img) = img {
                let (w, h) = (img.width() as u32, img.height() as u32);
                if self.remote_texture.is_none() || self.remote_tex_size != (w, h) {
                    self.remote_texture =
                        Some(ctx.load_texture("remote_frame", img.clone(), TextureOptions::LINEAR));
                    self.remote_tex_size = (w, h);
                } else if let Some(tex) = &mut self.remote_texture {
                    tex.set(img, TextureOptions::LINEAR);
                }
            }
        }

        let mut view_state: Option<RemoteViewState> = None;
        let mut ime_focus_resp: Option<egui::Response> = None;

        ui.group(|ui| {
            let tex = self.remote_texture.as_ref();
            if tex.is_none() {
                ui.label("暂无画面");
                return;
            }
            let tex = tex.unwrap();
            let avail = ui.available_size();
            let mut w = avail.x.max(1.0);
            let mut h = avail.y.max(1.0);
            let iw = tex.size_vec2().x.max(1.0);
            let ih = tex.size_vec2().y.max(1.0);
            let scale = (w / iw).min(h / ih);
            w = iw * scale;
            h = ih * scale;
            let (rect, resp) =
                ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::click_and_drag());
            ui.painter().image(
                tex.id(),
                rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );

            if self.remote_connected {
                let (pointer_pos, primary_pressed, secondary_pressed) = ctx.input(|i| {
                    (
                        i.pointer.interact_pos().or(i.pointer.latest_pos()),
                        i.pointer.primary_pressed(),
                        i.pointer.secondary_pressed(),
                    )
                });
                let inside = pointer_pos.is_some_and(|p| rect.contains(p));
                if (inside && (primary_pressed || secondary_pressed))
                    || resp.clicked()
                    || resp.drag_started()
                {
                    resp.request_focus();
                }
            }
            view_state = Some(RemoteViewState {
                rect,
                draw_size: (w, h),
                frame_size: (iw, ih),
                has_focus: resp.has_focus(),
            });
        });

        if self.remote_connected {
            let rect = egui::Rect::from_min_size(
                ui.max_rect().left_top() - egui::vec2(2000.0, 2000.0),
                egui::vec2(1.0, 1.0),
            );
            ui.allocate_new_ui(egui::UiBuilder::new().max_rect(rect), |ui| {
                let resp = ui.add_sized(
                    egui::vec2(1.0, 1.0),
                    egui::TextEdit::singleline(&mut self.remote_ime_buffer)
                        .frame(false)
                        .lock_focus(true)
                        .desired_width(1.0)
                        .text_color(egui::Color32::TRANSPARENT),
                );
                ime_focus_resp = Some(resp);
            });
        }

        if let Some(r) = &ime_focus_resp {
            r.request_focus();
            if let Some(v) = view_state.as_mut() {
                v.has_focus = r.has_focus();
            }
        }

        self.handle_remote_input(ctx, view_state);
    }

    fn handle_remote_input(&mut self, ctx: &egui::Context, view_state: Option<RemoteViewState>) {
        if !self.remote_connected {
            return;
        }
        let Some(view) = view_state else {
            return;
        };
        self.sync_remote_modifiers(ctx, view.has_focus);
        if !view.has_focus {
            return;
        }

        self.handle_remote_pointer(ctx, view);
        self.handle_remote_keyboard_and_wheel(ctx, view);
    }

    fn sync_remote_modifiers(&mut self, ctx: &egui::Context, focused: bool) {
        const RD_KEY_CONTROL: i32 = -113;
        const RD_KEY_SHIFT: i32 = -114;
        const RD_KEY_ALT: i32 = -115;
        const RD_KEY_META: i32 = -116;

        let cur = if focused {
            ctx.input(|i| Self::modifiers_to_mask(i.modifiers))
        } else {
            0
        };
        let prev = self.remote_last_mod_mask;
        let changed = cur ^ prev;
        if changed == 0 {
            return;
        }
        let send = |bit: i32, key: i32| {
            if (changed & bit) == 0 {
                return;
            }
            let down = (cur & bit) != 0;
            let _ = api::remote_client::remote_client_send_key(RemoteKeyEventDto {
                key_code: key,
                down,
                modifiers: cur,
            });
        };

        send(1, RD_KEY_SHIFT);
        send(2, RD_KEY_CONTROL);
        send(4, RD_KEY_ALT);
        send(8, RD_KEY_META);

        self.remote_last_mod_mask = cur;
    }

    fn handle_remote_pointer(&mut self, ctx: &egui::Context, view: RemoteViewState) {
        let (mods, pointer_pos, primary_pressed, primary_released, secondary_pressed, secondary_released) =
            ctx.input(|i| {
                (
                    i.modifiers,
                    i.pointer.interact_pos().or(i.pointer.latest_pos()),
                    i.pointer.primary_pressed(),
                    i.pointer.primary_released(),
                    i.pointer.secondary_pressed(),
                    i.pointer.secondary_released(),
                )
            });
        let mod_mask = Self::modifiers_to_mask(mods);

        if let Some(pos) = pointer_pos {
            let inside = view.rect.contains(pos);
            let can_send_pointer = inside || self.remote_left_down || self.remote_right_down;
            if can_send_pointer {
                let rel = pos - view.rect.min;
                let (w, h) = view.draw_size;
                let (iw, ih) = view.frame_size;
                let fx = (rel.x * (iw / w)).clamp(0.0, iw - 1.0);
                let fy = (rel.y * (ih / h)).clamp(0.0, ih - 1.0);
                let fx_i = fx.round() as i32;
                let fy_i = fy.round() as i32;

                if self.remote_last_sent_pos != Some((fx_i, fy_i)) {
                    self.remote_pending_move = Some((fx as f64, fy as f64, mod_mask));
                }

                if self.remote_last_move_sent_at.elapsed() >= Duration::from_millis(16) {
                    if let Some((x, y, m)) = self.remote_pending_move.take() {
                        let _ = api::remote_client::remote_client_send_pointer(
                            RemotePointerEventDto {
                                kind: "move".to_string(),
                                x,
                                y,
                                button: 1,
                                delta: 0.0,
                                modifiers: m,
                            },
                        );
                        self.remote_last_sent_pos = Some((fx_i, fy_i));
                        self.remote_last_move_sent_at = Instant::now();
                    }
                }

                if primary_pressed && inside && !self.remote_left_down {
                    self.remote_left_down = true;
                    let _ = api::remote_client::remote_client_send_pointer(
                        RemotePointerEventDto {
                            kind: "down".to_string(),
                            x: fx as f64,
                            y: fy as f64,
                            button: 1,
                            delta: 0.0,
                            modifiers: mod_mask,
                        },
                    );
                }

                if secondary_pressed && inside && !self.remote_right_down {
                    self.remote_right_down = true;
                    let _ = api::remote_client::remote_client_send_pointer(
                        RemotePointerEventDto {
                            kind: "down".to_string(),
                            x: fx as f64,
                            y: fy as f64,
                            button: 2,
                            delta: 0.0,
                            modifiers: mod_mask,
                        },
                    );
                }
            }
        }

        if primary_released && self.remote_left_down {
            self.remote_left_down = false;
            let (x, y) = self
                .remote_last_sent_pos
                .map(|(x, y)| (x as f64, y as f64))
                .unwrap_or((0.0, 0.0));
            let _ = api::remote_client::remote_client_send_pointer(RemotePointerEventDto {
                kind: "up".to_string(),
                x,
                y,
                button: 1,
                delta: 0.0,
                modifiers: mod_mask,
            });
        }
        if secondary_released && self.remote_right_down {
            self.remote_right_down = false;
            let (x, y) = self
                .remote_last_sent_pos
                .map(|(x, y)| (x as f64, y as f64))
                .unwrap_or((0.0, 0.0));
            let _ = api::remote_client::remote_client_send_pointer(RemotePointerEventDto {
                kind: "up".to_string(),
                x,
                y,
                button: 2,
                delta: 0.0,
                modifiers: mod_mask,
            });
        }
    }

    fn handle_remote_keyboard_and_wheel(&mut self, ctx: &egui::Context, view: RemoteViewState) {
        if !self.remote_ime_buffer.is_empty() {
            let text = std::mem::take(&mut self.remote_ime_buffer);
            for ch in text.chars() {
                let _ = api::remote_client::remote_client_send_key(RemoteKeyEventDto {
                    key_code: ch as i32,
                    down: true,
                    modifiers: 0,
                });
            }
        }

        let events = ctx.input(|i| i.events.clone());
        for e in events {
            match e {
                egui::Event::Key {
                    key,
                    pressed,
                    modifiers,
                    ..
                } => {
                    if let Some(code) = Self::egui_key_to_rd_code(key) {
                        let _ = api::remote_client::remote_client_send_key(RemoteKeyEventDto {
                            key_code: code,
                            down: pressed,
                            modifiers: Self::modifiers_to_mask(modifiers),
                        });
                    } else if let Some(ch) = Self::egui_key_to_char(key, modifiers) {
                        if modifiers.ctrl || modifiers.alt || modifiers.command || modifiers.mac_cmd {
                            let _ = api::remote_client::remote_client_send_key(RemoteKeyEventDto {
                                key_code: ch as i32,
                                down: pressed,
                                modifiers: Self::modifiers_to_mask(modifiers),
                            });
                        }
                    }
                }
                egui::Event::MouseWheel {
                    unit,
                    delta,
                    modifiers,
                } => {
                    let pos = ctx.input(|i| i.pointer.interact_pos().or(i.pointer.latest_pos()));
                    let Some(pos) = pos else {
                        continue;
                    };
                    if !view.rect.contains(pos) {
                        continue;
                    }
                    let dy = match unit {
                        egui::MouseWheelUnit::Point => delta.y,
                        egui::MouseWheelUnit::Line => delta.y * 50.0,
                        egui::MouseWheelUnit::Page => delta.y * 500.0,
                    };
                    let lines = (dy / 50.0).round().clamp(-32.0, 32.0);
                    if lines == 0.0 {
                        continue;
                    }
                    let rel = pos - view.rect.min;
                    let (w, h) = view.draw_size;
                    let (iw, ih) = view.frame_size;
                    let fx = (rel.x * (iw / w)).clamp(0.0, iw - 1.0);
                    let fy = (rel.y * (ih / h)).clamp(0.0, ih - 1.0);
                    let _ = api::remote_client::remote_client_send_pointer(RemotePointerEventDto {
                        kind: "scroll".to_string(),
                        x: fx as f64,
                        y: fy as f64,
                        button: 1,
                        delta: lines as f64,
                        modifiers: Self::modifiers_to_mask(modifiers),
                    });
                }
                _ => {}
            }
        }
    }

    fn decode_frame_to_image(&self, f: &VideoFrameDto) -> Option<ColorImage> {
        let w = f.width as usize;
        let h = f.height as usize;
        let need = w.saturating_mul(h).saturating_mul(4);
        let is_jpeg = f.rgba.len() >= 2 && f.rgba[0] == 0xFF && f.rgba[1] == 0xD8;
        let is_png =
            f.rgba.len() >= 8 && f.rgba[0..8] == [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

        if !is_jpeg && !is_png && need != 0 && f.rgba.len() == need {
            return Some(ColorImage::from_rgba_unmultiplied([w, h], &f.rgba));
        }
        let dyn_img = image::load_from_memory(&f.rgba).ok()?;
        let rgba = dyn_img.to_rgba8();
        let (w, h) = (rgba.width() as usize, rgba.height() as usize);
        Some(ColorImage::from_rgba_unmultiplied([w, h], rgba.as_raw()))
    }

    fn ui_home_screen(&mut self, ui: &mut egui::Ui) {
        let peers = api::peer::list_known_peers().unwrap_or_default();
        let mut peers_by_id = BTreeMap::new();
        let mut tree = PeerTree::default();
        for p in peers {
            peers_by_id.insert(p.peer_id.clone(), p.clone());
            tree.insert(p);
        }

        let mut recent_transfers = Vec::new();
        let mut seen = std::collections::HashSet::<String>::new();
        for p in self.progresses.iter().rev() {
            if seen.insert(p.transfer_id.clone()) {
                recent_transfers.push(p.clone());
                if recent_transfers.len() >= 5 {
                    break;
                }
            }
        }
        let lan_active = self.lan_join.is_some();
        let mut pending_drop = self.pending_drop.take();

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                Self::ui_section_title(ui, "传输进度");
                Self::card(ui, |ui| {
                    if recent_transfers.is_empty() {
                        ui.label("暂无传输任务");
                    } else {
                        for t in recent_transfers {
                            let total = t.total_bytes.max(0) as f32;
                            let sent = t.bytes_sent.max(0) as f32;
                            let meta = self.transfer_meta.get(&t.transfer_id);
                            let icon = meta.map(|m| m.icon.as_str()).unwrap_or("FILE");
                            let title = meta
                                .map(|m| m.title.as_str())
                                .unwrap_or_else(|| t.transfer_id.as_str());
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new(icon)
                                        .strong()
                                        .color(ui.visuals().selection.stroke.color),
                                );
                                ui.label(egui::RichText::new(title).strong());

                                if total > 0.0 {
                                    let ratio = (sent / total).clamp(0.0, 1.0);
                                    ui.add_sized(
                                        egui::vec2(220.0, 18.0),
                                        egui::ProgressBar::new(ratio).show_percentage(),
                                    );
                                    ui.label(format!(
                                        "{} / {}",
                                        Self::format_bytes(t.bytes_sent),
                                        Self::format_bytes(t.total_bytes)
                                    ));
                                } else {
                                    ui.label(format!("状态: {}", t.phase));
                                }
                            });
                            if let Some(err) = &t.error {
                                ui.label(
                                    egui::RichText::new(err).color(ui.visuals().error_fg_color),
                                );
                            }
                            ui.add_space(8.0);
                        }
                        if ui.button("清空进度记录").clicked() {
                            self.progresses.clear();
                            self.transfer_meta.clear();
                        }
                    }
                });

                ui.add_space(12.0);
                egui::CollapsingHeader::new("其它用户")
                    .default_open(true)
                    .show(ui, |ui| {
                        ui.label("先选中一个标签或用户，再把文件拖到下方拖拽区发送。");
                        ui.add_space(6.0);
                        Self::card(ui, |ui| {
                            ui.label(if lan_active {
                                "局域网发现：正在尝试发现同一网络中的其他电脑。"
                            } else {
                                "局域网发现：当前未能绑定 45678 端口（可能同机多开端口占用）。"
                            });
                            if peers_by_id.is_empty() {
                                ui.add_space(6.0);
                                ui.label(
                                    "暂无其他用户。请确认对方已打开本应用，且双方在同一网络。",
                                );
                            } else {
                                ui.add_space(6.0);
                                self.ui_peer_tree(ui, &tree, &[]);
                            }

                            ui.add_space(10.0);
                            let selected_text = match &self.selected_target {
                                SelectedTarget::None => "未选择".to_string(),
                                SelectedTarget::User { label, .. } => format!("用户：{label}"),
                                SelectedTarget::Tag { label, targets, .. } => {
                                    format!("标签：{label}（{} 个用户）", targets.len())
                                }
                            };
                            ui.horizontal(|ui| {
                                ui.label(format!("当前选择：{selected_text}"));
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui.button("清除选择").clicked() {
                                            self.selected_target = SelectedTarget::None;
                                        }
                                    },
                                );
                            });

                            ui.add_space(6.0);
                            let (rect, resp) = ui.allocate_exact_size(
                                egui::vec2(ui.available_width().max(1.0), 72.0),
                                egui::Sense::click(),
                            );
                            let frame = egui::Frame::group(ui.style())
                                .rounding(egui::Rounding::same(12.0))
                                .inner_margin(egui::Margin::symmetric(12.0, 12.0));
                            ui.painter().add(frame.paint(rect));
                            self.paint_drop_feedback(ui, rect);
                            ui.painter().text(
                                rect.center(),
                                egui::Align2::CENTER_CENTER,
                                "拖拽文件到此处发送（或点击选择…）",
                                egui::FontId::proportional(16.0),
                                ui.visuals().text_color(),
                            );

                            if resp.clicked() {
                                match &self.selected_target {
                                    SelectedTarget::None => {
                                        self.status = "请先选择一个标签或用户".to_string();
                                    }
                                    SelectedTarget::User { peer_id, label } => {
                                        if let Some(files) = FileDialog::new().pick_files() {
                                            let paths = files
                                                .into_iter()
                                                .map(|f| f.to_string_lossy().into_owned())
                                                .collect::<Vec<_>>();
                                            if !paths.is_empty() {
                                                self.open_send_dialog_for_user(
                                                    peer_id.clone(),
                                                    label.clone(),
                                                    paths,
                                                );
                                            }
                                        }
                                    }
                                    SelectedTarget::Tag { label, targets, .. } => {
                                        if targets.is_empty() {
                                            self.status = "该标签下没有可发送的用户".to_string();
                                        } else if let Some(files) = FileDialog::new().pick_files() {
                                            let paths = files
                                                .into_iter()
                                                .map(|f| f.to_string_lossy().into_owned())
                                                .collect::<Vec<_>>();
                                            if !paths.is_empty() {
                                                self.open_bulk_send_dialog_for_tag(
                                                    label.clone(),
                                                    targets.clone(),
                                                    paths,
                                                );
                                            }
                                        }
                                    }
                                }
                            }

                            if let Some(pd) = pending_drop.take() {
                                let inside = (!pd.pos.x.is_finite() || !pd.pos.y.is_finite())
                                    || rect.contains(pd.pos);
                                if inside {
                                    match &self.selected_target {
                                        SelectedTarget::None => {
                                            self.status = "请先选择一个标签或用户".to_string();
                                        }
                                        SelectedTarget::User { peer_id, label } => {
                                            self.open_send_dialog_for_user(
                                                peer_id.clone(),
                                                label.clone(),
                                                pd.paths,
                                            );
                                        }
                                        SelectedTarget::Tag { label, targets, .. } => {
                                            if targets.is_empty() {
                                                self.status =
                                                    "该标签下没有可发送的用户".to_string();
                                            } else {
                                                self.open_bulk_send_dialog_for_tag(
                                                    label.clone(),
                                                    targets.clone(),
                                                    pd.paths,
                                                );
                                            }
                                        }
                                    }
                                } else {
                                    pending_drop = Some(pd);
                                }
                            }
                        });
                    });

                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    Self::ui_section_title(ui, "接收记录");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("清空").clicked() {
                            self.events.clear();
                        }
                        if ui.button("另存为…").clicked() {
                            let text = self.format_receive_log(&peers_by_id, true);
                            let path = std::env::temp_dir().join("xtransfer_receive_log.txt");
                            match std::fs::write(&path, text) {
                                Ok(()) => self.status = format!("已保存到 {}", path.display()),
                                Err(e) => self.status = format!("保存失败: {e}"),
                            }
                        }
                        if ui.button("复制").clicked() {
                            ui.output_mut(|o| {
                                o.copied_text = self.format_receive_log(&peers_by_id, false)
                            });
                            self.status = format!("已复制 {} 条", self.events.len());
                        }
                    });
                });

                if self.events.is_empty() {
                    ui.label("暂无");
                } else {
                    for e in self.events.iter().rev().take(20) {
                        ui.add_space(8.0);
                        Self::card(ui, |ui| {
                            ui.horizontal(|ui| {
                                let badge = Self::file_icon_label(&e.file_name);
                                egui::Frame::none()
                                    .fill(ui.visuals().faint_bg_color)
                                    .rounding(egui::Rounding::same(6.0))
                                    .inner_margin(egui::Margin::symmetric(8.0, 4.0))
                                    .show(ui, |ui| {
                                        ui.label(egui::RichText::new(badge).strong());
                                    });

                                ui.label(egui::RichText::new(&e.file_name).strong());
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.label(Self::format_time_ms(e.timestamp_ms));
                                    },
                                );
                            });

                            ui.add_space(6.0);

                            let sender = self.sender_nickname(&peers_by_id, &e.sender_peer_id);
                            if let Some(err) = &e.error {
                                if !err.trim().is_empty() {
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label(egui::RichText::new(sender).strong());
                                        egui::Frame::none()
                                            .fill(ui.visuals().faint_bg_color)
                                            .rounding(egui::Rounding::same(10.0))
                                            .inner_margin(egui::Margin::symmetric(10.0, 6.0))
                                            .show(ui, |ui| {
                                                ui.label(
                                                    egui::RichText::new(Self::one_line_text(err))
                                                        .color(ui.visuals().error_fg_color),
                                                );
                                            });
                                    });
                                    return;
                                }
                            }

                            let msg = Self::one_line_text(&e.message);
                            if msg.is_empty() {
                                ui.label(egui::RichText::new(sender).strong());
                            } else {
                                ui.horizontal_wrapped(|ui| {
                                    ui.label(egui::RichText::new(sender).strong());
                                    egui::Frame::none()
                                        .fill(ui.visuals().faint_bg_color)
                                        .rounding(egui::Rounding::same(10.0))
                                        .inner_margin(egui::Margin::symmetric(10.0, 6.0))
                                        .show(ui, |ui| {
                                            ui.label(msg);
                                        });
                                });
                            }
                        });
                    }
                }
            });
        self.pending_drop = pending_drop;
    }

    fn ui_peer_tree(&mut self, ui: &mut egui::Ui, tree: &PeerTree, path: &[String]) {
        for (tag, child) in &tree.children {
            let resp = egui::CollapsingHeader::new(tag)
                .default_open(true)
                .show(ui, |ui| {
                    let mut p = path.to_vec();
                    p.push(tag.clone());
                    self.ui_peer_tree(ui, child, &p)
                });
            let mut key_parts = path.to_vec();
            key_parts.push(tag.clone());
            let key = key_parts.join("/");
            if matches!(&self.selected_target, SelectedTarget::Tag { key: k, .. } if k == &key) {
                let mut hot_rect = resp.header_response.rect;
                let full = ui.max_rect();
                hot_rect.min.x = full.min.x;
                hot_rect.max.x = full.max.x;
                self.paint_selected_feedback(ui, hot_rect);
            }
            if resp.header_response.clicked() {
                let mut peers = Vec::new();
                self.collect_peers_under_tree(child, &mut peers);
                peers.retain(|p| p.peer_id != self.device_id);
                let targets = peers
                    .into_iter()
                    .map(|p| BulkTarget {
                        label: if p.nickname.trim().is_empty() {
                            format!("{} · {}", p.host, p.peer_id)
                        } else {
                            format!("{} · {}", p.nickname, p.host)
                        },
                        peer_id: p.peer_id,
                    })
                    .collect::<Vec<_>>();
                self.selected_target = SelectedTarget::Tag {
                    key,
                    label: key_parts.join("/"),
                    targets,
                };
            }
        }

        let mut peers = tree.peers.clone();
        peers.sort_by(|a, b| {
            let an = if a.nickname.trim().is_empty() {
                &a.peer_id
            } else {
                &a.nickname
            };
            let bn = if b.nickname.trim().is_empty() {
                &b.peer_id
            } else {
                &b.nickname
            };
            an.cmp(bn)
        });
        for p in peers {
            let remote_ok = p.remote_desktop_port > 0;
            let title = if p.nickname.trim().is_empty() {
                format!("（未设置昵称） · {}", p.host)
            } else {
                format!("{} · {}", p.nickname, p.host)
            };
            let subtitle = if remote_ok {
                format!(
                    "文件服务 {}:{} · 远程协助 {}",
                    p.host, p.file_service_port, p.remote_desktop_port
                )
            } else {
                format!(
                    "文件服务 {}:{} · 远程协助未开启",
                    p.host, p.file_service_port
                )
            };
            let row = ui.horizontal(|ui| {
                let resp = ui.add(egui::Button::new(title).frame(false));
                ui.label(egui::RichText::new(subtitle).weak());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let remote_btn = ui.add_enabled(remote_ok, egui::Button::new("远程协助"));
                    if remote_btn.clicked() && remote_ok {
                        self.disconnect_remote();
                        self.remote_connect_host = p.host.clone();
                        self.remote_connect_port = p.remote_desktop_port.to_string();
                        self.remote_token = LAN_REMOTE_SESSION_TOKEN.to_string();
                        self.remote_pending_connect = Some((
                            p.host.clone(),
                            p.remote_desktop_port,
                            self.remote_token.clone(),
                        ));
                        self.remote_connected = false;
                        self.screen = Screen::RemoteDesktop;
                    }
                });
                resp
            });

            if row.inner.clicked() {
                let label = if p.nickname.trim().is_empty() {
                    p.host.clone()
                } else {
                    p.nickname.clone()
                };
                self.selected_target = SelectedTarget::User {
                    peer_id: p.peer_id.clone(),
                    label,
                };
            }
            if matches!(&self.selected_target, SelectedTarget::User { peer_id, .. } if peer_id == &p.peer_id)
            {
                let mut hot_rect = row.inner.rect;
                let full = ui.max_rect();
                hot_rect.min.x = full.min.x;
                hot_rect.max.x = full.max.x;
                self.paint_selected_feedback(ui, hot_rect);
            }
        }
    }

    fn ui_settings_screen(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                Self::ui_section_title(ui, "昵称");
                Self::card(ui, |ui| {
                    ui.text_edit_singleline(&mut self.nickname);
                });

                ui.add_space(12.0);
                Self::ui_section_title(ui, "分组标签（如会场、区域）");
                Self::card(ui, |ui| {
                    if self.tags.is_empty() {
                        self.tags.push(String::new());
                    }
                    let mut remove_idx = None;
                    for (i, t) in self.tags.iter_mut().enumerate() {
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(t)
                                    .hint_text(format!("标签 {}", i + 1))
                                    .id_source(("tag", i)),
                            );
                            if ui.button("移除").clicked() {
                                remove_idx = Some(i);
                            }
                        });
                    }
                    if let Some(i) = remove_idx {
                        if self.tags.len() > 1 {
                            self.tags.remove(i);
                        } else {
                            self.tags[0].clear();
                        }
                    }
                    if ui.button("添加标签").clicked() {
                        self.tags.push(String::new());
                    }
                });

                ui.add_space(12.0);
                Self::ui_section_title(ui, "接收文件保存位置");
                Self::card(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.text_edit_singleline(&mut self.receive_dir);
                        if ui.button("选择…").clicked() {
                            if let Some(dir) = FileDialog::new().pick_folder() {
                                self.receive_dir = dir.to_string_lossy().into_owned();
                                self.apply_receive_dir();
                            }
                        }
                        if ui.button("应用").clicked() {
                            self.apply_receive_dir();
                        }
                        if ui.button("打开").clicked() {
                            let p = self.receive_dir.trim();
                            if !p.is_empty() {
                                let _ = std::process::Command::new("explorer").arg(p).spawn();
                            }
                        }
                    });
                    ui.label("请选择一个已存在的文件夹；保存时会检查是否可写入。");
                });

                ui.add_space(12.0);
                Self::ui_section_title(ui, "远程协助");
                Self::card(ui, |ui| {
                    let before = self.remote_host_enabled;
                    ui.checkbox(&mut self.remote_host_enabled, "允许其他人远程控制本电脑");
                    ui.label("开启后，本应用会准备远程协助端口；关闭后他人无法远程连接本机。");
                    if before != self.remote_host_enabled {
                        if self.remote_host_enabled {
                            let tok = LAN_REMOTE_SESSION_TOKEN.to_string();
                            let res = self
                                .rt
                                .block_on(api::remote_host::remote_host_start(tok, None));
                            match res {
                                Ok(port) => {
                                    self.remote_host_port = Some(port);
                                    if let Ok(mut g) = self.lan_payload.lock() {
                                        g.rport = port;
                                    }
                                    let _ = api::config::set_remote_host_enabled(true);
                                    self.status = format!("远程协助已开启: {port}");
                                }
                                Err(e) => {
                                    self.remote_host_enabled = false;
                                    let _ = api::config::set_remote_host_enabled(false);
                                    self.status = format!("开启失败: {} {}", e.code, e.message);
                                }
                            }
                        } else {
                            let res = self.rt.block_on(api::remote_host::remote_host_stop());
                            match res {
                                Ok(()) => {
                                    self.remote_host_port = None;
                                    if let Ok(mut g) = self.lan_payload.lock() {
                                        g.rport = 0;
                                    }
                                    let _ = api::config::set_remote_host_enabled(false);
                                    self.status = "远程协助已关闭".to_string();
                                }
                                Err(e) => {
                                    self.status = format!("关闭失败: {} {}", e.code, e.message)
                                }
                            }
                        }
                    }
                });

                ui.add_space(18.0);
                ui.horizontal(|ui| {
                    if ui.button("保存并应用").clicked() {
                        self.tags = self
                            .tags
                            .iter()
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect();
                        self.save_profile();
                        self.apply_receive_dir();
                        let _ = api::config::set_remote_host_enabled(self.remote_host_enabled);
                        self.screen = Screen::Home;
                    }
                });
            });
    }

    fn ui_troubleshooting_screen(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                Self::ui_section_title(ui, "收不到别人发的文件");
                Self::card(ui, |ui| {
                    ui.label("• 请确认双方连的是同一个 Wi‑Fi（或同一有线局域网）。");
                    ui.label("• 在「设置」里选好「接收文件保存位置」，并点「保存并应用」。");
                    ui.label("• Windows 若弹出防火墙询问，请选择「允许」。");
                    ui.label("• 若仍不行，可把网络设为「专用网络」后再试。");
                });

                ui.add_space(12.0);
                Self::ui_section_title(ui, "列表里看不到其他电脑");
                Self::card(ui, |ui| {
                    ui.label("• 对方也需要打开本应用。");
                    ui.label("• 部分路由器会开启「访客网络」或「设备隔离」，会导致互相看不到，请换到普通局域网或关闭该功能。");
                    ui.label("• 若两台电脑曾复制过同一台机器的系统镜像，可能出现设备识别冲突，需分别清理本应用数据目录中的设备标识文件后重启。");
                });

                ui.add_space(12.0);
                Self::ui_section_title(ui, "远程协助连不上");
                Self::card(ui, |ui| {
                    ui.label("• 被协助的一方需要在「设置」中打开「允许其他人远程控制本电脑」。");
                    ui.label("• 协助方在列表里点「远程协助」；若按钮为灰色，说明对方未开启远程协助。");
                    ui.label("• 操作系统若询问屏幕录制、辅助功能等权限，请按提示允许。");
                });
            });
    }

    fn ui_about_screen(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                Self::ui_section_title(ui, "内网传输工具");
                Self::card(ui, |ui| {
                    ui.label("版本 1.0.0+1");
                    ui.add_space(8.0);
                    ui.label("这是做什么的？");
                    ui.label("在会议室或同一 Wi‑Fi 下，方便地把文件发给同事，也可以在对方允许时远程查看或操作对方电脑（远程协助）。");
                });
            });
    }

    fn format_time_ms(ms: i64) -> String {
        let ms = ms.max(0);
        let dt = Local
            .timestamp_millis_opt(ms)
            .single()
            .unwrap_or_else(|| Local.timestamp_millis_opt(0).unwrap());
        dt.format("%H:%M:%S").to_string()
    }

    fn format_receive_log(&self, peers_by_id: &BTreeMap<String, PeerInfoDto>, tsv: bool) -> String {
        let mut buf = String::new();
        if tsv {
            buf.push_str("文件名\t用户信息\t留言\ttimestamp_ms\n");
            for e in &self.events {
                let sender = self
                    .sender_display_line(peers_by_id, &e.sender_peer_id)
                    .replace('\t', " ");
                let msg = e.message.replace('\t', " ");
                buf.push_str(&format!(
                    "{}\t{}\t{}\t{}\n",
                    e.file_name, sender, msg, e.timestamp_ms
                ));
            }
        } else {
            for e in &self.events {
                let sender = self.sender_display_line(peers_by_id, &e.sender_peer_id);
                buf.push_str(&format!("{} | {} | {}\n", e.file_name, sender, e.message));
            }
        }
        buf
    }
}

impl eframe::App for LanTransferApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.sync_runtime_state(ctx);

        if self.last_screen == Screen::RemoteDesktop && self.screen != Screen::RemoteDesktop {
            if self.fullscreen_active {
                ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(
                    self.remote_prev_maximized.unwrap_or(false),
                ));
                self.fullscreen_active = false;
                self.remote_prev_maximized = None;
            }
            self.disconnect_remote();
        }

        if self.screen == Screen::RemoteDesktop {
            self.ui_remote_desktop_fullscreen(ctx);
            self.last_screen = self.screen;
            ctx.request_repaint_after(Duration::from_millis(33));
            return;
        }

        if self.fullscreen_active {
            ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(
                self.remote_prev_maximized.unwrap_or(false),
            ));
            self.fullscreen_active = false;
            self.remote_prev_maximized = None;
        }

        self.init_root_viewport(ctx);

        self.ui_app_bar(ctx);
        egui::CentralPanel::default().show(ctx, |ui| match self.screen {
            Screen::Home => self.ui_home_screen(ui),
            Screen::Settings => self.ui_settings_screen(ui),
            Screen::Troubleshooting => self.ui_troubleshooting_screen(ui),
            Screen::About => self.ui_about_screen(ui),
            Screen::RemoteDesktop => {}
        });

        self.ui_send_dialog(ctx);
        self.ui_bulk_send_dialog(ctx);
        self.last_screen = self.screen;
        ctx.request_repaint_after(Duration::from_millis(33));
    }
}

fn main() -> eframe::Result<()> {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size(egui::vec2(1000.0, 720.0)),
        ..Default::default()
    };
    eframe::run_native(
        "LanTransfer",
        opts,
        Box::new(|cc| Ok(Box::new(LanTransferApp::new(cc)))),
    )
}
