//! 初始化、本机标识、接收目录与本地身份快照。

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::types::{ApiError, LocalProfileDto};
use crate::app_state::{
    default_portable_cache_dir, validate_receive_dir, with_state, with_state_mut, AppState,
};
use crate::file_transfer::sync_file_receive_dir_cache;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct PersistedSettings {
    nickname: String,
    tags: Vec<String>,
    receive_path: Option<String>,
    remote_host_enabled: bool,
}

fn settings_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join("settings.json")
}

fn to_relative_path(cache_dir: &Path, p: &Path) -> String {
    if let Ok(r) = p.strip_prefix(cache_dir) {
        let s = r.to_string_lossy().replace('\\', "/");
        return s.trim_start_matches('/').to_string();
    }
    p.to_string_lossy().into_owned()
}

fn from_relative_path(cache_dir: &Path, p: &str) -> PathBuf {
    let t = p.trim();
    if t.is_empty() {
        return cache_dir.join("receive");
    }
    let pb = PathBuf::from(t);
    if pb.is_absolute() {
        pb
    } else {
        cache_dir.join(pb)
    }
}

fn load_settings(cache_dir: &Path) -> PersistedSettings {
    let path = settings_path(cache_dir);
    let Ok(s) = fs::read_to_string(path) else {
        return PersistedSettings::default();
    };
    serde_json::from_str::<PersistedSettings>(&s).unwrap_or_default()
}

fn save_settings(cache_dir: &Path, settings: &PersistedSettings) -> Result<(), ApiError> {
    fs::create_dir_all(cache_dir)
        .map_err(|e| ApiError::new("CACHE_DIR_CREATE", format!("无法创建数据目录: {e}")))?;
    let json = serde_json::to_string_pretty(settings)
        .map_err(|e| ApiError::new("SETTINGS_ENCODE", format!("序列化设置失败: {e}")))?;
    fs::write(settings_path(cache_dir), json)
        .map_err(|e| ApiError::new("SETTINGS_WRITE", format!("写入设置失败: {e}")))?;
    Ok(())
}

pub fn init_rust_lib() {
    let mut receive_dir_for_cache: Option<PathBuf> = None;
    with_state_mut(|s| {
        if s.initialized {
            return;
        }
        if s.cache_dir.is_none() {
            let dir = default_portable_cache_dir();
            let _ = fs::create_dir_all(&dir);
            s.cache_dir = Some(dir);
        }
        let cache_dir = s
            .cache_dir
            .clone()
            .unwrap_or_else(default_portable_cache_dir);
        let settings = load_settings(&cache_dir);
        if !settings.nickname.trim().is_empty() || !settings.tags.is_empty() {
            s.profile = Some(LocalProfileDto {
                nickname: settings.nickname.clone(),
                tags: settings.tags.clone(),
            });
        }
        s.remote_host_enabled = settings.remote_host_enabled;

        let receive = settings
            .receive_path
            .as_deref()
            .map(|p| from_relative_path(&cache_dir, p))
            .unwrap_or_else(|| cache_dir.join("receive"));
        let _ = fs::create_dir_all(&receive);
        s.receive_path = Some(receive.clone());
        receive_dir_for_cache = Some(receive);

        s.device_id = AppState::load_or_create_device_id(s.cache_dir.clone());
        s.session_id = Uuid::new_v4().to_string();
        s.initialized = true;
    });
    sync_file_receive_dir_cache(receive_dir_for_cache);
}

/// 配置持久化设备 ID 的目录；不传则使用系统临时目录下的子目录。
pub fn set_rust_cache_dir(path: String) -> Result<(), ApiError> {
    let p = PathBuf::from(path.trim());
    fs::create_dir_all(&p)
        .map_err(|e| ApiError::new("CACHE_DIR_CREATE", format!("无法创建缓存目录: {e}")))?;
    with_state_mut(|s| {
        s.cache_dir = Some(p.clone());
        s.device_id = AppState::load_or_create_device_id(Some(p));
    });
    Ok(())
}

