use anyhow::Result;
use clap::{Parser, Subcommand};
use futures::stream::{self, StreamExt};
use glob::glob;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use log::{debug, error, info, warn};
use nunu_cli::{
    BuildPlatform, Client, Config, DeletionPolicy, UploadOptions,
    api::client::{BuildDetails, UploadInfo},
    ci_metadata::collect_ci_metadata,
    file_config::FileConfig,
    metadata::collect_git_metadata,
    upload_file,
};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Tracks active uploads for graceful cancellation
type ActiveUploads = Arc<RwLock<HashMap<String, UploadMetadata>>>;

#[derive(Debug, Clone)]
struct UploadMetadata {
    build_id: String,
    upload_id: Option<String>,
    object_key: String,
}

#[derive(Parser)]
#[command(name = "nunu-cli")]
#[command(about = "Upload build artifacts to Nunu.ai", long_about = None)]
#[command(version)]
struct Cli {
    /// Enable verbose output (shows all logs)
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Path to config file (JSON format)
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Upload a build artifact
    #[command(override_usage = "<FILES>... [OPTIONS]")]
    Upload {
        /// Files to upload (supports glob patterns like *.apk, app?.exe, build[0-9].ipa)
        #[arg(value_name = "FILES", num_args = 1..)]
        files: Vec<String>,

        /// API token for authentication
        #[arg(short, long, env = "NUNU_API_TOKEN")]
        token: Option<String>,

        /// Project ID
        #[arg(short, long, env = "NUNU_PROJECT_ID")]
        project_id: Option<String>,

        /// API base URL
        #[arg(long, env = "NUNU_API_URL")]
        api_url: Option<String>,

        /// Build name (will be used as template for multiple files)
        #[arg(short, long)]
        name: String,

        /// Target platform (optional, can be inferred from file extension)
        #[arg(long, value_parser = clap::value_parser!(BuildPlatform))]
        platform: Option<BuildPlatform>,

        /// Build description (optional)
        #[arg(short, long)]
        description: Option<String>,

        /// Upload timeout in minutes (1-1440, default determined by server)
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=1440))]
        upload_timeout: Option<u32>,

        /// Automatically delete old builds if storage limits are exceeded
        #[arg(long)]
        auto_delete: bool,

        /// Deletion policy when auto-delete is enabled (`least_recent` or `oldest`)
        #[arg(long, default_value = "least_recent", requires = "auto_delete", value_parser = clap::value_parser!(DeletionPolicy))]
        deletion_policy: DeletionPolicy,

        /// Force multipart upload
        #[arg(long)]
        force_multipart: bool,

        /// Number of parallel uploads/parts (1-32, default: 4)
        #[arg(long, default_value = "4")]
        parallel: usize,

        /// Tags for the build (comma-separated, max 50 chars each)
        #[arg(long, value_delimiter = ',')]
        tags: Option<Vec<String>>,
    },
}

/// Infer platform from file extension
///
/// # Errors
///
/// Returns an error if the platform cannot be inferred from the file extension
fn infer_platform(file_path: &str) -> Result<BuildPlatform> {
    let path = Path::new(file_path);
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    match extension.as_str() {
        "exe" | "msi" => Ok(BuildPlatform::Windows),
        "dmg" | "pkg" => Ok(BuildPlatform::Macos),
        "ipa" => Ok(BuildPlatform::IosNative),
        "apk" => Ok(BuildPlatform::Android),
        "deb" | "rpm" | "appimage" => Ok(BuildPlatform::Linux),
        "app" => Err(anyhow::anyhow!(
            "Cannot infer platform for .app files. Please specify --platform explicitly (macos or ios-simulator)"
        )),
        "zip" | "tar" | "gz" | "7z" | "tgz" | "bz2" => Err(anyhow::anyhow!(
            "Cannot infer platform for archive files (.{}). Please specify --platform explicitly",
            extension
        )),
        _ => Err(anyhow::anyhow!(
            "Cannot infer platform from file extension '.{}'. Please specify --platform explicitly",
            extension
        )),
    }
}

