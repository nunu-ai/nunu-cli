use super::LocalTool;
use crate::{
    api::{
        Client,
        client::{BuildDetails, BuildPlatform, UploadInfo},
    },
    ci_metadata::collect_ci_metadata,
    config::Config,
    metadata::collect_git_metadata,
    upload::{UploadOptions, upload_file},
};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use rmcp::{
    ErrorData,
    model::{CallToolResponse, CallToolResult, ContentBlock, JsonObject, Tool, ToolAnnotations},
};
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub(super) const NAME: &str = "upload_build";
const DEFAULT_PARALLELISM: usize = 4;

pub(super) struct UploadBuildTool {
    config: Config,
    allowed_root: PathBuf,
}

#[derive(Clone)]
struct InitiatedUpload {
    build_id: String,
    upload_id: Option<String>,
    object_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UploadBuildInput {
    path: String,
    name: String,
    project_id: String,
    platform: Option<BuildPlatform>,
    tags: Option<Vec<String>>,
}

impl UploadBuildTool {
    pub(super) fn new(config: Config, allowed_root: PathBuf) -> Self {
        Self {
            config,
            allowed_root,
        }
    }

    async fn upload(&self, input: UploadBuildInput) -> Result<CallToolResult> {
        validate_input(&input)?;
        let config = Config::with_credential(
            self.config.credential.clone(),
            &self.config.api_url,
            Some(input.project_id.clone()),
        )?;
        let path = self.validate_path(&input.path).await?;
        let path_text = path
            .to_str()
            .context("the build path must contain valid Unicode")?;
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .context("the build file name must contain valid Unicode")?
            .to_string();
        let platform = input
            .platform
            .clone()
            .map_or_else(|| infer_platform(&path), Ok)?;
        let initiated = Arc::new(Mutex::new(None::<InitiatedUpload>));
        let initiated_for_callback = Arc::clone(&initiated);
        let mut options = build_options(&input, &platform);
        options.on_upload_initiated = Some(Arc::new(move |build_id, upload_id, object_key| {
            if let Ok(mut active) = initiated_for_callback.lock() {
                *active = Some(InitiatedUpload {
                    build_id,
                    upload_id,
                    object_key,
                });
            }
        }));
        let build_id = match upload_file(&config, path_text, options).await {
            Ok(build_id) => build_id,
            Err(error) => {
                Self::abort_if_initiated(&config, &initiated).await;
                return Err(error.into());
            }
        };
        let structured = serde_json::json!({
            "status": "uploaded",
            "build_id": build_id,
            "project_id": input.project_id,
            "name": input.name,
            "file_name": file_name,
        });
        let mut result = CallToolResult::success(vec![ContentBlock::text(format!(
            "Uploaded '{file_name}' as '{}'.",
            input.name
        ))]);
        result.structured_content = Some(structured);
        Ok(result)
    }

    async fn validate_path(&self, requested: &str) -> Result<PathBuf> {
        let requested_path = Path::new(requested);
        let candidate = if requested_path.is_absolute() {
            requested_path.to_path_buf()
        } else {
            self.allowed_root.join(requested_path)
        };
        let path = tokio::fs::canonicalize(&candidate)
            .await
            .with_context(|| format!("cannot access the requested build file '{requested}'"))?;
        let metadata = tokio::fs::metadata(&path)
            .await
            .context("cannot inspect the requested build file")?;
        anyhow::ensure!(
            metadata.is_file(),
            "the requested build path is not a regular file"
        );
        anyhow::ensure!(
            path.starts_with(&self.allowed_root),
            "the requested build file is outside the MCP server's allowed directory"
        );
        Ok(path)
    }

    async fn abort_if_initiated(config: &Config, initiated: &Mutex<Option<InitiatedUpload>>) {
        let active = initiated
            .lock()
            .ok()
            .and_then(|active| active.as_ref().cloned());
        if let Some(active) = active
            && let Ok(client) = Client::new(config.clone())
        {
            let _ = client
                .abort_upload(
                    &active.build_id,
                    active.upload_id.as_deref(),
                    Some(&active.object_key),
                )
                .await;
        }
    }
}

#[async_trait]
impl LocalTool for UploadBuildTool {
    fn name(&self) -> &'static str {
        NAME
    }

