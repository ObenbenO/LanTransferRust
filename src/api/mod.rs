//! X传输工具 — Rust API。
//!
//! 网络与远控核心协议当前为占位实现，配置 / 对端列表 / 目录校验已可用。

pub mod types;

pub mod config;
pub mod peer;
pub mod remote_client;
pub mod remote_host;
pub mod transfer;
