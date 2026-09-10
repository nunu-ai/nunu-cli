use super::LocalTool;
use crate::{api::Client, config::Config};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use rmcp::{
    ErrorData,
    model::{CallToolResponse, CallToolResult, ContentBlock, JsonObject, Tool, ToolAnnotations},
};
use serde::Deserialize;
use std::{
    collections::HashSet,
    fs,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

pub(super) const NAME: &str = "download_run_files";

pub(super) struct DownloadRunFilesTool {
    config: Config,
    allowed_root: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DownloadRunFilesInput {
    project_id: String,
    run_id: String,
    path: String,
    files: Option<Vec<RunFileSelector>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunFileSelector {
    run_id: String,
    file_ref: String,
}

#[derive(Debug, Deserialize)]
struct RunDetails {
    players: Vec<RunPlayer>,
}

#[derive(Debug, Deserialize)]
struct RunPlayer {
    id: String,
    player_number: u64,
    artifacts: Vec<RunArtifact>,
}

#[derive(Debug, Deserialize)]
struct RunArtifact {
    filename: String,
    url: String,
    size: u64,
}

struct PlannedDownload {
    player_number: u64,
    composite_run_id: String,
    artifact_path: String,
    url: String,
    expected_size: u64,
    destination: PathBuf,
    temporary: PathBuf,
}

impl DownloadRunFilesTool {
    pub(super) fn new(config: Config, allowed_root: PathBuf) -> Self {
        Self {
            config,
            allowed_root,
        }
    }

    async fn download(&self, input: DownloadRunFilesInput) -> Result<CallToolResult> {
        validate_input(&input)?;
        let requested_artifacts = requested_run_files(&input)?;
        let config = Config::with_credential(
            self.config.credential.clone(),
            &self.config.api_url,
            Some(input.project_id.trim().to_string()),
        )?;
        let client = Client::new(config)?;
        let destination_root = self.prepare_destination(&input.path).await?;
        let run = client
            .get_run(input.run_id.trim())
            .await
            .context("failed to fetch run files")?;
        let run: RunDetails = serde_json::from_value(run)
            .context("the run response did not contain the expected artifact list")?;
        let downloads = self
            .plan_downloads(&destination_root, run, requested_artifacts.as_ref())
            .await?;
        let mut cleanup = DownloadCleanup::new(&downloads);

        let mut completed = Vec::with_capacity(downloads.len());
        for download in &downloads {
            match client
                .download_to_file(&download.url, &download.temporary)
                .await
            {
                Ok(bytes) if bytes == download.expected_size => completed.push(bytes),
                Ok(bytes) => {
                    anyhow::bail!(
                        "artifact '{}' was incomplete: expected {} bytes, downloaded {bytes}",
                        download.artifact_path,
                        download.expected_size
                    );
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to download artifact '{}'", download.artifact_path)
                    });
                }
            }
        }

        for download in &downloads {
            if let Err(error) = finalize_download(&download.temporary, &download.destination).await
            {
                return Err(error).with_context(|| {
                    format!(
                        "failed to finalize downloaded artifact '{}'",
                        download.artifact_path
                    )
                });
            }
            cleanup.record_finalized(download.destination.clone());
        }

        let files: Vec<_> = downloads
            .iter()
            .zip(completed.iter())
            .map(|(download, bytes)| {
                serde_json::json!({
                    "player_number": download.player_number,
                    "composite_run_id": download.composite_run_id,
                    "file_ref": format!("artifact:{}", download.artifact_path),
                    "artifact_path": download.artifact_path,
                    "local_path": download.destination.to_string_lossy(),
                    "size_bytes": bytes,
                })
            })
            .collect();
        let total_bytes: u64 = completed.iter().sum();
        let file_count = files.len();
        let structured = serde_json::json!({
            "status": "downloaded",
            "project_id": input.project_id.trim(),
            "run_id": input.run_id.trim(),
            "destination": destination_root.to_string_lossy(),
            "file_count": file_count,
            "total_bytes": total_bytes,
            "files": files,
        });
        let mut result = CallToolResult::success(vec![ContentBlock::text(format!(
            "Downloaded {file_count} run artifact(s) ({total_bytes} bytes) to '{}'.",
            destination_root.display()
        ))]);
        result.structured_content = Some(structured);
        cleanup.disarm();
        Ok(result)
    }

    async fn prepare_destination(&self, requested: &str) -> Result<PathBuf> {
        let requested = Path::new(requested);
        let relative = if requested.is_absolute() {
            resolve_absolute_destination(&self.allowed_root, requested).await?
        } else {
            requested.to_path_buf()
        };

        let mut destination = self.allowed_root.clone();
        for component in relative.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(component) => {
                    destination.push(component);
                    match tokio::fs::metadata(&destination).await {
                        Ok(metadata) => anyhow::ensure!(
                            metadata.is_dir(),
                            "the download destination '{}' is not a directory",
                            destination.display()
                        ),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            tokio::fs::create_dir(&destination).await.with_context(|| {
                                format!(
                                    "failed to create download directory '{}'",
                                    destination.display()
                                )
                            })?;
                        }
                        Err(error) => return Err(error.into()),
                    }
                    destination = tokio::fs::canonicalize(&destination).await?;
                    anyhow::ensure!(
                        destination.starts_with(&self.allowed_root),
                        "the download destination is outside the MCP server's allowed directory"
                    );
                }
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    anyhow::bail!("the download destination cannot contain parent traversal")
                }
            }
        }
        Ok(destination)
    }

    async fn plan_downloads(
        &self,
        destination_root: &Path,
        run: RunDetails,
        requested_artifacts: Option<&HashSet<(String, PathBuf)>>,
    ) -> Result<Vec<PlannedDownload>> {
        let mut selected_artifacts = Vec::new();
        let mut missing_artifacts = requested_artifacts.cloned().unwrap_or_default();
        for player in run.players {
            for artifact in player.artifacts {
                let relative = safe_artifact_path(&artifact.filename)?;
                let stable_ref = (player.id.clone(), relative.clone());
                if requested_artifacts.is_some_and(|files| !files.contains(&stable_ref)) {
                    continue;
                }
                missing_artifacts.remove(&stable_ref);
                selected_artifacts.push((
                    player.player_number,
                    player.id.clone(),
                    artifact,
                    relative,
                ));
            }
        }
        if !missing_artifacts.is_empty() {
            let mut missing: Vec<_> = missing_artifacts
                .iter()
                .map(|(run_id, path)| format!("{run_id}/artifact:{}", path.to_string_lossy()))
                .collect();
            missing.sort_unstable();
            anyhow::bail!(
                "requested run file(s) were not found: {}",
                missing.join(", ")
            );
        }

        let mut downloads = Vec::new();
        let mut destinations = HashSet::new();
        for (player_number, composite_run_id, artifact, relative) in selected_artifacts {
            let player_relative = PathBuf::from(format!("player-{player_number}")).join(relative);
            let file_name = player_relative
                .file_name()
                .context("artifact filename is empty")?;
            let parent_relative = player_relative
                .parent()
                .context("artifact destination has no parent directory")?;
            let parent = prepare_child_directory(destination_root, parent_relative)
                .await
                .with_context(|| {
                    format!(
                        "failed to prepare local directory for artifact '{}'",
                        artifact.filename
                    )
                })?;
            let destination = parent.join(file_name);
            anyhow::ensure!(
                destinations.insert(destination.clone()),
                "multiple run artifacts resolve to the same local path '{}'",
                destination.display()
            );
            anyhow::ensure!(
                !tokio::fs::try_exists(&destination).await?,
                "refusing to overwrite existing path '{}'",
                destination.display()
            );
            let temporary = temporary_path(&destination, downloads.len());
            anyhow::ensure!(
                !tokio::fs::try_exists(&temporary).await?,
                "temporary download path already exists: '{}'",
                temporary.display()
            );
            downloads.push(PlannedDownload {
                player_number,
                composite_run_id,
                artifact_path: artifact.filename,
                url: artifact.url,
                expected_size: artifact.size,
                destination,
                temporary,
            });
        }
        Ok(downloads)
    }
}