    fn definition(&self) -> Tool {
        definition()
    }

    async fn call(
        &self,
        arguments: Option<JsonObject>,
    ) -> std::result::Result<CallToolResponse, ErrorData> {
        let arguments = arguments.unwrap_or_default();
        let input =
            serde_json::from_value::<UploadBuildInput>(serde_json::Value::Object(arguments))
                .map_err(|error| {
                    ErrorData::invalid_params(format!("invalid {NAME} arguments: {error}"), None)
                })?;
        let result = match self.upload(input).await {
            Ok(result) => result,
            Err(error) => CallToolResult::error(vec![ContentBlock::text(format!(
                "Build upload failed: {error}"
            ))]),
        };
        Ok(result.into())
    }
}

fn validate_input(input: &UploadBuildInput) -> Result<()> {
    anyhow::ensure!(!input.path.trim().is_empty(), "path cannot be empty");
    anyhow::ensure!(!input.name.trim().is_empty(), "name cannot be empty");
    anyhow::ensure!(
        !input.project_id.trim().is_empty(),
        "project_id cannot be empty"
    );
    if let Some(tags) = &input.tags {
        for tag in tags {
            anyhow::ensure!(!tag.is_empty(), "tags cannot be empty");
            anyhow::ensure!(
                tag.len() <= 50,
                "tag '{tag}' exceeds the maximum length of 50 characters"
            );
        }
    }
    Ok(())
}

fn infer_platform(path: &Path) -> Result<BuildPlatform> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let platform = match extension.as_str() {
        "exe" | "msi" => BuildPlatform::Windows,
        "dmg" | "pkg" => BuildPlatform::Macos,
        "ipa" => BuildPlatform::IosNative,
        "apk" => BuildPlatform::Android,
        "deb" | "rpm" | "appimage" => BuildPlatform::Linux,
        "app" => {
            anyhow::bail!("platform is required for .app files (use 'macos' or 'ios-simulator')")
        }
        "zip" | "tar" | "gz" | "7z" | "tgz" | "bz2" => {
            anyhow::bail!("platform is required for archive files")
        }
        _ => anyhow::bail!("platform could not be inferred; provide it explicitly"),
    };
    Ok(platform)
}

fn build_options(input: &UploadBuildInput, platform: &BuildPlatform) -> UploadOptions {
    UploadOptions {
        name: input.name.clone(),
        platform: platform.as_str().to_string(),
        description: None,
        upload_timeout: None,
        auto_delete: false,
        deletion_policy: None,
        force_multipart: false,
        parallel: DEFAULT_PARALLELISM,
        on_upload_initiated: None,
        progress_bar: Some(indicatif::ProgressBar::hidden()),
        details: Some(BuildDetails {
            vcs: collect_git_metadata(),
            ci: collect_ci_metadata(),
            upload: Some(UploadInfo {
                method: "mcp".to_string(),
                cli_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                uploader: std::env::var("USER")
                    .ok()
                    .or_else(|| std::env::var("USERNAME").ok()),
            }),
        }),
        tags: input.tags.clone(),
    }
}

