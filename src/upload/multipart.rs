use crate::api::{Client, client::UploadedPart};
use crate::config::Config;
use crate::error::Result;
use crate::upload::UploadOptions;
use indicatif::{ProgressBar, ProgressStyle};
use log::{debug, info};
use std::path::Path;

/// Uploads a file using multipart upload.
///
/// # Errors
///
/// Returns an error if:
/// - The file path is invalid or cannot be converted to a filename
/// - File reading fails
/// - Network requests fail (initiate, part URLs request, part upload, or completion request)
/// - API calls return error responses
///
/// # Panics
///
/// Panics if the progress bar template string is invalid (which should not happen with the hardcoded template).
pub async fn upload_multipart(
    config: &Config,
    file_path: &str,
    file_size: u64,
    options: UploadOptions,
) -> Result<String> {
    let filename = Path::new(file_path)
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| crate::error::Error::ConfigError("Invalid filename".to_string()))?;

    info!(
        "Uploading {} ({:.2} MB) using multipart upload",
        filename,
        file_size / 1024 / 1024
    );

    let client = Client::new(config.clone());

    // Step 1: Initiate multipart upload
    let initiate_response = client
        .initiate_multipart_upload(
            &options.name,
            filename,
            file_size,
            &options.platform,
            options.description,
            options.upload_timeout,
            options.auto_delete,
            options.deletion_policy,
        )
        .await?;

    info!(
        "Multipart upload initiated - {} parts of {} MB each",
        initiate_response.total_parts,
        initiate_response.part_size / 1024 / 1024
    );

    // Read the entire file
    let file_data = tokio::fs::read(file_path).await?;

    // Create progress bar
    let pb = ProgressBar::new(file_size);
    #[allow(clippy::expect_used)]
    pb.set_style(
        ProgressStyle::default_bar()
            .template(
                "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})"
            )
            .expect("Failed to set progress bar template")
            .progress_chars("#>-"),
    );

    // Step 2: Upload parts
    // Process parts in batches to avoid too many concurrent requests
    // Use the parallel setting from options to control batch size

    let mut uploaded_parts: Vec<UploadedPart> = Vec::new();
    let part_size = initiate_response.part_size;
    let total_parts = initiate_response.total_parts;
    let batch_size = options.parallel;

    for batch_start in (1..=total_parts).step_by(batch_size) {
        let batch_end = (batch_start + batch_size - 1).min(total_parts);
        let part_numbers: Vec<u64> = (batch_start..=batch_end).map(|n| n as u64).collect();

        debug!("Requesting URLs for parts {batch_start}-{batch_end} of {total_parts}");

        // Step 2a: Request presigned URLs for this batch
        let urls_response = client
            .request_part_urls(
                &initiate_response.upload_id,
                &initiate_response.object_key,
                part_numbers.clone(),
            )
            .await?;

        // Step 2b: Upload each part in this batch
        for presigned_url_part in urls_response.presigned_urls {
            let part_number = presigned_url_part.part_number;
            let part_url = presigned_url_part.url;

            // Calculate part data boundaries
            #[allow(clippy::cast_possible_truncation)]
            let start = ((part_number - 1) as usize) * part_size;
            let end = (start + part_size).min(file_data.len());
            let part_data = file_data[start..end].to_vec();

            debug!("Uploading part {} ({} bytes)", part_number, part_data.len());

            // Upload the part
            let etag = client.upload_part(&part_url, part_data.clone()).await?;

            // Store the uploaded part info
            uploaded_parts.push(UploadedPart { part_number, etag });

            // Update progress
            pb.inc(part_data.len() as u64);

            info!("Part {part_number} uploaded successfully");
        }
    }

    pb.finish_with_message("All parts uploaded");

    // Sort parts by part number (required by S3)
    uploaded_parts.sort_by_key(|p| p.part_number);

    info!(
        "Completing multipart upload with {} parts",
        uploaded_parts.len()
    );

    // Step 3: Complete the multipart upload
    client
        .complete_multipart_upload(
            &initiate_response.build_id,
            &initiate_response.upload_id,
            &initiate_response.object_key,
            uploaded_parts,
        )
        .await?;

    info!("Build ID: {}", initiate_response.build_id);

    Ok(initiate_response.build_id)
}