#[async_trait]
impl LocalTool for DownloadRunFilesTool {
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
            serde_json::from_value::<DownloadRunFilesInput>(serde_json::Value::Object(arguments))
                .map_err(|error| {
                ErrorData::invalid_params(format!("invalid {NAME} arguments: {error}"), None)
            })?;
        let result = match self.download(input).await {
            Ok(result) => result,
            Err(error) => CallToolResult::error(vec![ContentBlock::text(format!(
                "Run file download failed: {error:#}"
            ))]),
        };
        Ok(result.into())
    }
}

fn validate_input(input: &DownloadRunFilesInput) -> Result<()> {
    anyhow::ensure!(
        !input.project_id.trim().is_empty(),
        "project_id cannot be empty"
    );
    anyhow::ensure!(!input.run_id.trim().is_empty(), "run_id cannot be empty");
    anyhow::ensure!(!input.path.trim().is_empty(), "path cannot be empty");
    if let Some(files) = &input.files {
        anyhow::ensure!(!files.is_empty(), "files cannot be empty");
    }
    Ok(())
}

fn requested_run_files(
    input: &DownloadRunFilesInput,
) -> Result<Option<HashSet<(String, PathBuf)>>> {
    let Some(files) = &input.files else {
        return Ok(None);
    };
    let mut requested = HashSet::with_capacity(files.len());
    for file in files {
        let run_id = file.run_id.trim();
        anyhow::ensure!(!run_id.is_empty(), "files[].run_id cannot be empty");
        let artifact_path = file.file_ref.strip_prefix("artifact:").with_context(|| {
            format!(
                "invalid file_ref '{}': only artifact:<path> references can be downloaded",
                file.file_ref
            )
        })?;
        let safe = safe_artifact_path(artifact_path)
            .with_context(|| format!("invalid file_ref '{}'", file.file_ref))?;
        anyhow::ensure!(
            requested.insert((run_id.to_string(), safe)),
            "files contains a duplicate selector: run_id '{run_id}', file_ref '{}'",
            file.file_ref
        );
    }
    Ok(Some(requested))
}

