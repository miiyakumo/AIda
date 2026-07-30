//! First implementation slice for the Alda Music Agent.
//!
//! This crate currently provides a versioned command protocol, a bounded
//! single-writer application service, and a loopback HTTP adapter. Persistence,
//! WebSocket streaming, PWA bootstrap, providers, and music tools are later
//! slices and are intentionally not represented as complete here.

pub mod app_service;
pub mod http;
pub mod protocol;