/// 释放监听线程等资源；再次使用需重新初始化。
pub fn shutdown_rust_lib() -> Result<(), ApiError> {
    with_state_mut(|s| {
        s.file_service.take();
        s.remote_host_service.take();
        s.file_listen_port = None;
        s.remote_host_port = None;
        s.remote_host_token = None;
        s.remote_client_peer = None;
        s.remote_client_token = None;
        s.initialized = false;
    });
    Ok(())
}

pub fn device_id() -> Result<String, ApiError> {
    with_state(|s| {
        if !s.initialized {
            return Err(ApiError::new("NOT_INITIALIZED", "请先完成初始化"));
        }
        Ok(s.device_id.clone())
    })
}

pub fn session_id() -> Result<String, ApiError> {
    with_state(|s| {
        if !s.initialized {
            return Err(ApiError::new("NOT_INITIALIZED", "请先完成 Rust 初始化"));
        }
        Ok(s.session_id.clone())
    })
}

/// 生成新的会话 ID（例如用户重新登录）。
pub fn refresh_session_id() -> Result<String, ApiError> {
    with_state_mut(|s| {
        if !s.initialized {
            return Err(ApiError::new("NOT_INITIALIZED", "请先完成 Rust 初始化"));
        }
        s.session_id = Uuid::new_v4().to_string();
        Ok(s.session_id.clone())
    })
}

pub fn set_default_receive_path(path: String) -> Result<(), ApiError> {
    let pb = validate_receive_dir(&path)?;
    let (cache_dir, mut settings) = with_state(|s| {
        (
            s.cache_dir
                .clone()
                .unwrap_or_else(default_portable_cache_dir),
            load_settings(
                &s.cache_dir
                    .clone()
                    .unwrap_or_else(default_portable_cache_dir),
            ),
        )
    });
    with_state_mut(|s| {
        if !s.initialized {
            return Err(ApiError::new("NOT_INITIALIZED", "请先完成 Rust 初始化"));
        }
        s.receive_path = Some(pb.clone());
        Ok(())
    })?;
    settings.receive_path = Some(to_relative_path(&cache_dir, &pb));
    save_settings(&cache_dir, &settings)?;
    sync_file_receive_dir_cache(Some(pb));
    Ok(())
}

pub fn get_default_receive_path() -> Option<String> {
    with_state(|s| {
        s.receive_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
    })
}

pub fn set_local_profile(profile: LocalProfileDto) -> Result<(), ApiError> {
    let cache_dir = with_state(|s| {
        s.cache_dir
            .clone()
            .unwrap_or_else(default_portable_cache_dir)
    });
    let mut settings = load_settings(&cache_dir);
    with_state_mut(|s| {
        if !s.initialized {
            return Err(ApiError::new("NOT_INITIALIZED", "请先完成 Rust 初始化"));
        }
        s.profile = Some(profile.clone());
        Ok(())
    })?;
    settings.nickname = profile.nickname;
    settings.tags = profile.tags;
    save_settings(&cache_dir, &settings)?;
    Ok(())
}

pub fn get_local_profile() -> Option<LocalProfileDto> {
    with_state(|s| s.profile.clone())
}

pub fn set_remote_host_enabled(enabled: bool) -> Result<(), ApiError> {
    let cache_dir = with_state(|s| {
        s.cache_dir
            .clone()
            .unwrap_or_else(default_portable_cache_dir)
    });
    let mut settings = load_settings(&cache_dir);
    with_state_mut(|s| {
        if !s.initialized {
            return Err(ApiError::new("NOT_INITIALIZED", "请先完成 Rust 初始化"));
        }
        s.remote_host_enabled = enabled;
        Ok(())
    })?;
    settings.remote_host_enabled = enabled;
    save_settings(&cache_dir, &settings)?;
    Ok(())
}

pub fn get_remote_host_enabled() -> bool {
    with_state(|s| s.remote_host_enabled)
}
