//! 文件收发：TCP 监听 + 协议 v1 多文件发送。

use std::path::Path;

use uuid::Uuid;

use super::types::{ApiError, FileReceiveEventDto, SendFilesRequestDto, TransferProgressDto};
use crate::app_state::{
    now_ms_pub, push_progress, push_receive_event, spawn_file_listener, with_state, with_state_mut,
};
use crate::file_transfer::send_files_blocking;

pub async fn file_service_start(preferred_port: Option<u16>) -> Result<u16, ApiError> {
    with_state_mut(|s| {
        if !s.initialized {
            return Err(ApiError::new("NOT_INITIALIZED", "请先完成 Rust 初始化"));
        }
        if s.file_service.is_some() {
            return Err(ApiError::new(
                "ALREADY_LISTENING",
                "文件服务已在运行，请先 file_service_stop",
            ));
        }
        let (port, handle) = spawn_file_listener(preferred_port)?;
        s.file_listen_port = Some(port);
        s.file_service = Some(handle);
        Ok(port)
    })
}

pub async fn file_service_stop() -> Result<(), ApiError> {
    with_state_mut(|s| {
        s.file_service.take();
        s.file_listen_port = None;
        Ok(())
    })
}

pub async fn send_files(req: SendFilesRequestDto) -> Result<String, ApiError> {
    let target = if req.target_peer_id.trim().is_empty() {
        with_state(|s| {
            s.active_peer_id.clone().ok_or_else(|| {
                ApiError::new("NO_TARGET", "未指定 target_peer_id 且未 connect_peer")
            })
        })?
    } else {
        req.target_peer_id.clone()
    };

    let (host, port) = with_state(|s| {
        let p = s
            .peers
            .get(&target)
            .ok_or_else(|| ApiError::new("PEER_UNKNOWN", format!("未知 peer: {target}")))?;
        Ok((p.host.clone(), p.file_service_port))
    })?;

    for fp in &req.file_paths {
        let p = Path::new(fp);
        if !p.exists() {
            return Err(ApiError::new("FILE_NOT_FOUND", format!("文件不存在: {fp}")));
        }
        if !p.is_file() {
            return Err(ApiError::new("NOT_A_FILE", format!("路径不是文件: {fp}")));
        }
    }

    let transfer_id = Uuid::new_v4().to_string();
    let total: i64 = req
        .file_paths
        .iter()
        .filter_map(|fp| std::fs::metadata(fp).ok().map(|m| m.len() as i64))
        .sum();

    let sender_id = with_state(|s| s.device_id.clone());
    let paths = req.file_paths.clone();
    let message = req.message.clone();
    let host_owned = host.clone();

    let tid = transfer_id.clone();
    tokio::spawn(async move {
        let tid_for_blocking = tid.clone();
        let join = tokio::task::spawn_blocking(move || {
            send_files_blocking(
                &host_owned,
                port,
                &sender_id,
                &message,
                &paths,
                &tid_for_blocking,
                total,
            )
        });
        match join.await {
            Ok(Ok(())) => {
                with_state_mut(|s| {
                    push_progress(
                        s,
                        TransferProgressDto {
                            transfer_id: tid.clone(),
                            bytes_sent: total,
                            total_bytes: total,
                            phase: "done".to_string(),
                            error: None,
                        },
                    );
                });
            }
            Ok(Err(e)) => {
                with_state_mut(|s| {
                    push_progress(
                        s,
                        TransferProgressDto {
                            transfer_id: tid.clone(),
                            bytes_sent: 0,
                            total_bytes: total,
                            phase: "failed".to_string(),
                            error: Some(e.message.clone()),
                        },
                    );
                });
            }
            Err(_) => {
                with_state_mut(|s| {
                    push_progress(
                        s,
                        TransferProgressDto {
                            transfer_id: tid.clone(),
                            bytes_sent: 0,
                            total_bytes: total,
                            phase: "failed".to_string(),
                            error: Some("发送任务线程异常结束".to_string()),
                        },
                    );
                });
            }
        }
    });

    Ok(transfer_id)
}

pub fn cancel_transfer(_transfer_id: String) -> Result<(), ApiError> {
    // 占位：正式实现中取消后台任务句柄
    Ok(())
}

pub fn pull_file_receive_events() -> Vec<FileReceiveEventDto> {
    with_state_mut(|s| std::mem::take(&mut s.receive_events))
}

pub fn pull_transfer_progress() -> Vec<TransferProgressDto> {
    with_state_mut(|s| std::mem::take(&mut s.transfer_progress))
}

/// 测试用：模拟本机收到一条文件记录（单机无对端时供 UI 联调）。
pub fn debug_push_mock_receive_event(
    file_name: String,
    message: String,
    sender_peer_id: String,
) -> Result<(), ApiError> {
    with_state_mut(|s| {
        if !s.initialized {
            return Err(ApiError::new("NOT_INITIALIZED", "请先完成 Rust 初始化"));
        }
        let saved = s
            .receive_path
            .as_ref()
            .map(|p| p.join(&file_name))
            .map(|p| p.to_string_lossy().into_owned());
        push_receive_event(
            s,
            FileReceiveEventDto {
                file_name,
                message,
                sender_peer_id,
                saved_path: saved,
                error: None,
                timestamp_ms: now_ms_pub(),
            },
        );
        Ok(())
    })
}