/// Generate build name from template and filename
fn generate_build_name(template: &str, file_path: &str, file_count: usize) -> String {
    let filename = Path::new(file_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(file_path);

    if file_count == 1 {
        template.to_string()
    } else {
        format!("{template} - {filename}")
    }
}

/// Expand glob patterns to file paths
///
/// This function handles both regular file paths and glob patterns.
/// If a pattern doesn't match any files, it's treated as a literal path
/// (which will fail later with a clear error).
///
/// # Errors
///
/// Returns an error if glob pattern parsing fails
fn expand_globs(patterns: &[String]) -> Result<Vec<String>> {
    let mut expanded_files = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for pattern in patterns {
        // Check if pattern contains glob characters
        if pattern.contains('*') || pattern.contains('?') || pattern.contains('[') {
            debug!("Expanding glob pattern: {pattern}");

            match glob(pattern) {
                Ok(paths) => {
                    let mut matched_any = false;
                    for entry in paths {
                        match entry {
                            Ok(path) => {
                                matched_any = true;
                                let path_str = path.to_string_lossy().to_string();

                                // Only add files (skip directories) and avoid duplicates
                                if path.is_file() && seen.insert(path_str.clone()) {
                                    expanded_files.push(path_str);
                                }
                            }
                            Err(e) => {
                                warn!("Error reading glob entry: {e}");
                            }
                        }
                    }

                    if matched_any {
                        debug!(
                            "Pattern '{pattern}' matched {} file(s)",
                            expanded_files.len() - (expanded_files.len() - seen.len())
                        );
                    } else {
                        warn!("Pattern '{pattern}' did not match any files");
                    }
                }
                Err(e) => {
                    return Err(anyhow::anyhow!("Invalid glob pattern '{pattern}': {e}"));
                }
            }
        } else {
            // Not a glob pattern, use as-is (deduplicate)
            if seen.insert(pattern.clone()) {
                expanded_files.push(pattern.clone());
            }
        }
    }

    if expanded_files.is_empty() {
        Err(anyhow::anyhow!("No files matched the provided patterns"))
    } else {
        Ok(expanded_files)
    }
}

#[allow(clippy::too_many_lines)]
#[tokio::main]
async fn main() -> Result<()> {
    if let Err(e) = dotenvy::dotenv() {
        if !e.to_string().contains("not found") {
            debug!("Error loading .env file: {e}");
        }
    } else {
        debug!("Loaded environment from .env file");
    }

    let cli = Cli::parse();

    // Initialize logger based on verbose flag
    // 0: warn/error only (clean 2-line display)
    // 1: info level (general progress)
    // 2: debug level (detailed debugging)
    // 3+: trace level (maximum detail)
    if cli.verbose > 0 {
        let log_level = match cli.verbose {
            1 => "info",
            2 => "debug",
            _ => "trace",
        };
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(log_level))
            .format(|buf, record| writeln!(buf, "[{}] {}", record.level(), record.args()))
            .init();
    } else {
        // In non-verbose mode, only show warnings and errors
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    }

    let result: Result<String> = match cli.command {
        Commands::Upload {
            files,
            token,
            project_id,
            api_url,
            name,
            platform,
            description,
            upload_timeout,
            auto_delete,
            deletion_policy,
            force_multipart,
            parallel,
            tags,
        } => {
            if files.is_empty() {
                return Err(anyhow::anyhow!("No files specified for upload"));
            }

            // Expand glob patterns to actual file paths
            let files = expand_globs(&files)?;

            if cli.verbose > 0 {
                info!("Found {} file(s) to upload", files.len());
            }

            // Validate parallel value
            if !(1..=32).contains(&parallel) {
                return Err(anyhow::anyhow!(
                    "Parallel value must be between 1 and 32, got {parallel}"
                ));
            }

            // Validate tags (each tag must be 1-50 characters)
            if let Some(ref tag_list) = tags {
                for tag in tag_list {
                    if tag.is_empty() {
                        return Err(anyhow::anyhow!("Tags cannot be empty"));
                    }
                    if tag.len() > 50 {
                        return Err(anyhow::anyhow!(
                            "Tag '{}' exceeds maximum length of 50 characters (length: {})",
                            tag,
                            tag.len()
                        ));
                    }
                }
            }

            // Load config file with priority:
            // 1. CLI args (highest)
            // 2. Environment variables
            // 3. Config file (--config or default locations)
            let file_config = FileConfig::load_with_fallback(cli.config.as_ref())?;

            // Resolve final values with priority
            let final_token = token
                .or_else(|| std::env::var("NUNU_API_TOKEN").ok())
                .or(file_config.api_token)
                .ok_or_else(|| anyhow::anyhow!("API token not provided (use --token, NUNU_API_TOKEN env var, or config file)"))?;

            let final_project_id = project_id
                .or_else(|| std::env::var("NUNU_PROJECT_ID").ok())
                .or(file_config.project_id)
                .ok_or_else(|| anyhow::anyhow!("Project ID not provided (use --project-id, NUNU_PROJECT_ID env var, or config file)"))?;

            let final_api_url = api_url
                .or_else(|| std::env::var("NUNU_API_URL").ok())
                .or(file_config.api_url)
                .unwrap_or_else(|| "https://nunu.ai/api".to_string());

            let config = Config::new(final_token, final_project_id, final_api_url)?;

            let file_count = files.len();

            // Shared state for tracking active uploads
            let active_uploads: ActiveUploads = Arc::new(RwLock::new(HashMap::new()));

            // Create MultiProgress for coordinated progress display
            let multi_progress = MultiProgress::new();

            // Create a status line for non-verbose mode
            let status_bar = if cli.verbose == 0 {
                let bar = multi_progress.insert(0, ProgressBar::new(0));
                bar.set_style(
                    ProgressStyle::default_bar()
                        .template("{msg}")
                        .unwrap_or_else(|_| ProgressStyle::default_bar()),
                );
                Some(bar)
            } else {
                None
            };

            // Helper to log messages
            let log_message = |msg: String| {
                if let Some(ref bar) = status_bar {
                    bar.set_message(msg);
                } else {
                    info!("{msg}");
                }
            };

            log_message(format!("Using API URL: {}", config.api_url));
            log_message(format!("Parallel uploads/parts: {parallel}"));

            // Collect build metadata
            debug!("Collecting build metadata (VCS and CI/CD)");
            let vcs = collect_git_metadata();
            let ci = collect_ci_metadata();
            let upload_info = Some(UploadInfo {
                method: "cli".to_string(),
                cli_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                uploader: std::env::var("USER")
                    .ok()
                    .or_else(|| std::env::var("USERNAME").ok()),
            });

            let details = if vcs.is_some() || ci.is_some() || upload_info.is_some() {
                Some(BuildDetails {
                    vcs,
                    ci,
                    upload: upload_info,
                })
            } else {
                None
            };

            if let Some(ref d) = details {
                if d.vcs.is_some() {
                    debug!("Collected VCS metadata: Git");
                }
                if d.ci.is_some() {
                    debug!("Collected CI/CD metadata");
                }
            }

            // Set up signal handlers for graceful shutdown
            #[cfg(unix)]
            let mut sigterm = {
                use tokio::signal::unix::{SignalKind, signal};
                signal(SignalKind::terminate()).ok()
            };

            let ctrl_c = tokio::signal::ctrl_c();

            // Process files in parallel using streams
            let verbose = cli.verbose;
            let upload_task = async {
                stream::iter(files)
                    .map(|file_path| {
                        let config = config.clone();
                        let name = name.clone();
                        let platform = platform.clone();
                        let description = description.clone();
                        let deletion_policy = deletion_policy.clone();
                        let active_uploads = active_uploads.clone();
                        let multi_progress = multi_progress.clone();
                        let status_bar = status_bar.clone();
                        let details = details.clone();
                        let tags = tags.clone();

                        async move {
                            // Helper to log messages
                            let log_msg = |msg: String| {
                                if verbose == 0 {
                                    if let Some(ref bar) = status_bar {
                                        bar.set_message(msg);
                                    }
                                } else {
                                    info!("{msg}");
                                }
                            };
                            // Determine platform (explicit or inferred)
                            let file_platform = match &platform {
                                Some(p) => p.clone(),
                                None => match infer_platform(&file_path) {
                                    Ok(p) => p,
                                    Err(e) => {
                                        return (file_path.clone(), Err(e));
                                    }
                                },
                            };

                            // Generate build name
                            let build_name = generate_build_name(&name, &file_path, file_count);

                            // Get file size for progress bar
                            let file_size = match tokio::fs::metadata(&file_path).await {
                                Ok(metadata) => metadata.len(),
                                Err(e) => {
                                    return (file_path.clone(), Err(anyhow::anyhow!("Failed to read file metadata: {e}")));
                                }
                            };

                            // Create progress bar for this upload
                            let pb = multi_progress.add(ProgressBar::new(file_size));
                            pb.set_style(
                                ProgressStyle::default_bar()
                                    .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta}) {msg}")
                                    .unwrap_or_else(|_| ProgressStyle::default_bar())
                                    .progress_chars("#>-"),
                            );
                            pb.set_message(Path::new(&file_path).file_name().and_then(|n| n.to_str()).unwrap_or(&file_path).to_string());

                            log_msg(format!(
                                "Uploading {} as {} (platform: {})",
                                file_path,
                                build_name,
                                file_platform.as_str()
                            ));

                            // Create callback to track upload metadata
                            let file_path_clone = file_path.clone();
                            let active_uploads_clone = active_uploads.clone();
                            let callback = std::sync::Arc::new(
                                move |build_id: String,
                                      upload_id: Option<String>,
                                      object_key: String| {
                                    let file_path = file_path_clone.clone();
                                    let active_uploads = active_uploads_clone.clone();
                                    tokio::spawn(async move {
                                        let mut uploads = active_uploads.write().await;
                                        uploads.insert(
                                            file_path,
                                            UploadMetadata {
                                                build_id,
                                                upload_id,
                                                object_key,
                                            },
                                        );
                                    });
                                },
                            );

                            let options = UploadOptions {
                                name: build_name,
                                platform: file_platform.as_str().to_string(),
                                description: description.clone(),
                                upload_timeout,
                                auto_delete,
                                deletion_policy: Some(deletion_policy.as_str().to_string()),
                                force_multipart,
                                parallel,
                                on_upload_initiated: Some(callback),
                                progress_bar: Some(pb.clone()),
                                details: details.clone(),
                                tags: tags.clone(),
                            };

                            let result = upload_file(&config, &file_path, options)
                                .await
                                .map_err(|e| anyhow::anyhow!("{e}"));

                            // Finish progress bar
                            if result.is_ok() {
                                pb.finish_with_message("✓ Complete");
                            } else {
                                pb.finish_with_message("✗ Failed");
                            }

                            // Remove from active uploads on completion (success or failure)
                            {
                                let mut uploads = active_uploads.write().await;
                                uploads.remove(&file_path);
                            }

                            (file_path, result)
                        }
                    })
                    .buffer_unordered(parallel)
                    .collect::<Vec<(String, Result<String>)>>()
                    .await
            };

            // Wait for either upload completion or termination signal
            #[cfg(unix)]
            let results = {
                tokio::select! {
                    results = upload_task => results,
                    _ = ctrl_c => {
                        eprintln!("\n🛑 Received interrupt signal (SIGINT/Ctrl+C).");

                        // Try to abort all active uploads
                        let uploads = active_uploads.read().await;
                        if !uploads.is_empty() {
                            eprintln!("⏳ Attempting to abort {} active upload(s)...", uploads.len());
                            let client = Client::new(config.clone());

                            for (file_path, metadata) in uploads.iter() {
                                debug!("Aborting upload for {file_path}: build_id={}", metadata.build_id);
                                if let Err(e) = client
                                    .abort_upload(
                                        &metadata.build_id,
                                        metadata.upload_id.as_deref(),
                                        Some(&metadata.object_key),
                                    )
                                    .await
                                {
                                    warn!("Failed to abort upload for {file_path}: {e}");
                                } else {
                                    debug!("Successfully aborted upload for {file_path}");
                                }
                            }
                            eprintln!("✓ Abort requests sent.");
                        }

                        eprintln!("⚠️  Upload cancelled.");
                        std::process::exit(130); // Standard exit code for SIGINT
                    }
                    _ = async {
                        match sigterm.as_mut() {
                            Some(sig) => sig.recv().await,
                            None => std::future::pending().await,
                        }
                    }, if sigterm.is_some() => {
                        eprintln!("\n🛑 Received termination signal (SIGTERM).");

                        // Try to abort all active uploads
                        let uploads = active_uploads.read().await;
                        if !uploads.is_empty() {
                            eprintln!("⏳ Attempting to abort {} active upload(s)...", uploads.len());
                            let client = Client::new(config.clone());

                            for (file_path, metadata) in uploads.iter() {
                                debug!("Aborting upload for {file_path}: build_id={}", metadata.build_id);
                                if let Err(e) = client
                                    .abort_upload(
                                        &metadata.build_id,
                                        metadata.upload_id.as_deref(),
                                        Some(&metadata.object_key),
                                    )
                                    .await
                                {
                                    warn!("Failed to abort upload for {file_path}: {e}");
                                } else {
                                    debug!("Successfully aborted upload for {file_path}");
                                }
                            }
                            eprintln!("✓ Abort requests sent.");
                        }

                        eprintln!("⚠️  Upload terminated.");
                        std::process::exit(143); // Standard exit code for SIGTERM (128 + 15)
                    }
                }
            };

            #[cfg(not(unix))]
            let results = {
                tokio::select! {
                    results = upload_task => results,
                    _ = ctrl_c => {
                        eprintln!("\n🛑 Received interrupt signal (Ctrl+C).");

                        // Try to abort all active uploads
                        let uploads = active_uploads.read().await;
                        if !uploads.is_empty() {
                            eprintln!("⏳ Attempting to abort {} active upload(s)...", uploads.len());
                            let client = Client::new(config.clone());

                            for (file_path, metadata) in uploads.iter() {
                                debug!("Aborting upload for {file_path}: build_id={}", metadata.build_id);
                                if let Err(e) = client
                                    .abort_upload(
                                        &metadata.build_id,
                                        metadata.upload_id.as_deref(),
                                        Some(&metadata.object_key),
                                    )
                                    .await
                                {
                                    warn!("Failed to abort upload for {file_path}: {e}");
                                } else {
                                    debug!("Successfully aborted upload for {file_path}");
                                }
                            }
                            eprintln!("✓ Abort requests sent.");
                        }

                        eprintln!("⚠️  Upload cancelled.");
                        std::process::exit(130); // Standard exit code for SIGINT
                    }
                }
            };

            // Process results
            let mut build_ids = Vec::new();
            let mut errors = Vec::new();

            for (file_path, result) in results {
                match result {
                    Ok(build_id) => {
                        info!("✅ {file_path} uploaded successfully - Build ID: {build_id}");
                        build_ids.push((file_path, build_id));
                    }
                    Err(e) => {
                        errors.push(format!("{file_path}: {e}"));
                    }
                }
            }

            // Report results
            if !build_ids.is_empty() {
                println!("\n✅ Successfully uploaded {} file(s):", build_ids.len());
                for (file, build_id) in &build_ids {
                    println!("  {file} → Build ID: {build_id}");
                }
            }

            if !errors.is_empty() {
                eprintln!("\n❌ Failed to upload {} file(s):", errors.len());
                for error in &errors {
                    eprintln!("  {error}");
                }
                return Err(anyhow::anyhow!("{} file(s) failed to upload", errors.len()));
            }

            Ok(build_ids
                .first()
                .map(|(_, id)| id.clone())
                .unwrap_or_default())
        }
    };

    match result {
        Ok(_) => Ok(()),
        Err(e) => {
            error!("Upload failed: {e}");
            std::process::exit(1);
        }
    }
}
