use crate::api::Client;
use crate::config::Config;
use crate::error::Result;
use crate::upload::UploadOptions;
use indicatif::{ProgressBar, ProgressStyle};
use log::info;
use std::path::Path;

/// Uploads a single file part to the server.
///
/// # Errors
///
/// Returns an error if:
/// - The file path is invalid or cannot be converted to a filename
/// - File reading fails
/// - Network requests fail (upload URL request, file upload, or completion request)
/// - API calls return error responses
///
/// # Panics
///
/// Panics if the progress bar template string is invalid (which should not happen with the hardcoded template).
pub async fn upload_single_part(
    config: &Config,
    file_path: &str,
    file_size: u64,
    options: UploadOptions,
) -> Result<String> {
    let filename = Path::new(file_path)
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| crate::error::Error::ConfigError("Invalid filename".to_string()))?;

    info!("Uploading {} ({:.2} MB)", filename, file_size / 1024 / 1024);

    let client = Client::new(config.clone());

    let upload_response = client
        .request_upload_url(
            &options.name,
            filename,
            file_size,
            &options.platform,
            options.description.clone(),
            options.upload_timeout,
            options.auto_delete,
            options.deletion_policy.clone(),
            options.details.clone(),
            options.tags.clone(),
        )
        .await?;

    // Notify about upload initiation
    if let Some(callback) = &options.on_upload_initiated {
        callback(
            upload_response.build_id.clone(),
            None,
            upload_response.object_key.clone(),
        );
    }

    let file_data = tokio::fs::read(file_path).await?;

    // Use provided progress bar or create a new one
    let pb = if let Some(pb) = options.progress_bar.clone() {
        pb.set_length(file_size);
        pb.set_message(format!("Uploading {filename}"));
        pb
    } else {
        let pb = ProgressBar::new(file_size);
        #[allow(clippy::expect_used)]
        pb.set_style(
            ProgressStyle::default_bar()
                .template(
                    "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta}) {msg}"
                )
                .expect("Failed to set progress bar template")
                .progress_chars("#>-"),
        );
        pb
    };

    // Upload with progress tracking
    let pb_clone = pb.clone();
    client
        .upload_to_url_with_progress(&upload_response.upload_url, file_data, move |uploaded| {
            pb_clone.set_position(uploaded);
        })
        .await?;

    pb.finish_with_message("Upload complete");

    client.complete_upload(&upload_response.build_id).await?;

    info!("Build ID: {}", upload_response.build_id);

    Ok(upload_response.build_id)
}
