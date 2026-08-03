//! Alda Music Agent 的本地服务与持久化运行时。
//!
//! crate 提供版本化命令协议、有界 App Service、仅回环 HTTP/Artifact 端点、
//! 同源 PWA、可恢复 WebSocket 事件流，以及 production durable composition root。
//! 内存 backend 仅用于显式测试入口；真实 `serve` 只使用持久化 backend。

pub mod app_service;
pub mod artifact_store;
mod control_store;
pub mod domain;
pub mod durable_runtime;
pub mod http;
pub mod production_runtime;
pub mod protocol;
pub mod state;
pub mod state_store;
