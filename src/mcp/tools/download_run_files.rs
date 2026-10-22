use super::LocalTool;
use crate::{
    api::{
        Client,
        client::{ArtifactDownload, ArtifactManifest},
    },
    config::Config,
};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use futures::{StreamExt as _, stream};
use rmcp::{
    ErrorData, Peer, RoleClient,
    model::{CallToolResponse, CallToolResult, ContentBlock, JsonObject, Tool, ToolAnnotations},
    schemars::JsonSchema,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

pub(super) const NAME: &str = "download_run_files";
mod input;

pub(super) struct DownloadRunFilesTool {
    config: Config,
    allowed_root: PathBuf,
    upstream: Option<Peer<RoleClient>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(crate = "rmcp::schemars")]
struct DownloadRunFilesInput {
    /// Optional project ID. Inferred from the run link, configuration, or `inspect_run`.
    #[serde(default)]
    project_id: String,
    /// Nunu run link or multiplayer/composite run ID from `list_runs` or `inspect_run`.
    #[serde(default)]
    #[schemars(!default, length(min = 1))]
    run_id: String,
    /// Output folder inside the workspace. Defaults to downloads/<project>/<run>.
    #[serde(default, alias = "path")]
    destination_dir: String,
    /// Omit for all artifacts; filenames select matching files across players.
    #[schemars(length(min = 1, max = 1000))]
    files: Option<Vec<RunFileSelector>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(untagged, deny_unknown_fields)]
#[schemars(crate = "rmcp::schemars", inline)]
enum RunFileSelector {
    Filename(String),
    Selector {
        /// Optional composite run ID to select one player's file.
        #[serde(default)]
        run_id: String,
        /// Stable artifact:<path> reference returned by `inspect_run`.
        file_ref: String,
        /// Optional output path relative to the destination, without a player prefix.
        relative_path: Option<String>,
    },
}

struct PlannedDownload {
    artifact: ArtifactDownload,
    destination: PathBuf,
}

#[derive(Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct DownloadReport {
    status: BatchStatus,
    project_id: String,
    run_id: String,
    destination: String,
    file_count: usize,
    failed_count: usize,
    total_bytes: u64,
    files: Vec<FileReport>,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars", inline)]
enum BatchStatus {
    Downloaded,
    Partial,
    Failed,
}

#[derive(Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct FileReport {
    player_number: u64,
    composite_run_id: String,
    file_ref: String,
    artifact_path: String,
    local_path: String,
    #[serde(flatten)]
    outcome: FileOutcome,
}

#[derive(Serialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars", inline)]
enum FileOutcome {
    Downloaded { size_bytes: u64 },
    Failed { error: String },
}

impl DownloadRunFilesTool {
    pub(super) fn new(config: Config, allowed_root: PathBuf) -> Self {
        Self {
            config,
            allowed_root,
            upstream: None,
        }
    }

    pub(super) fn with_upstream(mut self, upstream: Peer<RoleClient>) -> Self {
        self.upstream = Some(upstream);
        self
    }

    async fn download(&self, input: DownloadRunFilesInput) -> Result<CallToolResult> {
        let input = input::resolve(input, &self.config, self.upstream.as_ref()).await?;
        let config = Config::with_credential(
            self.config.credential.clone(),
            &self.config.api_url,
            Some(input.project_id.clone()),
        )?;
        let client = Client::new(config)?;
        let selectors = input::explicit_selectors(input.files.as_deref())?;
        let manifest = client
            .artifact_manifest(&input.run_id, selectors.as_deref())
            .await?;
        let destination = self.prepare_destination(&input.destination_dir).await?;
        let downloads = self
            .plan_downloads(&destination, manifest, input.files.as_deref())
            .await?;
        // Ordered buffering keeps manifest order without indexing and sorting results.
        let client = &client;
        let files = stream::iter(downloads)
            .map(|download| async move {
                let outcome = match download_one(client, &download).await {
                    Ok(()) => FileOutcome::Downloaded {
                        size_bytes: download.artifact.size_bytes,
                    },
                    Err(error) => FileOutcome::Failed {
                        error: format!("{error:#}"),
                    },
                };
                FileReport {
                    player_number: download.artifact.player_number,
                    composite_run_id: download.artifact.composite_run_id,
                    file_ref: format!("artifact:{}", download.artifact.filename),
                    artifact_path: download.artifact.filename,
                    local_path: download.destination.to_string_lossy().into_owned(),
                    outcome,
                }
            })
            .buffered(4)
            .collect::<Vec<_>>()
            .await;
        let (mut file_count, mut total_bytes) = (0, 0);
        for file in &files {
            if let FileOutcome::Downloaded { size_bytes } = file.outcome {
                file_count += 1;
                total_bytes += size_bytes;
            }
        }
        let failed_count = files.len() - file_count;
        let report = DownloadReport {
            status: match (file_count, failed_count) {
                (_, 0) => BatchStatus::Downloaded,
                (0, _) => BatchStatus::Failed,
                _ => BatchStatus::Partial,
            },
            project_id: input.project_id,
            run_id: input.run_id,
            destination: destination.to_string_lossy().into_owned(),
            file_count,
            failed_count,
            total_bytes,
            files,
        };
        let mut result = CallToolResult::success(vec![ContentBlock::text(format!(
            "Downloaded {file_count} run artifact(s) ({total_bytes} bytes); {failed_count} failed. Existing paths are preserved. Retry failed files from the beginning using files."
        ))]);
        result.is_error = Some(failed_count != 0);
        result.structured_content = Some(serde_json::to_value(report)?);
        Ok(result)
    }

