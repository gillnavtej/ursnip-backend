//! Library root for the ursnip-backend crate.
//!
//! This module re-exports internal modules to make them accessible from
//! integration tests in the `tests/` directory.

pub mod admin;
pub mod auth;
pub mod billing;
pub mod collaborators;
pub mod config;
pub mod db;
pub mod devices;
pub mod email;
pub mod errors;
pub mod logging;
pub mod middleware;
pub mod models;
pub mod notifications;
pub mod router;
pub mod scheduler;
pub mod sharing;
pub mod subscription;
pub mod sync;
pub mod workspace;

pub use errors::AppError;
