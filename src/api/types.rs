//! DTO 与错误类型。

/// 业务错误，便于按 `code` 做分支提示。
#[derive(Clone, Debug)]
pub struct ApiError {
    pub code: String,
    pub message: String,
}

impl ApiError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// 本地身份（昵称 + 有序标签路径，如 会场 / 片区）。
#[derive(Clone, Debug, Default)]
pub struct LocalProfileDto {
    pub nickname: String,
    pub tags: Vec<String>,
}

/// Bonsoir 发现后注入 Rust 的对端信息。
#[derive(Clone, Debug)]
pub struct PeerInfoDto {
    pub peer_id: String,
    pub instance_id: String,
    pub nickname: String,
    pub tags: Vec<String>,
    pub host: String,
    pub file_service_port: u16,
    pub remote_desktop_port: u16,
}

/// 发送文件请求。
#[derive(Clone, Debug)]
pub struct SendFilesRequestDto {
    pub target_peer_id: String,
    pub file_paths: Vec<String>,
    pub message: String,
}

/// 接收端事件（文件名 + 留言等），供 UI 轮询或后续改为 Stream。
#[derive(Clone, Debug)]
pub struct FileReceiveEventDto {
    pub file_name: String,
    pub message: String,
    pub sender_peer_id: String,
    pub saved_path: Option<String>,
    pub error: Option<String>,
    pub timestamp_ms: i64,
}

/// 发送进度（占位；完整协议落地后由后台任务推送）。
#[derive(Clone, Debug)]
pub struct TransferProgressDto {
    pub transfer_id: String,
    pub bytes_sent: i64,
    pub total_bytes: i64,
    pub phase: String,
    pub error: Option<String>,
}

/// 控制端指针事件（坐标建议为被控端分辨率下的逻辑坐标，具体约定在协议文档中细化）。
#[derive(Clone, Debug)]
pub struct RemotePointerEventDto {
    pub kind: String,
    pub x: f64,
    pub y: f64,
    pub button: i32,
    pub delta: f64,
    /// 与 [RemoteKeyEventDto::modifiers] 相同位约定：Shift=1, Ctrl=2, Alt=4, Meta/Win=8。
    pub modifiers: i32,
}

#[derive(Clone, Debug)]
pub struct RemoteKeyEventDto {
    pub key_code: i32,
    pub down: bool,
    pub modifiers: i32,
}

/// 一帧 RGBA 裸数据（当前占位返回空）。
#[derive(Clone, Debug)]
pub struct VideoFrameDto {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}
