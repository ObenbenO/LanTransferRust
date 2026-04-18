//! 远程桌面控制端：XRDS 连接 + RGBA 帧轮询 + 输入发送。

use super::types::{ApiError, RemoteKeyEventDto, RemotePointerEventDto, VideoFrameDto};
use crate::app_state::with_state_mut;
use crate::remote_desktop::{
    client_connect, client_disconnect, client_send_key_bin, client_send_pointer_bin,
    client_try_take_frame,
};

pub async fn remote_client_connect(
    host: String,
    port: u16,
    session_token: String,
) -> Result<(), ApiError> {
    let tok = session_token.trim().to_string();
    if tok.is_empty() {
        return Err(ApiError::new("INVALID_TOKEN", "session_token 不能为空"));
    }
    with_state_mut(|s| {
        if !s.initialized {
            return Err(ApiError::new("NOT_INITIALIZED", "请先完成 Rust 初始化"));
        }
        Ok(())
    })?;
    client_connect(&host, port, &tok)?;
    with_state_mut(|s| {
        s.remote_client_peer = Some((host, port));
        s.remote_client_token = Some(tok);
        Ok(())
    })
}

pub async fn remote_client_disconnect() -> Result<(), ApiError> {
    client_disconnect();
    with_state_mut(|s| {
        s.remote_client_peer = None;
        s.remote_client_token = None;
        Ok(())
    })
}

pub fn remote_client_try_take_rgba_frame() -> Option<VideoFrameDto> {
    client_try_take_frame()
}

pub fn remote_client_send_pointer(event: RemotePointerEventDto) -> Result<(), ApiError> {
    with_state_mut(|s| {
        if s.remote_client_peer.is_none() {
            return Err(ApiError::new("CLIENT_NOT_CONNECTED", "控制端未连接"));
        }
        Ok(())
    })?;
    client_send_pointer_bin(&event)
}

pub fn remote_client_send_key(event: RemoteKeyEventDto) -> Result<(), ApiError> {
    with_state_mut(|s| {
        if s.remote_client_peer.is_none() {
            return Err(ApiError::new("CLIENT_NOT_CONNECTED", "控制端未连接"));
        }
        Ok(())
    })?;
    client_send_key_bin(event.key_code, event.down, event.modifiers)
}
