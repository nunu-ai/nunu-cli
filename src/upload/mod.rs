pub mod multipart;
pub mod single;

use crate::api::client::BuildDetails;
use crate::config::Config;
use crate::error::Result;
use indicatif::ProgressBar;
use std::sync::Arc;

const MAX_SINGLE_PART_SIZE: u64 = 3 * 1024 * 1024 * 1024; // 3GB

/// Callback function type for upload initiation
pub type OnUploadInitiated = Arc<dyn Fn(String, Option<String>, String) + Send + Sync>;

/// Options for uploading a file
#[derive(Clone)]
pub struct UploadOptions {
    pub name: String,
    pub platform: String,
    pub description: Option<String>,
    pub upload_timeout: Option<u32>,
    pub auto_delete: bool,
    pub deletion_policy: Option<String>,
    pub force_multipart: bool,
    pub parallel: usize,
    /// Optional callback invoked when upload is initiated with `(build_id, upload_id, object_key)`
    pub on_upload_initiated: Option<OnUploadInitiated>,
    /// Optional progress bar for tracking upload progress
    pub progress_bar: Option<ProgressBar>,
    /// Optional build details (VCS, CI/CD metadata)
    pub details: Option<BuildDetails>,
    /// Optional tags for the build
    pub tags: Option<Vec<String>>,
}

impl std::fmt::Debug for UploadOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UploadOptions")
            .field("name", &self.name)
            .field("platform", &self.platform)
            .field("description", &self.description)
            .field("upload_timeout", &self.upload_timeout)
            .field("auto_delete", &self.auto_delete)
            .field("deletion_policy", &self.deletion_policy)
            .field("force_multipart", &self.force_multipart)
            .field("parallel", &self.parallel)
            .field("on_upload_initiated", &self.on_upload_initiated.is_some())
            .field("progress_bar", &self.progress_bar.is_some())
            .field("details", &self.details.is_some())
            .field("tags", &self.tags.is_some())
            .finish()
    }
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
