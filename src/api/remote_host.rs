//! 远程桌面被控端：XRDS 协议（截屏 xcap + 键鼠 enigo）。

use super::types::ApiError;
use crate::app_state::with_state_mut;
use crate::remote_desktop::spawn_remote_desktop_listener;

pub async fn remote_host_start(
    session_token: String,
    preferred_port: Option<u16>,
) -> Result<u16, ApiError> {
    let token = session_token.trim().to_string();
    if token.is_empty() {
        return Err(ApiError::new("INVALID_TOKEN", "session_token 不能为空"));
    }
    with_state_mut(|s| {
        if !s.initialized {
            return Err(ApiError::new("NOT_INITIALIZED", "请先完成 Rust 初始化"));
        }
        if s.remote_host_service.is_some() {
            return Err(ApiError::new(
                "HOST_RUNNING",
                "远程桌面宿主已在运行，请先 remote_host_stop",
            ));
        }
        let (port, handle) = spawn_remote_desktop_listener(preferred_port, token.clone())?;
        s.remote_host_token = Some(token);
        s.remote_host_port = Some(port);
        s.remote_host_service = Some(handle);
        Ok(port)
    })
}

pub async fn remote_host_stop() -> Result<(), ApiError> {
    with_state_mut(|s| {
        s.remote_host_service.take();
        s.remote_host_port = None;
        s.remote_host_token = None;
        Ok(())
    })
}

/// 模拟客户端发来的输入（JSON 或后续二进制协议）；当前仅校验 token 占位。
pub fn remote_host_dispatch_input(
    session_token: String,
    _payload_json: String,
) -> Result<(), ApiError> {
    with_state_mut(|s| {
        let Some(t) = &s.remote_host_token else {
            return Err(ApiError::new("HOST_NOT_RUNNING", "远程宿主未启动"));
        };
        if t != &session_token {
            return Err(ApiError::new("TOKEN_MISMATCH", "会话令牌不匹配"));
        }
        // TODO: 解析 payload，经 enigo 注入键鼠
        Ok(())
    })
}