fn safe_artifact_path(filename: &str) -> Result<PathBuf> {
    anyhow::ensure!(!filename.is_empty(), "artifact filename cannot be empty");
    let path = Path::new(filename);
    anyhow::ensure!(
        !path.is_absolute(),
        "artifact path cannot be absolute: '{filename}'"
    );
    let mut safe = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(component) => safe.push(component),
            _ => anyhow::bail!("artifact path contains unsafe traversal: '{filename}'"),
        }
    }
    anyhow::ensure!(
        safe.file_name().is_some(),
        "artifact filename cannot be empty"
    );
    Ok(safe)
}

fn temporary_path(destination: &Path, index: usize) -> PathBuf {
    let name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("artifact");
    destination.with_file_name(format!(
        ".{name}.nunu-download-{}-{index}.part",
        std::process::id()
    ))
}

struct DownloadCleanup {
    temporary: Vec<PathBuf>,
    finalized: Vec<PathBuf>,
    armed: bool,
}

impl DownloadCleanup {
    fn new(downloads: &[PlannedDownload]) -> Self {
        Self {
            temporary: downloads
                .iter()
                .map(|download| download.temporary.clone())
                .collect(),
            finalized: Vec::with_capacity(downloads.len()),
            armed: true,
        }
    }

    fn record_finalized(&mut self, path: PathBuf) {
        self.finalized.push(path);
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for DownloadCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        for path in &self.temporary {
            let _ = fs::remove_file(path);
        }
        for path in &self.finalized {
            let _ = fs::remove_file(path);
        }
    }
}

struct CreatedFileCleanup {
    path: PathBuf,
    armed: bool,
}

