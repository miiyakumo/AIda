//! A1-A4 development protocol slice for the Alda Music Agent.
//!
//! This crate provides the versioned command protocol, bounded single-writer
//! application service, loopback HTTP and Artifact endpoints, same-origin PWA
//! bootstrap, and resumable WebSocket event streaming implemented through A4.
//! State remains process-local and in memory. Persistence, external providers,
//! music tools, and later production capabilities are not implemented.

pub mod app_service;
pub mod artifact_store;
mod control_store;
pub mod domain;
pub mod durable_runtime;
pub mod http;
pub mod protocol;
pub mod state;
pub mod state_store;
