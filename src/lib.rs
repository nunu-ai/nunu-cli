//! Nunu CLI library for uploading build artifacts

pub mod ci_metadata;
pub mod config;
pub mod error;
pub mod mcp;
pub mod metadata;
mod tls;

pub mod auth;

pub mod api;
pub mod upload;

pub use config::Config;
pub use error::{Error, Result};

// Re-export commonly used types
pub use api::{BuildPlatform, Client, DeletionPolicy};
pub use upload::{UploadOptions, upload_file};
