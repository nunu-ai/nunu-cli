use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("API request failed: {0}")]
    ApiError(String),

    #[error("File error: {0}")]
    FileError(#[from] std::io::Error),

    #[error("HTTP error: {0}")]
    HttpError(#[from] reqwest::Error),

    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),

    #[error("Invalid configuration: {0}")]
    ConfigError(String),

    #[error("Upload failed: {0}")]
    UploadError(String),
}

pub type Result<T> = std::result::Result<T, Error>;