impl CreatedFileCleanup {
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CreatedFileCleanup {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

async fn finalize_download(temporary: &Path, destination: &Path) -> std::io::Result<()> {
    match fs::hard_link(temporary, destination) {
        Ok(()) => {
            let mut destination_cleanup = CreatedFileCleanup::new(destination);
            tokio::fs::remove_file(temporary).await?;
            destination_cleanup.disarm();
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Err(error),
        Err(_) => copy_without_overwrite(temporary, destination).await,
    }
}

async fn copy_without_overwrite(temporary: &Path, destination: &Path) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt as _;

    let source = fs::File::open(temporary)?;
    let destination_file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let mut destination_cleanup = CreatedFileCleanup::new(destination);
    let mut source = tokio::fs::File::from_std(source);
    let mut destination_file = tokio::fs::File::from_std(destination_file);
    tokio::io::copy(&mut source, &mut destination_file).await?;
    destination_file.flush().await?;
    drop(destination_file);
    tokio::fs::remove_file(temporary).await?;
    destination_cleanup.disarm();
    Ok(())
}

async fn resolve_absolute_destination(allowed_root: &Path, requested: &Path) -> Result<PathBuf> {
    anyhow::ensure!(
        !requested
            .components()
            .any(|component| matches!(component, Component::ParentDir)),
        "the download destination cannot contain parent traversal"
    );

    let mut existing = requested.to_path_buf();
    let canonical_existing = loop {
        match tokio::fs::canonicalize(&existing).await {
            Ok(path) => break path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                anyhow::ensure!(
                    existing.pop(),
                    "the download destination has no existing ancestor"
                );
            }
            Err(error) => return Err(error.into()),
        }
    };
    let unresolved = requested.strip_prefix(&existing)?;
    let relative_existing = canonical_existing
        .strip_prefix(allowed_root)
        .with_context(|| {
            format!(
                "the download destination is outside the MCP server's allowed directory '{}': {}",
                allowed_root.display(),
                requested.display()
            )
        })?;
    Ok(relative_existing.join(unresolved))
}

async fn prepare_child_directory(root: &Path, relative: &Path) -> Result<PathBuf> {
    let mut directory = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            anyhow::bail!("artifact directory contains unsafe traversal")
        };
        directory.push(component);
        match tokio::fs::metadata(&directory).await {
            Ok(metadata) => anyhow::ensure!(
                metadata.is_dir(),
                "artifact directory '{}' is not a directory",
                directory.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tokio::fs::create_dir(&directory).await?;
            }
            Err(error) => return Err(error.into()),
        }
        directory = tokio::fs::canonicalize(&directory).await?;
        anyhow::ensure!(
            directory.starts_with(root),
            "artifact directory is outside the download destination"
        );
    }
    Ok(directory)
}

