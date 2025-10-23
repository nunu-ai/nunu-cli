use anyhow::Result;
use clap::{Parser, Subcommand};
use env_logger::Env;
use futures::stream::{self, StreamExt};
use log::{debug, error, info};
use nunu_cli::{BuildPlatform, Config, DeletionPolicy, UploadOptions, upload_file};
use std::path::Path;

#[derive(Parser)]
#[command(name = "nunu-cli")]
#[command(about = "Upload build artifacts to Nunu.ai", long_about = None)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Upload a build artifact
    Upload {
        /// Files to upload (can specify multiple files)
        files: Vec<String>,

        /// API token for authentication
        #[arg(short, long, env = "NUNU_API_TOKEN")]
        token: String,

        /// Project ID
        #[arg(short, long, env = "NUNU_PROJECT_ID")]
        project_id: String,

        /// API base URL
        #[arg(long, default_value = "https://nunu.ai/api", env = "NUNU_API_URL")]
        api_url: String,

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

        /// Force multipart upload (useful for debugging)
        #[arg(long)]
        force_multipart: bool,

        /// Number of parallel uploads/parts (1-32, default: 4)
        #[arg(long, default_value = "4")]
        parallel: usize,
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

    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();

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
        } => {
            if files.is_empty() {
                return Err(anyhow::anyhow!("No files specified for upload"));
            }

            // Validate parallel value
            if !(1..=32).contains(&parallel) {
                return Err(anyhow::anyhow!(
                    "Parallel value must be between 1 and 32, got {parallel}"
                ));
            }

            info!("Using API URL: {api_url}");
            info!("Parallel uploads/parts: {parallel}");
            let config = Config::new(token, project_id, api_url)?;

            let file_count = files.len();

            // Process files in parallel using streams
            let results: Vec<(String, Result<String>)> = stream::iter(files)
                .map(|file_path| {
                    let config = config.clone();
                    let name = name.clone();
                    let platform = platform.clone();
                    let description = description.clone();
                    let deletion_policy = deletion_policy.clone();

                    async move {
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

                        info!(
                            "Uploading {file_path} as {build_name} (platform: {})",
                            file_platform.as_str()
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
                        };

                        let result = upload_file(&config, &file_path, options)
                            .await
                            .map_err(|e| anyhow::anyhow!("{e}"));
                        (file_path, result)
                    }
                })
                .buffer_unordered(parallel)
                .collect()
                .await;

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
