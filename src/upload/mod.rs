pub mod multipart;
pub mod single;

use crate::config::Config;
use crate::error::Result;

const MAX_SINGLE_PART_SIZE: u64 = 3 * 1024 * 1024 * 1024; // 3GB

/// Options for uploading a file
#[derive(Debug, Clone)]
pub struct UploadOptions {
    pub name: String,
    pub platform: String,
    pub description: Option<String>,
    pub upload_timeout: Option<u32>,
    pub auto_delete: bool,
    pub deletion_policy: Option<String>,
    pub force_multipart: bool,
    pub parallel: usize,
}

/// Upload a file to Nunu.ai
///
/// # Errors
///
/// Returns an error if:
/// - The file cannot be read or accessed
/// - The upload operation fails
pub async fn upload_file(
    config: &Config,
    file_path: &str,
    options: UploadOptions,
) -> Result<String> {
    let file_metadata = tokio::fs::metadata(file_path).await?;
    let file_size = file_metadata.len();

    if options.force_multipart || file_size > MAX_SINGLE_PART_SIZE {
        multipart::upload_multipart(config, file_path, file_size, options).await
    } else {
        single::upload_single_part(config, file_path, file_size, options).await
    }
}