fn definition() -> Tool {
    let input_schema = serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "path": {
                "type": "string",
                "minLength": 1,
                "description": "Absolute path to one local build artifact file, or a path relative to the configured MCP workspace root. Globs and directories are not supported."
            },
            "name": {
                "type": "string",
                "minLength": 1,
                "description": "Display name for the build."
            },
            "project_id": {
                "type": "string",
                "minLength": 1,
                "description": "ID of the Nunu project that will receive the build."
            },
            "platform": {
                "type": "string",
                "enum": [
                    "windows", "macos", "linux", "android", "ios-native",
                    "ios-simulator", "xbox", "playstation"
                ],
                "description": "Target platform. May be omitted when it can be inferred from the file extension."
            },
            "tags": {
                "type": "array",
                "items": { "type": "string", "minLength": 1, "maxLength": 50 },
                "description": "Optional tags to attach to the build."
            }
        },
        "required": ["path", "name", "project_id"]
    });
    let output_schema = serde_json::json!({
        "type": "object",
        "properties": {
            "status": { "type": "string", "const": "uploaded" },
            "build_id": { "type": "string" },
            "project_id": { "type": "string" },
            "name": { "type": "string" },
            "file_name": { "type": "string" }
        },
        "required": ["status", "build_id", "project_id", "name", "file_name"]
    });
    Tool::new(
        NAME,
        "Upload one build artifact from the local filesystem to Nunu. The file must be inside the MCP server's working directory.",
        input_schema.as_object().cloned().unwrap_or_default(),
    )
    .with_title("Upload Nunu build")
    .with_raw_output_schema(Arc::new(
        output_schema.as_object().cloned().unwrap_or_default(),
    ))
    .with_annotations(
        ToolAnnotations::new()
            .read_only(false)
            .destructive(false)
            .idempotent(false)
            .open_world(true),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_is_small_and_does_not_expose_internal_metadata() {
        let tool = definition();
        let properties = tool.input_schema["properties"]
            .as_object()
            .expect("input properties");
        let mut names: Vec<_> = properties.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, ["name", "path", "platform", "project_id", "tags"]);
        assert_eq!(
            tool.input_schema["required"],
            serde_json::json!(["path", "name", "project_id"])
        );
        assert_eq!(
            tool.annotations.as_ref().and_then(|a| a.read_only_hint),
            Some(false)
        );
        assert_eq!(
            tool.annotations.as_ref().and_then(|a| a.idempotent_hint),
            Some(false)
        );
        assert_eq!(
            tool.annotations.as_ref().and_then(|a| a.open_world_hint),
            Some(true)
        );
    }

    #[test]
    fn options_mark_the_source_as_mcp_internally() {
        let input = UploadBuildInput {
            path: "app.apk".to_string(),
            name: "Release".to_string(),
            project_id: "project_123".to_string(),
            platform: None,
            tags: Some(vec!["stable".to_string()]),
        };
        let options = build_options(&input, &BuildPlatform::Android);
        let upload = options
            .details
            .and_then(|details| details.upload)
            .expect("upload metadata");
        assert_eq!(upload.method, "mcp");
        assert_eq!(options.tags, input.tags);
    }

    #[test]
    fn infers_only_unambiguous_platforms() {
        assert_eq!(
            infer_platform(Path::new("release.APK"))
                .expect("infer Android")
                .as_str(),
            "android"
        );
        assert!(infer_platform(Path::new("release.zip")).is_err());
        assert!(infer_platform(Path::new("release.unknown")).is_err());
    }

    #[tokio::test]
    async fn path_must_resolve_inside_the_allowed_root() {
        let root = tempfile::tempdir().expect("create allowed root");
        let outside = tempfile::tempdir().expect("create outside root");
        let inside_file = root.path().join("app.apk");
        let outside_file = outside.path().join("app.apk");
        std::fs::write(&inside_file, b"inside").expect("write inside file");
        std::fs::write(&outside_file, b"outside").expect("write outside file");
        let tool = UploadBuildTool::new(
            Config::new("secret".to_string(), "http://localhost:3000/api")
                .expect("create upload config"),
            root.path().canonicalize().expect("canonicalize root"),
        );

        assert_eq!(
            tool.validate_path(inside_file.to_str().expect("inside path"))
                .await
                .expect("allow inside file"),
            inside_file
                .canonicalize()
                .expect("canonicalize inside file")
        );
        assert_eq!(
            tool.validate_path("app.apk")
                .await
                .expect("resolve relative path from allowed root"),
            inside_file
                .canonicalize()
                .expect("canonicalize relative inside file")
        );
        assert!(
            tool.validate_path(outside_file.to_str().expect("outside path"))
                .await
                .is_err()
        );
    }
}