fn definition() -> Tool {
    let input_schema = serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "project_id": {
                "type": "string",
                "minLength": 1,
                "description": "ID of the Nunu project containing the run."
            },
            "run_id": {
                "type": "string",
                "minLength": 1,
                "description": "ID of the multiplayer run whose artifacts should be downloaded."
            },
            "path": {
                "type": "string",
                "minLength": 1,
                "description": "Local destination directory, absolute or relative to the configured MCP workspace root. It is created when needed. Artifacts are stored below player-<number>/ directories. Existing files are never overwritten."
            },
            "files": {
                "type": "array",
                "minItems": 1,
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "run_id": {
                            "type": "string",
                            "minLength": 1,
                            "description": "Composite run ID returned by inspect_run."
                        },
                        "file_ref": {
                            "type": "string",
                            "pattern": "^artifact:.+",
                            "description": "Stable artifact:<path> file reference returned by inspect_run."
                        }
                    },
                    "required": ["run_id", "file_ref"]
                },
                "description": "Optional stable run-file selectors returned by inspect_run. Omit this field to download every artifact in the run."
            }
        },
        "required": ["project_id", "run_id", "path"]
    });
    let output_schema = serde_json::json!({
        "type": "object",
        "properties": {
            "status": { "type": "string", "const": "downloaded" },
            "project_id": { "type": "string" },
            "run_id": { "type": "string" },
            "destination": { "type": "string" },
            "file_count": { "type": "integer" },
            "total_bytes": { "type": "integer" },
            "files": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "player_number": { "type": "integer" },
                        "composite_run_id": { "type": "string" },
                        "file_ref": { "type": "string" },
                        "artifact_path": { "type": "string" },
                        "local_path": { "type": "string" },
                        "size_bytes": { "type": "integer" }
                    },
                    "required": ["player_number", "composite_run_id", "file_ref", "artifact_path", "local_path", "size_bytes"]
                }
            }
        },
        "required": ["status", "project_id", "run_id", "destination", "file_count", "total_bytes", "files"]
    });
    Tool::new(
        NAME,
        "Download selected artifact files, or all artifact files, from a Nunu run into the local workspace. Artifact directories are preserved beneath one directory per player, and existing files are not overwritten.",
        input_schema.as_object().cloned().unwrap_or_default(),
    )
    .with_title("Download Nunu run files")
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
    fn schema_has_only_the_required_public_inputs() {
        let tool = definition();
        let properties = tool.input_schema["properties"]
            .as_object()
            .expect("input properties");
        let mut names: Vec<_> = properties.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, ["files", "path", "project_id", "run_id"]);
        assert_eq!(
            tool.input_schema["required"],
            serde_json::json!(["project_id", "run_id", "path"])
        );
        assert_eq!(
            tool.annotations.as_ref().and_then(|a| a.read_only_hint),
            Some(false)
        );
    }

    #[test]
    fn artifact_paths_must_be_relative_and_cannot_traverse() {
        assert_eq!(
            safe_artifact_path("deliverables/report.json").expect("safe path"),
            PathBuf::from("deliverables/report.json")
        );
        assert!(safe_artifact_path("../secret").is_err());
        assert!(safe_artifact_path("/etc/passwd").is_err());
        assert!(safe_artifact_path("").is_err());
    }

    #[test]
    fn selected_files_use_get_run_file_identifiers() {
        let input = DownloadRunFilesInput {
            project_id: "project_123".to_string(),
            run_id: "multiplayer_123".to_string(),
            path: "downloads".to_string(),
            files: Some(vec![RunFileSelector {
                run_id: "composite_123-1".to_string(),
                file_ref: "artifact:logs/output.txt".to_string(),
            }]),
        };

        let selected = requested_run_files(&input)
            .expect("parse selectors")
            .expect("selected-file mode");

        assert!(selected.contains(&(
            "composite_123-1".to_string(),
            PathBuf::from("logs/output.txt")
        )));
    }

    #[test]
    fn selected_files_reject_non_artifact_refs() {
        let input = DownloadRunFilesInput {
            project_id: "project_123".to_string(),
            run_id: "multiplayer_123".to_string(),
            path: "downloads".to_string(),
            files: Some(vec![RunFileSelector {
                run_id: "composite_123-1".to_string(),
                file_ref: "event_123#img0".to_string(),
            }]),
        };

        assert!(requested_run_files(&input).is_err());
    }

    #[tokio::test]
    async fn plans_only_files_matching_stable_run_and_file_refs() {
        let root = tempfile::tempdir().expect("create destination root");
        let config =
            Config::new("secret".to_string(), "http://localhost:3000/api").expect("create config");
        let tool = DownloadRunFilesTool::new(
            config,
            root.path().canonicalize().expect("canonicalize root"),
        );
        let run = RunDetails {
            players: vec![
                RunPlayer {
                    id: "composite_123-1".to_string(),
                    player_number: 1,
                    artifacts: vec![RunArtifact {
                        filename: "logs/output.txt".to_string(),
                        url: "https://example.com/player-1".to_string(),
                        size: 1,
                    }],
                },
                RunPlayer {
                    id: "composite_123-2".to_string(),
                    player_number: 2,
                    artifacts: vec![RunArtifact {
                        filename: "logs/output.txt".to_string(),
                        url: "https://example.com/player-2".to_string(),
                        size: 2,
                    }],
                },
            ],
        };
        let selected = HashSet::from([(
            "composite_123-2".to_string(),
            PathBuf::from("logs/output.txt"),
        )]);

        let downloads = tool
            .plan_downloads(root.path(), run, Some(&selected))
            .await
            .expect("plan selected file");

        assert_eq!(downloads.len(), 1);
        assert_eq!(downloads[0].composite_run_id, "composite_123-2");
        assert_eq!(downloads[0].artifact_path, "logs/output.txt");
        assert!(
            downloads[0]
                .destination
                .ends_with("player-2/logs/output.txt")
        );
    }

    #[tokio::test]
    async fn destination_is_created_inside_allowed_root() {
        let root = tempfile::tempdir().expect("create allowed root");
        let tool = DownloadRunFilesTool::new(
            Config::new("secret".to_string(), "http://localhost:3000/api").expect("create config"),
            root.path().canonicalize().expect("canonicalize root"),
        );

        let destination = tool
            .prepare_destination("downloads/run-1")
            .await
            .expect("create destination");
        assert_eq!(
            destination,
            root.path()
                .join("downloads/run-1")
                .canonicalize()
                .expect("canonicalize destination")
        );
        assert!(tool.prepare_destination("../outside").await.is_err());
    }

    #[tokio::test]
    async fn absolute_destination_with_missing_children_is_resolved_inside_allowed_root() {
        let root = tempfile::tempdir().expect("create allowed root");
        let tool = DownloadRunFilesTool::new(
            Config::new("secret".to_string(), "http://localhost:3000/api").expect("create config"),
            root.path().canonicalize().expect("canonicalize root"),
        );
        let requested = root.path().join("downloads/run-absolute");

        let destination = tool
            .prepare_destination(requested.to_str().expect("UTF-8 test path"))
            .await
            .expect("create absolute destination");

        assert_eq!(
            destination,
            requested
                .canonicalize()
                .expect("canonicalize absolute destination")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn destination_rejects_symlinks_outside_allowed_root() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("create allowed root");
        let outside = tempfile::tempdir().expect("create outside root");
        symlink(outside.path(), root.path().join("escape")).expect("create symlink");
        let tool = DownloadRunFilesTool::new(
            Config::new("secret".to_string(), "http://localhost:3000/api").expect("create config"),
            root.path().canonicalize().expect("canonicalize root"),
        );

        assert!(tool.prepare_destination("escape/downloads").await.is_err());
        assert!(!outside.path().join("downloads").exists());
    }

    #[tokio::test]
    async fn downloads_run_artifacts_with_the_configured_credential() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let artifact_url = format!("http://{address}/runs/composite_1/artifacts/logs/output.txt");
        let run_body = serde_json::json!({
            "players": [{
                "id": "composite_1",
                "player_number": 1,
                "artifacts": [{
                    "filename": "logs/output.txt",
                    "url": artifact_url,
                    "size": 7
                }]
            }]
        })
        .to_string();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.expect("accept request");
                let mut request = vec![0_u8; 8192];
                let read = socket.read(&mut request).await.expect("read request");
                let request = String::from_utf8_lossy(&request[..read]);
                assert!(
                    request.to_ascii_lowercase().contains("x-api-key: secret"),
                    "request did not contain the API key: {request}"
                );
                let (status, content_type, body) = if request
                    .starts_with("GET /api/v1/project/project_123/runs/run_123 ")
                {
                    ("200 OK", "application/json", run_body.as_bytes())
                } else if request.starts_with("GET /runs/composite_1/artifacts/logs/output.txt ") {
                    ("200 OK", "application/octet-stream", b"content".as_slice())
                } else {
                    ("404 Not Found", "text/plain", b"not found".as_slice())
                };
                let headers = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                socket
                    .write_all(headers.as_bytes())
                    .await
                    .expect("write response headers");
                socket.write_all(body).await.expect("write response body");
            }
        });

        let root = tempfile::tempdir().expect("create allowed root");
        let tool = DownloadRunFilesTool::new(
            Config::new("secret".to_string(), format!("http://{address}/api"))
                .expect("create config"),
            root.path().canonicalize().expect("canonicalize root"),
        );
        let result = tool
            .download(DownloadRunFilesInput {
                project_id: "project_123".to_string(),
                run_id: "run_123".to_string(),
                path: "downloads".to_string(),
                files: None,
            })
            .await
            .expect("download run files");

        server.await.expect("test server completes");
        assert_eq!(
            std::fs::read(root.path().join("downloads/player-1/logs/output.txt"))
                .expect("read downloaded artifact"),
            b"content"
        );
        assert_eq!(
            result.structured_content.expect("structured")["file_count"],
            1
        );
    }

    #[tokio::test]
    async fn download_does_not_follow_authenticated_redirects() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let redirect_target = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind redirect target");
        let target_address = redirect_target
            .local_addr()
            .expect("redirect target address");
        let artifact_server = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind artifact server");
        let artifact_address = artifact_server
            .local_addr()
            .expect("artifact server address");
        let source = tokio::spawn(async move {
            let (mut socket, _) = artifact_server.accept().await.expect("accept request");
            let mut request = vec![0_u8; 4096];
            let read = socket.read(&mut request).await.expect("read request");
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.to_ascii_lowercase().contains("x-api-key: secret"));
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: http://{target_address}/leak\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .expect("write redirect");
        });
        let client = Client::new(
            Config::new(
                "secret".to_string(),
                format!("http://{artifact_address}/api"),
            )
            .expect("create config"),
        )
        .expect("create client");
        let root = tempfile::tempdir().expect("create destination root");

        let result = client
            .download_to_file(
                &format!("http://{artifact_address}/artifact"),
                &root.path().join("artifact"),
            )
            .await;

        source.await.expect("artifact server completes");
        assert!(result.is_err(), "redirect response must not be followed");
        assert!(!root.path().join("artifact").exists());
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                redirect_target.accept()
            )
            .await
            .is_err(),
            "redirect target unexpectedly received the authenticated request"
        );
    }

    #[tokio::test]
    async fn cancellation_removes_partial_temporary_files() {
        let root = tempfile::tempdir().expect("create destination root");
        let temporary = root.path().join("artifact.part");
        let download = PlannedDownload {
            player_number: 1,
            composite_run_id: "composite_1".to_string(),
            artifact_path: "artifact".to_string(),
            url: "https://example.com/artifact".to_string(),
            expected_size: 1,
            destination: root.path().join("artifact"),
            temporary: temporary.clone(),
        };
        let (created_tx, created_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _cleanup = DownloadCleanup::new(&[download]);
            tokio::fs::write(&temporary, b"partial")
                .await
                .expect("write partial file");
            created_tx.send(()).expect("signal partial file creation");
            std::future::pending::<()>().await;
        });
        created_rx.await.expect("wait for partial file");
        let temporary = root.path().join("artifact.part");
        assert!(temporary.exists());

        task.abort();
        let _ = task.await;

        assert!(!temporary.exists());
    }

    #[tokio::test]
    async fn copy_finalization_never_overwrites_an_existing_destination() {
        let root = tempfile::tempdir().expect("create destination root");
        let temporary = root.path().join("artifact.part");
        let destination = root.path().join("artifact");
        tokio::fs::write(&temporary, b"new")
            .await
            .expect("write temporary file");
        tokio::fs::write(&destination, b"existing")
            .await
            .expect("write existing destination");

        let error = copy_without_overwrite(&temporary, &destination)
            .await
            .expect_err("existing destination must be preserved");

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            tokio::fs::read(&destination)
                .await
                .expect("read destination"),
            b"existing"
        );
        assert!(temporary.exists());
    }
}
