//! Bonsoir 发现结果注入与对端列表维护。

use super::types::{ApiError, PeerInfoDto};
use crate::app_state::{with_state, with_state_mut};

pub fn register_discovered_peer(peer: PeerInfoDto) -> Result<(), ApiError> {
    with_state_mut(|s| {
        if !s.initialized {
            return Err(ApiError::new("NOT_INITIALIZED", "请先完成 Rust 初始化"));
        }
        if peer.peer_id.trim().is_empty() {
            return Err(ApiError::new("INVALID_PEER", "peer_id 不能为空"));
        }
        s.peers.insert(peer.peer_id.clone(), peer);
        Ok(())
    })
}

pub fn unregister_peer(peer_id: String) -> Result<(), ApiError> {
    with_state_mut(|s| {
        s.peers.remove(&peer_id);
        if s.active_peer_id.as_ref() == Some(&peer_id) {
            s.active_peer_id = None;
        }
        Ok(())
    })
}

pub fn list_known_peers() -> Result<Vec<PeerInfoDto>, ApiError> {
    with_state(|s| {
        if !s.initialized {
            return Err(ApiError::new("NOT_INITIALIZED", "请先完成 Rust 初始化"));
        }
        Ok(s.peers.values().cloned().collect())
    })
}

pub fn clear_all_peers() -> Result<(), ApiError> {
    with_state_mut(|s| {
        s.peers.clear();
        s.active_peer_id = None;
        Ok(())
    })
}

/// 将后续 `send_files` 默认对端设为该 peer（仍可在请求里显式指定）。
pub fn connect_peer(peer_id: String) -> Result<(), ApiError> {
    with_state_mut(|s| {
        if !s.initialized {
            return Err(ApiError::new("NOT_INITIALIZED", "请先完成 Rust 初始化"));
        }
        if !s.peers.contains_key(&peer_id) {
            return Err(ApiError::new(
                "PEER_UNKNOWN",
                format!("未知 peer_id: {peer_id}"),
            ));
        }
        s.active_peer_id = Some(peer_id);
        Ok(())
    })
}