    async fn prepare_destination(&self, requested: &str) -> Result<PathBuf> {
        prepare_directory(&self.allowed_root, Path::new(requested)).await
    }

    async fn plan_downloads(
        &self,
        root: &Path,
        manifest: ArtifactManifest,
        selectors: Option<&[RunFileSelector]>,
    ) -> Result<Vec<PlannedDownload>> {
        let multiplayer = manifest.players.len() > 1;
        let players: HashMap<_, _> = manifest
            .players
            .into_iter()
            .map(|player| (player.id, player.player_number))
            .collect();
        let selected = input::select_files(selectors, manifest.files, multiplayer)?;
        let mut destinations = HashSet::new();
        let mut downloads = Vec::new();
        for (artifact, output) in selected {
            anyhow::ensure!(
                players.get(&artifact.composite_run_id) == Some(&artifact.player_number),
                "artifact manifest references an unknown or inconsistent player"
            );
            let parent =
                prepare_directory(root, output.parent().context("missing output directory")?)
                    .await?;
            let destination = parent.join(output.file_name().context("missing output filename")?);
            anyhow::ensure!(
                destinations.insert(destination.clone()),
                "multiple artifacts resolve to '{}'",
                destination.display()
            );
            downloads.push(PlannedDownload {
                artifact,
                destination,
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
        let input =
            serde_json::from_value(serde_json::Value::Object(arguments.unwrap_or_default()))
                .map_err(|error| {
                    ErrorData::invalid_params(format!("invalid {NAME} arguments: {error}"), None)
                })?;
        Ok(match self.download(input).await {
            Ok(result) => result,
            Err(error) => CallToolResult::error(vec![ContentBlock::text(format!(
                "Run file download failed: {error:#}"
            ))]),
        }
        .into())
    }
}

fn validate_artifact_path(filename: &str) -> Result<()> {
    anyhow::ensure!(
        !filename.chars().any(|c| c.is_control() || c == '\\')
            && filename
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."),
        "unsafe artifact path: {filename:?}"
    );
    Ok(())
}

fn safe_artifact_path(filename: &str) -> Result<PathBuf> {
    validate_artifact_path(filename)?;
    anyhow::ensure!(
        !filename
            .chars()
            .any(|c| matches!(c, ':' | '<' | '>' | '"' | '|' | '?' | '*'))
            && filename.split('/').all(|part| {
                let device = part
                    .split('.')
                    .next()
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                !part.ends_with(['.', ' '])
                    && !matches!(device.as_str(), "con" | "prn" | "aux" | "nul")
                    && !(device.len() == 4
                        && (device.starts_with("com") || device.starts_with("lpt"))
                        && matches!(device.as_bytes()[3], b'1'..=b'9'))
            }),
        "unsafe or non-portable artifact path: {filename:?}"
    );
    Ok(PathBuf::from(filename))
}

async fn download_one(client: &Client, download: &PlannedDownload) -> Result<()> {
    match fs::symlink_metadata(&download.destination) {
        Ok(_) => anyhow::bail!(
            "refusing to overwrite existing path '{}'",
            download.destination.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let parent = download
        .destination
        .parent()
        .context("missing output directory")?;
    // This file has no replaceable pathname on Unix; Windows locks its name
    // until the handle closes. Finalization also reads only this handle.
    let mut temporary = tempfile::tempfile_in(parent)?;
    client
        .download_signed_artifact(&download.artifact, &mut temporary)
        .await?;
    save_download(&mut temporary, &download.destination)
        .await
        .context("failed to finalize the downloaded artifact")?;
    Ok(())
}

struct IncompleteDownload<'a> {
    destination: &'a Path,
    file: Option<fs::File>,
}

#[cfg(unix)]
impl IncompleteDownload<'_> {
    fn owns_path(&self) -> std::io::Result<bool> {
        use std::os::unix::fs::MetadataExt as _;

        let file = self
            .file
            .as_ref()
            .ok_or_else(|| std::io::Error::other("download output is closed"))?;
        let opened = file.metadata()?;
        let current = fs::symlink_metadata(self.destination)?;
        Ok(current.is_file() && opened.dev() == current.dev() && opened.ino() == current.ino())
    }
}

impl Drop for IncompleteDownload<'_> {
    fn drop(&mut self) {
        #[cfg(unix)]
        let remove_output = self.owns_path().unwrap_or(false);
        // Windows denies deletion/renaming while this output handle is open.
        #[cfg(not(unix))]
        let remove_output = true;
        if let Some(file) = self.file.take() {
            // Windows must close the handle before the incomplete file is removed.
            drop(file);
            if remove_output {
                let _ = fs::remove_file(self.destination);
            }
        }
    }
}

async fn save_download(verified: &mut fs::File, destination: &Path) -> std::io::Result<()> {
    use std::io::{Read as _, Seek as _, Write as _};

    verified.rewind()?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut pending = IncompleteDownload {
        destination,
        file: Some(options.open(destination)?),
    };
    let output = pending
        .file
        .as_mut()
        .ok_or_else(|| std::io::Error::other("download output is closed"))?;
    // Never reopen or rename a temporary pathname: only the verified handle
    // supplies bytes. create_new refuses existing files and symlinks.
    let mut buffer = vec![0_u8; 128 * 1024];
    loop {
        let count = verified.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        output.write_all(&buffer[..count])?;
        // Yield between bounded writes so large saves remain cancellable and
        // other downloads can continue. Cancellation drops the cleanup guard.
        tokio::task::yield_now().await;
    }
    output.flush()?;
    #[cfg(unix)]
    if !pending.owns_path()? {
        return Err(std::io::Error::other(
            "download output path changed during saving",
        ));
    }
    drop(pending.file.take());
    Ok(())
}

async fn prepare_directory(root: &Path, requested: &Path) -> Result<PathBuf> {
    anyhow::ensure!(
        !requested
            .components()
            .any(|part| part == Component::ParentDir),
        "download directory cannot contain parent traversal"
    );
    let candidate = root.join(requested);
    let mut ancestor = candidate.clone();
    let mut directory = loop {
        match tokio::fs::canonicalize(&ancestor).await {
            Ok(path) => break path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                anyhow::ensure!(ancestor.pop(), "destination has no existing ancestor");
            }
            Err(error) => return Err(error.into()),
        }
    };
    anyhow::ensure!(
        directory.starts_with(root),
        "download directory is outside the allowed directory"
    );
    anyhow::ensure!(
        tokio::fs::metadata(&directory).await?.is_dir(),
        "download path is not a directory"
    );
    for component in candidate.strip_prefix(ancestor)?.components() {
        match component {
            Component::CurDir => continue,
            Component::Normal(name) => directory.push(name),
            _ => anyhow::bail!("invalid download directory"),
        }
        match tokio::fs::create_dir(&directory).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        directory = tokio::fs::canonicalize(&directory).await?;
        anyhow::ensure!(
            directory.starts_with(root),
            "download directory is outside the allowed directory"
        );
        anyhow::ensure!(
            tokio::fs::metadata(&directory).await?.is_dir(),
            "download path is not a directory"
        );
    }
    Ok(directory)
}

fn definition() -> Tool {
    let mut tool = Tool::new(NAME,
        "Download a Nunu run's files into the workspace. A run link or run ID is enough. Multiple players get separate player-<number> folders. Existing paths return per-file errors and are preserved; use files to retry failed downloads from the beginning.",
        JsonObject::new())
        .with_input_schema::<DownloadRunFilesInput>()
        .with_output_schema::<DownloadReport>()
        .with_title("Download Nunu run files")
        .with_annotations(ToolAnnotations::new().read_only(false).destructive(false).idempotent(true).open_world(true));
    // Serde accepts the legacy alias; expose it in the advertised schema too.
    if let Some(properties) = Arc::make_mut(&mut tool.input_schema)
        .get_mut("properties")
        .and_then(serde_json::Value::as_object_mut)
    {
        properties.insert("path".to_string(), properties["destination_dir"].clone());
    }
    tool
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::client::ArtifactPlayer;

    fn artifact(
        run_id: &str,
        player_number: u64,
        filename: &str,
        size_bytes: u64,
    ) -> ArtifactDownload {
        ArtifactDownload {
            composite_run_id: run_id.to_string(),
            player_number,
            filename: filename.to_string(),
            size_bytes,
            etag: "\"v1\"".to_string(),
            download_url: "https://example.invalid/artifact".to_string(),
        }
    }

    fn workspace_tool(root: &Path) -> DownloadRunFilesTool {
        DownloadRunFilesTool::new(
            Config::new("secret".to_string(), "http://localhost:3000/api").expect("config"),
            root.canonicalize().expect("workspace"),
        )
    }

    fn manifest(files: Vec<ArtifactDownload>) -> ArtifactManifest {
        let mut players: Vec<ArtifactPlayer> = Vec::new();
        for file in &files {
            if !players
                .iter()
                .any(|player| player.id == file.composite_run_id)
            {
                players.push(ArtifactPlayer {
                    id: file.composite_run_id.clone(),
                    player_number: file.player_number,
                });
            }
        }
        ArtifactManifest { players, files }
    }

    async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
        use tokio::io::AsyncReadExt as _;
        let mut bytes = Vec::new();
        loop {
            let mut chunk = [0; 8192];
            let count = socket.read(&mut chunk).await.expect("request");
            assert!(count > 0, "incomplete request");
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(end) = bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                let length = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .map_or(0, |value| {
                        value.trim().parse::<usize>().expect("content length")
                    });
                if bytes.len() >= end + 4 + length {
                    break;
                }
            }
        }
        String::from_utf8(bytes).expect("request text")
    }

    #[test]
    fn schema_has_only_the_required_public_inputs() {
        let tool = definition();
        let properties = tool.input_schema["properties"]
            .as_object()
            .expect("input properties");
        let mut names: Vec<_> = properties.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            ["destination_dir", "files", "path", "project_id", "run_id"]
        );
        assert_eq!(tool.input_schema["required"], serde_json::json!(["run_id"]));
        assert!(tool.input_schema.get("oneOf").is_none());
        assert_eq!(
            tool.annotations.as_ref().and_then(|a| a.read_only_hint),
            Some(false)
        );
        assert_eq!(tool.input_schema["additionalProperties"], false);
        for invalid in [
            serde_json::json!({"run_id":"run-1", "download_url":"https://foreign.test"}),
            serde_json::json!({"files":[{"file_ref":"artifact:file", "download_url":"https://foreign.test"}]}),
        ] {
            assert!(serde_json::from_value::<DownloadRunFilesInput>(invalid).is_err());
        }
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
        for path in [
            "a/../secret",
            "a\\..\\secret",
            "C:/secret",
            "file:stream",
            ".. /secret",
            "a./secret",
            "NUL.txt",
            "COM1/log",
            "a//b",
            "a/./b",
            "a\0b",
        ] {
            assert!(safe_artifact_path(path).is_err(), "unsafe path: {path:?}");
        }
    }

    #[test]
    fn selected_files_use_get_run_file_identifiers() {
        let input = DownloadRunFilesInput {
            project_id: "project_123".to_string(),
            run_id: "multiplayer_123".to_string(),
            destination_dir: "downloads".to_string(),
            files: Some(vec![RunFileSelector::Selector {
                run_id: "composite_123-1".to_string(),
                file_ref: "artifact:logs/output.txt".to_string(),
                relative_path: None,
            }]),
        };

        let selected = input::explicit_selectors(input.files.as_deref())
            .expect("parse selectors")
            .expect("scoped selectors");
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].run_id, "composite_123-1");
        assert_eq!(selected[0].file_ref, "artifact:logs/output.txt");
    }

    #[test]
    fn selected_files_reject_non_artifact_refs() {
        let input = DownloadRunFilesInput {
            project_id: "project_123".to_string(),
            run_id: "multiplayer_123".to_string(),
            destination_dir: "downloads".to_string(),
            files: Some(vec![RunFileSelector::Selector {
                run_id: "composite_123-1".to_string(),
                file_ref: "event_123#img0".to_string(),
                relative_path: None,
            }]),
        };

        assert!(input::explicit_selectors(input.files.as_deref()).is_err());
    }

    #[tokio::test]
    async fn plans_only_files_matching_stable_run_and_file_refs() {
        let root = tempfile::tempdir().expect("create destination root");
        let tool = workspace_tool(root.path());
        let run = manifest(vec![
            artifact("composite_123-1", 1, "logs/output.txt", 1),
            artifact("composite_123-2", 2, "logs/output.txt", 2),
        ]);
        let selected = [RunFileSelector::Selector {
            run_id: "composite_123-2".to_string(),
            file_ref: "artifact:logs/output.txt".to_string(),
            relative_path: None,
        }];

        let downloads = tool
            .plan_downloads(root.path(), run, Some(&selected))
            .await
            .expect("plan selected file");

        assert_eq!(downloads.len(), 1);
        assert_eq!(downloads[0].artifact.composite_run_id, "composite_123-2");
        assert_eq!(downloads[0].artifact.filename, "logs/output.txt");
        assert!(
            downloads[0]
                .destination
                .ends_with("player-2/logs/output.txt")
        );
    }

    #[tokio::test]
    async fn flattens_single_player_artifacts_by_default() {
        let root = tempfile::tempdir().expect("create destination root");
        let tool = workspace_tool(root.path());
        let run = manifest(vec![artifact(
            "composite_123-1",
            1,
            "custom/attempts.json",
            1,
        )]);

        let downloads = tool
            .plan_downloads(root.path(), run, None)
            .await
            .expect("plan single-player artifacts");

        assert_eq!(downloads.len(), 1);
        assert!(downloads[0].destination.ends_with("custom/attempts.json"));
    }

    #[tokio::test]
    async fn uses_a_selected_file_relative_path_override_exactly() {
        let root = tempfile::tempdir().expect("create destination root");
        let tool = workspace_tool(root.path());
        let run = manifest(vec![artifact(
            "composite_123-1",
            1,
            "custom/attempts.json",
            1,
        )]);
        let selected = [RunFileSelector::Selector {
            run_id: "composite_123-1".to_string(),
            file_ref: "artifact:custom/attempts.json".to_string(),
            relative_path: Some("attempts.json".to_string()),
        }];

        let downloads = tool
            .plan_downloads(root.path(), run, Some(&selected))
            .await
            .expect("plan selected artifact with override");

        assert_eq!(downloads.len(), 1);
        assert!(downloads[0].destination.ends_with("attempts.json"));
        assert!(!downloads[0].destination.ends_with("custom/attempts.json"));
    }

    #[tokio::test]
    async fn non_portable_storage_names_can_use_a_safe_output_alias() {
        let root = tempfile::tempdir().expect("workspace");
        let tool = workspace_tool(root.path());
        let selectors = [RunFileSelector::Selector {
            run_id: String::new(),
            file_ref: "artifact:logs/output:stderr.txt".to_string(),
            relative_path: Some("stderr.txt".to_string()),
        }];
        input::explicit_selectors(Some(&selectors)).expect("valid storage selector");
        let downloads = tool
            .plan_downloads(
                root.path(),
                manifest(vec![artifact("player-1", 1, "logs/output:stderr.txt", 7)]),
                Some(&selectors),
            )
            .await
            .expect("safe alias");
        assert_eq!(downloads[0].destination, root.path().join("stderr.txt"));
        assert!(
            tool.plan_downloads(
                root.path(),
                manifest(vec![artifact("player-1", 1, "logs/output:stderr.txt", 7)]),
                None,
            )
            .await
            .is_err()
        );
        assert!(validate_artifact_path("../secret").is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn finalization_uses_verified_bytes_when_the_temporary_name_is_replaced() {
        use std::{io::Write as _, os::unix::fs::symlink};

        let root = tempfile::tempdir().expect("workspace");
        let outside = tempfile::NamedTempFile::new().expect("outside file");
        fs::write(outside.path(), b"preserve me").expect("outside contents");
        let mut temporary = tempfile::NamedTempFile::new_in(root.path()).expect("staging file");
        temporary
            .write_all(b"verified bytes")
            .expect("verified contents");
        fs::remove_file(temporary.path()).expect("replace staging name");
        symlink(outside.path(), temporary.path()).expect("replacement symlink");
        let destination = root.path().join("output.txt");

        save_download(temporary.as_file_mut(), &destination)
            .await
            .expect("save verified handle");

        assert!(
            !fs::symlink_metadata(&destination)
                .expect("output metadata")
                .is_symlink()
        );
        assert_eq!(
            fs::read(destination).expect("output contents"),
            b"verified bytes"
        );
        assert_eq!(
            fs::read(outside.path()).expect("outside contents"),
            b"preserve me"
        );
    }

    #[tokio::test]
    async fn failed_finalization_removes_its_incomplete_output() {
        let root = tempfile::tempdir().expect("workspace");
        let source = root.path().join("source.txt");
        fs::write(&source, b"content").expect("source contents");
        let mut write_only = fs::OpenOptions::new()
            .write(true)
            .open(source)
            .expect("write-only handle");
        let destination = root.path().join("output.txt");
        assert!(save_download(&mut write_only, &destination).await.is_err());
        assert!(!destination.exists());
    }

    #[tokio::test]
    async fn cancellation_during_finalization_removes_its_incomplete_output() {
        use futures::FutureExt as _;
        use std::io::Write as _;

        let root = tempfile::tempdir().expect("workspace");
        let mut verified = tempfile::tempfile_in(root.path()).expect("staging file");
        verified
            .write_all(b"verified bytes")
            .expect("verified contents");
        let destination = root.path().join("output.txt");
        let mut save = Box::pin(save_download(&mut verified, &destination));
        assert!(save.as_mut().now_or_never().is_none());
        assert!(destination.exists());
        drop(save);
        assert!(!destination.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn finalization_detects_replacement_of_the_output_path() {
        use futures::FutureExt as _;
        use std::{io::Write as _, os::unix::fs::symlink};

        let root = tempfile::tempdir().expect("workspace");
        let outside = tempfile::NamedTempFile::new().expect("outside file");
        fs::write(outside.path(), b"preserve me").expect("outside contents");
        let mut verified = tempfile::tempfile_in(root.path()).expect("staging file");
        verified
            .write_all(b"verified bytes")
            .expect("verified contents");
        let destination = root.path().join("output.txt");
        let mut save = Box::pin(save_download(&mut verified, &destination));
        assert!(save.as_mut().now_or_never().is_none());
        fs::remove_file(&destination).expect("replace output name");
        symlink(outside.path(), &destination).expect("replacement symlink");
        save.await
            .expect_err("replaced output must not be reported as downloaded");
        assert!(
            fs::symlink_metadata(&destination)
                .expect("replacement preserved")
                .is_symlink()
        );
        assert_eq!(
            fs::read(outside.path()).expect("outside contents"),
            b"preserve me"
        );
    }

    #[tokio::test]
    async fn planning_rejects_invalid_manifest_entries_and_output_collisions() {
        let root = tempfile::tempdir().expect("workspace");
        let tool = workspace_tool(root.path());
        let mut unknown_player = manifest(vec![artifact("player-1", 1, "file.txt", 1)]);
        unknown_player.players.clear();
        let mut inconsistent_player = manifest(vec![artifact("player-1", 1, "file.txt", 1)]);
        inconsistent_player.players[0].player_number = 2;
        for manifest in [
            unknown_player,
            inconsistent_player,
            manifest(vec![artifact("player-1", 1, "../escape", 1)]),
            manifest(vec![
                artifact("player-1", 1, "file.txt", 1),
                artifact("player-1", 1, "file.txt", 1),
            ]),
        ] {
            assert!(
                tool.plan_downloads(root.path(), manifest, None)
                    .await
                    .is_err()
            );
        }
        let files = manifest(vec![
            artifact("player-1", 1, "a.txt", 1),
            artifact("player-1", 1, "b.txt", 1),
        ]);
        let selectors: Vec<RunFileSelector> = serde_json::from_value(serde_json::json!([
            {"file_ref":"artifact:a.txt", "relative_path":"same.txt"},
            {"file_ref":"artifact:b.txt", "relative_path":"same.txt"},
        ]))
        .expect("selectors");
        assert!(
            tool.plan_downloads(root.path(), files, Some(&selectors))
                .await
                .is_err()
        );
        assert_eq!(fs::read_dir(root.path()).expect("workspace").count(), 0);
    }

    #[test]
    fn selected_file_relative_paths_must_be_safe() {
        let input = DownloadRunFilesInput {
            project_id: "project_123".to_string(),
            run_id: "multiplayer_123".to_string(),
            destination_dir: "downloads".to_string(),
            files: Some(vec![RunFileSelector::Selector {
                run_id: "composite_123-1".to_string(),
                file_ref: "artifact:logs/output.txt".to_string(),
                relative_path: Some("../attempts.json".to_string()),
            }]),
        };

        assert!(input::explicit_selectors(input.files.as_deref()).is_err());
    }

    #[tokio::test]
    async fn destination_is_created_inside_allowed_root() {
        let root = tempfile::tempdir().expect("create allowed root");
        let tool = workspace_tool(root.path());

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
        let tool = workspace_tool(root.path());
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
        let tool = workspace_tool(root.path());

        assert!(tool.prepare_destination("escape/downloads").await.is_err());
        assert!(!outside.path().join("downloads").exists());
    }

    #[tokio::test]
    async fn a_run_link_alone_downloads_to_the_default_folder_without_leaking_credentials() {
        use tokio::io::AsyncWriteExt as _;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let artifact_url = format!("http://{address}/runs/composite_1/artifacts/logs/output.txt");
        let run_body = serde_json::json!({
            "players": [{
                "id": "composite_1",
                "player_number": 1,
            }],
            "files": [{ "composite_run_id": "composite_1", "player_number": 1,
                "filename": "logs/output.txt", "download_url": artifact_url, "size_bytes": 7,
                "etag": "\"v1\"", "expires_at_unix": 4_000_000_000_u64 }]
        })
        .to_string();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.expect("accept request");
                let request = read_request(&mut socket).await;
                let (status, content_type, body) = if request.starts_with(
                    "POST /api/v1/project/project_123/runs/run_123/artifact-downloads ",
                ) {
                    assert!(request.to_ascii_lowercase().contains("x-api-key: secret"));
                    ("200 OK", "application/json", run_body.as_bytes())
                } else if request.starts_with("GET /runs/composite_1/artifacts/logs/output.txt ") {
                    assert!(!request.to_ascii_lowercase().contains("x-api-key"));
                    assert!(!request.to_ascii_lowercase().contains("authorization:"));
                    assert!(request.to_ascii_lowercase().contains("if-match: \"v1\""));
                    ("200 OK", "application/octet-stream", b"content".as_slice())
                } else {
                    ("404 Not Found", "text/plain", b"not found".as_slice())
                };
                let headers = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nETag: \"v1\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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
            .download(
                serde_json::from_value(serde_json::json!({
                    "run_id": format!("http://{address}/nexus/project_123/runs/c/run_123/details")
                }))
                .expect("link-only input"),
            )
            .await
            .expect("download run files");

        server.await.expect("test server completes");
        assert_eq!(
            std::fs::read(
                root.path()
                    .join("downloads/project_123/run_123/logs/output.txt")
            )
            .expect("read downloaded artifact"),
            b"content"
        );
        assert_eq!(
            result.structured_content.expect("structured")["file_count"],
            1
        );
    }

    #[tokio::test]
    async fn plain_filenames_download_only_matching_files_into_separate_player_folders() {
        use tokio::io::AsyncWriteExt as _;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let manifest = serde_json::json!({
            "players": [{"id": "player-1", "player_number": 1}, {"id": "player-2", "player_number": 2}],
            "files": (["player-1", "player-2"].into_iter().enumerate().flat_map(|(i, id)| {
                ["logs/output.txt", "other.txt"].map(|name| serde_json::json!({
                    "composite_run_id": id, "player_number": i + 1, "filename": name,
                    "size_bytes": 1, "etag": "\"v1\"", "expires_at_unix": 4_000_000_000_u64,
                    "download_url": format!("http://{address}/{id}/{name}")
                }))
            }).collect::<Vec<_>>())
        }).to_string();
        let server = tokio::spawn(async move {
            for _ in 0..3 {
                let (mut socket, _) = listener.accept().await.expect("request");
                let request = read_request(&mut socket).await;
                let body = if request
                    .starts_with("POST /api/v1/project/project-1/runs/run-1/artifact-downloads ")
                {
                    assert!(request.contains("artifact:logs/output.txt"));
                    assert!(
                        !request.contains("\"run_id\""),
                        "filename selectors omit player IDs"
                    );
                    manifest.as_str()
                } else {
                    assert!(
                        request.starts_with("GET /player-1/logs/output.txt ")
                            || request.starts_with("GET /player-2/logs/output.txt ")
                    );
                    "x"
                };
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nETag: \"v1\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.expect("response");
            }
        });
        let root = tempfile::tempdir().expect("root");
        let mut config =
            Config::new("secret".to_string(), format!("http://{address}/api")).expect("config");
        config.project_id = Some("project-1".to_string());
        let tool =
            DownloadRunFilesTool::new(config, root.path().canonicalize().expect("canonical root"));
        let result = tool
            .download(
                serde_json::from_value(serde_json::json!({
                    "run_id": "run-1", "files": ["logs/output.txt"]
                }))
                .expect("simple input"),
            )
            .await
            .expect("download");
        assert_eq!(
            result.structured_content.expect("structured")["file_count"],
            2
        );
        for player in [1, 2] {
            let folder = root
                .path()
                .join(format!("downloads/project-1/run-1/player-{player}"));
            assert_eq!(
                std::fs::read(folder.join("logs/output.txt")).expect("file"),
                b"x"
            );
            assert!(!folder.join("other.txt").exists());
        }
        server.await.expect("server");
    }

    #[tokio::test]
    async fn failed_files_restart_from_scratch_without_redownloading_successful_files() {
        use tokio::io::AsyncWriteExt as _;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let manifest = serde_json::json!({
            "players": [{"id": "player-1", "player_number": 1}],
            "files": (["good.txt", "missing.txt"].map(|filename| serde_json::json!({
                "composite_run_id": "player-1", "player_number": 1, "filename": filename,
                "size_bytes": 7, "etag": "\"v1\"", "expires_at_unix": 4_000_000_000_u64,
                "download_url": format!("http://{address}/{filename}")
            })))
        })
        .to_string();
        let server = tokio::spawn(async move {
            let mut successful_gets = 0;
            let mut interrupted = false;
            for _ in 0..5 {
                let (mut socket, _) = listener.accept().await.expect("request");
                let request = read_request(&mut socket).await;
                let (body, length) = if request.starts_with("POST ") {
                    (manifest.as_str(), manifest.len())
                } else if request.starts_with("GET /good.txt ") {
                    successful_gets += 1;
                    ("content", 7)
                } else {
                    assert!(!request.to_ascii_lowercase().contains("range:"));
                    if interrupted {
                        ("content", 7)
                    } else {
                        interrupted = true;
                        ("con", 7)
                    }
                };
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nETag: \"v1\"\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n{body}").as_bytes()).await.expect("response");
            }
            assert_eq!(
                successful_gets, 1,
                "retrying the failed file must preserve the successful file"
            );
        });
        let root = tempfile::tempdir().expect("tempdir");
        let tool = DownloadRunFilesTool::new(
            Config::new("secret".to_string(), format!("http://{address}/api")).expect("config"),
            root.path().canonicalize().expect("root"),
        );
        for attempt in 0..2 {
            let result = tool
                .download(DownloadRunFilesInput {
                    project_id: "project-1".to_string(),
                    run_id: "run-1".to_string(),
                    destination_dir: "downloads".to_string(),
                    files: (attempt == 1).then(|| {
                        vec![RunFileSelector::Selector {
                            run_id: String::new(),
                            file_ref: "artifact:missing.txt".to_string(),
                            relative_path: None,
                        }]
                    }),
                })
                .await
                .expect("partial response");
            let content = result.structured_content.expect("structured content");
            assert_eq!(
                content["status"],
                if attempt == 0 {
                    "partial"
                } else {
                    "downloaded"
                }
            );
            assert_eq!(content["failed_count"], usize::from(attempt == 0));
            assert_eq!(content["file_count"], 1);
            assert_eq!(content["files"][0]["status"], "downloaded");
            assert_eq!(
                std::fs::read(root.path().join("downloads/good.txt")).expect("completed output"),
                b"content"
            );
            let destination = root.path().join("downloads");
            assert_eq!(
                fs::read_dir(&destination)
                    .expect("output directory")
                    .count(),
                attempt + 1
            );
            if attempt == 1 {
                assert_eq!(
                    fs::read(destination.join("missing.txt")).expect("retried file"),
                    b"content"
                );
            }
        }
        server.await.expect("server");
    }

    #[tokio::test]
    async fn signed_downloads_do_not_follow_redirects() {
        use tokio::io::AsyncWriteExt as _;

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
            let request = read_request(&mut socket).await;
            assert!(!request.to_ascii_lowercase().contains("x-api-key"));
            assert!(!request.to_ascii_lowercase().contains("authorization:"));
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
        let mut temporary = tempfile::tempfile().expect("temporary file");
        let mut artifact = artifact("player-1", 1, "artifact", 1);
        artifact.download_url = format!("http://{artifact_address}/artifact");
        let result = client
            .download_signed_artifact(&artifact, &mut temporary)
            .await;

        source.await.expect("artifact server completes");
        assert!(result.is_err(), "redirect response must not be followed");
        assert_eq!(temporary.metadata().expect("metadata").len(), 0);
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
    async fn existing_output_is_rejected_without_creating_temporary_files() {
        let root = tempfile::tempdir().expect("workspace");
        let destination = root.path().join("output.txt");
        fs::write(&destination, b"existing").expect("existing output");
        let client =
            Client::new(Config::new("secret".to_string(), "http://localhost/api").expect("config"))
                .expect("client");
        let download = PlannedDownload {
            artifact: artifact("player-1", 1, "output.txt", 7),
            destination: destination.clone(),
        };
        let error = download_one(&client, &download)
            .await
            .expect_err("existing output");
        assert!(error.to_string().contains("refusing to overwrite"));
        assert_eq!(
            fs::read(destination).expect("preserved output"),
            b"existing"
        );
        assert_eq!(fs::read_dir(root.path()).expect("workspace").count(), 1);
    }

    #[tokio::test]
    async fn output_created_during_the_request_is_preserved() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let root = tempfile::tempdir().expect("workspace");
        let destination = root.path().join("output.txt");
        let other_writer = destination.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request");
            let mut bytes = [0; 4096];
            assert!(socket.read(&mut bytes).await.expect("read") > 0);
            fs::write(other_writer, b"existing").expect("concurrent output");
            socket.write_all(b"HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nContent-Length: 7\r\nConnection: close\r\n\r\ncontent").await.expect("response");
        });
        let client = Client::new(
            Config::new("secret".to_string(), format!("http://{address}/api")).expect("config"),
        )
        .expect("client");
        let mut artifact = artifact("player-1", 1, "output.txt", 7);
        artifact.download_url = format!("http://{address}/file");
        let download = PlannedDownload {
            artifact,
            destination: destination.clone(),
        };
        download_one(&client, &download)
            .await
            .expect_err("concurrent output must be preserved");
        server.await.expect("server");
        assert_eq!(
            fs::read(destination).expect("preserved output"),
            b"existing"
        );
        assert_eq!(fs::read_dir(root.path()).expect("workspace").count(), 1);
    }

    #[tokio::test]
    async fn cancellation_removes_the_partial_file() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let root = tempfile::tempdir().expect("workspace");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let (started, response_started) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request");
            let mut bytes = [0; 4096];
            assert!(socket.read(&mut bytes).await.expect("read") > 0);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nContent-Length: 7\r\n\r\ncon")
                .await
                .expect("partial response");
            started.send(()).expect("notify partial response");
            std::future::pending::<()>().await;
        });
        let client = Client::new(
            Config::new("secret".to_string(), format!("http://{address}/api")).expect("config"),
        )
        .expect("client");
        let mut artifact = artifact("player-1", 1, "output.txt", 7);
        artifact.download_url = format!("http://{address}/file");
        let download = PlannedDownload {
            artifact,
            destination: root.path().join("output.txt"),
        };
        let task = tokio::spawn(async move { download_one(&client, &download).await });
        tokio::time::timeout(std::time::Duration::from_secs(2), response_started)
            .await
            .expect("partial response timeout")
            .expect("partial response started");
        assert!(!task.is_finished());
        task.abort();
        let _ = task.await;
        server.abort();
        assert_eq!(fs::read_dir(root.path()).expect("workspace").count(), 0);
    }
}
