use super::{DownloadRunFilesInput, RunFileSelector, safe_artifact_path, validate_artifact_path};
use crate::{
    api::client::{ArtifactDownload, ArtifactSelector},
    config::Config,
};
use anyhow::{Context as _, Result};
use rmcp::{
    Peer, RoleClient,
    model::{CallToolRequestParams, CallToolResponse, CallToolResult},
};
use std::{collections::HashMap, path::PathBuf};
use url::Url;

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

async fn resolve_project(upstream: &Peer<RoleClient>, run_id: &str) -> Result<String> {
    let request =
        CallToolRequestParams::new("inspect_run").with_arguments(serde_json::Map::from_iter([
            ("run_id".to_string(), run_id.into()),
            ("view".to_string(), "outline".into()),
        ]));
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        upstream.call_tool_once(request),
    )
    .await
    .context("project lookup timed out; try a full Nunu run link")?
    .context("project lookup failed; try a full Nunu run link")?;
    let CallToolResponse::Complete(result) = response else {
        anyhow::bail!("project lookup did not complete; try a full Nunu run link");
    };
    project_from_inspection(&result)
}

fn project_from_inspection(result: &CallToolResult) -> Result<String> {
    anyhow::ensure!(
        result.is_error != Some(true),
        "run inspection failed; check access or provide a full Nunu run link"
    );
    let project = result
        .structured_content
        .as_ref()
        .and_then(|value| value["project_id"].as_str())
        .map(str::to_string)
        .or_else(|| {
            result
                .content
                .iter()
                .filter_map(|content| content.as_text())
                .find_map(|text| {
                    let value: serde_json::Value = serde_json::from_str(&text.text).ok()?;
                    value["project_id"].as_str().map(str::to_string)
                })
        });
    project.filter(|id| valid_id(id)).context(
        "project lookup returned no valid project; provide a full Nunu run link or project_id",
    )
}

pub(super) async fn resolve(
    mut input: DownloadRunFilesInput,
    config: &Config,
    upstream: Option<&Peer<RoleClient>>,
) -> Result<DownloadRunFilesInput> {
    input.run_id = input.run_id.trim().to_string();
    input.project_id = input.project_id.trim().to_string();
    anyhow::ensure!(
        !input.run_id.is_empty(),
        "Which run should be downloaded? Provide a Nunu run link or an ID from list_runs or inspect_run."
    );
    if input.run_id.contains("://") || input.run_id.starts_with('/') {
        let api = Url::parse(&config.api_url)?;
        let link = api.join(&input.run_id).context("invalid Nunu run link")?;
        anyhow::ensure!(
            link.origin() == api.origin()
                && link.username().is_empty()
                && link.password().is_none(),
            "the run link must belong to the configured Nunu deployment"
        );
        let segments: Vec<_> = link.path_segments().context("invalid run link")?.collect();
        let (project, run) = match segments.as_slice() {
            ["nexus", project, "runs", "c" | "m", run, ..]
            | ["api", "v1", "project", project, "runs", run, ..] => (*project, *run),
            _ => {
                anyhow::bail!("Use /nexus/<project>/runs/c/<run>, or supply run_id and project_id")
            }
        };
        anyhow::ensure!(
            input.project_id.is_empty() || input.project_id == project,
            "project_id does not match the run link"
        );
        input.project_id = project.to_string();
        input.run_id = run.to_string();
    }
    anyhow::ensure!(
        valid_id(&input.run_id),
        "run_id must contain only letters, numbers, hyphens, or underscores"
    );
    if input.project_id.is_empty() {
        input.project_id = config
            .project_id
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_string();
        if input.project_id.is_empty()
            && let Some(upstream) = upstream
        {
            input.project_id = resolve_project(upstream, &input.run_id).await?;
        }
    }
    anyhow::ensure!(
        valid_id(&input.project_id),
        "Provide a full Nunu run link, a valid project_id, or configure NUNU_PROJECT_ID"
    );
    if input.destination_dir.trim().is_empty() {
        input.destination_dir = format!("downloads/{}/{}", input.project_id, input.run_id);
    }
    if let Some(files) = &input.files {
        anyhow::ensure!(
            (1..=1000).contains(&files.len()),
            "select between 1 and 1000 files"
        );
    }
    Ok(input)
}

impl RunFileSelector {
    fn parts(&self) -> Result<(&str, &str, Option<PathBuf>)> {
        let (run, filename, relative) = match self {
            Self::Filename(filename) => (
                "",
                filename.strip_prefix("artifact:").unwrap_or(filename),
                None,
            ),
            Self::Selector {
                run_id,
                file_ref,
                relative_path,
            } => (
                run_id.trim(),
                file_ref
                    .strip_prefix("artifact:")
                    .context("only artifact:<path> references can be downloaded")?,
                relative_path.as_deref(),
            ),
        };
        anyhow::ensure!(
            run.is_empty() || valid_id(run),
            "invalid file selector run_id"
        );
        validate_artifact_path(filename)?;
        Ok((run, filename, relative.map(safe_artifact_path).transpose()?))
    }
}

pub(super) fn explicit_selectors(
    files: Option<&[RunFileSelector]>,
) -> Result<Option<Vec<ArtifactSelector>>> {
    files
        .map(|files| {
            files
                .iter()
                .map(|file| {
                    let (run_id, filename, _) = file.parts()?;
                    Ok(ArtifactSelector {
                        run_id: run_id.to_string(),
                        file_ref: format!("artifact:{filename}"),
                    })
                })
                .collect()
        })
        .transpose()
}

pub(super) fn select_files(
    requested: Option<&[RunFileSelector]>,
    files: Vec<ArtifactDownload>,
    multiplayer: bool,
) -> Result<Vec<(ArtifactDownload, PathBuf)>> {
    let mut outputs = HashMap::new();
    if let Some(requested) = requested {
        anyhow::ensure!(
            (1..=1000).contains(&requested.len()),
            "select between 1 and 1000 files"
        );
        for selector in requested {
            let (run_id, filename, relative) = selector.parts()?;
            let matches: Vec<_> = files
                .iter()
                .enumerate()
                .filter(|(_, file)| {
                    file.filename == filename
                        && (run_id.is_empty() || file.composite_run_id == run_id)
                })
                .map(|(index, _)| index)
                .collect();
            anyhow::ensure!(
                !matches.is_empty(),
                "Artifact '{run_id}/artifact:{filename}' was not found in this run"
            );
            anyhow::ensure!(
                relative.is_none() || matches.len() == 1,
                "Artifact '{filename}' exists for multiple players; supply run_id or remove relative_path"
            );
            for index in matches {
                anyhow::ensure!(
                    outputs.insert(index, relative.clone()).is_none(),
                    "files selects '{filename}' more than once"
                );
            }
        }
    } else {
        outputs.extend((0..files.len()).map(|index| (index, None)));
    }
    files
        .into_iter()
        .enumerate()
        .filter_map(|(index, file)| {
            outputs.remove(&index).map(|relative| {
                validate_artifact_path(&file.filename)?;
                let output = if let Some(relative) = relative {
                    relative
                } else {
                    let original = safe_artifact_path(&file.filename)?;
                    if multiplayer {
                        PathBuf::from(format!("player-{}", file.player_number)).join(original)
                    } else {
                        original
                    }
                };
                Ok((file, output))
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::ContentBlock;

    #[tokio::test]
    async fn a_bare_run_id_resolves_its_project_through_the_authenticated_upstream() {
        use rmcp::{
            ServerHandler, ServiceExt,
            model::{
                ClientCapabilities, ClientInfo, Implementation, ServerCapabilities, ServerInfo,
            },
            service::RequestContext,
        };
        struct Inspector;
        impl ServerHandler for Inspector {
            fn get_info(&self) -> ServerInfo {
                ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            }
            fn call_tool(
                &self,
                request: CallToolRequestParams,
                _context: RequestContext<rmcp::RoleServer>,
            ) -> impl Future<Output = std::result::Result<CallToolResponse, rmcp::ErrorData>> + Send + '_
            {
                assert_eq!(request.name, "inspect_run");
                assert_eq!(request.arguments.expect("arguments")["run_id"], "run-1");
                std::future::ready(Ok(CallToolResult::success(vec![ContentBlock::text(
                    "{\"project_id\":\"project-1\"}",
                )])
                .into()))
            }
        }
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (server, upstream) = tokio::join!(
            Inspector.serve(tokio::io::split(server_io)),
            ClientInfo::new(
                ClientCapabilities::default(),
                Implementation::new("test", "1")
            )
            .serve(tokio::io::split(client_io))
        );
        let mut server = server.expect("server");
        let mut upstream = upstream.expect("upstream");
        let root = tempfile::tempdir().expect("root");
        let tool = super::super::DownloadRunFilesTool::new(
            config(),
            root.path().canonicalize().expect("canonical root"),
        )
        .with_upstream(upstream.peer().clone());
        let input = super::resolve(
            parse(serde_json::json!({"run_id":"run-1"})),
            &tool.config,
            tool.upstream.as_ref(),
        )
        .await
        .expect("resolve run");
        assert_eq!(input.project_id, "project-1");
        assert_eq!(input.destination_dir, "downloads/project-1/run-1");
        upstream.close().await.expect("close upstream");
        server.close().await.expect("close server");
    }

    #[test]
    fn project_lookup_accepts_structured_or_text_results_and_rejects_errors_and_traversal() {
        let mut result =
            CallToolResult::success(vec![ContentBlock::text("{\"project_id\":\"project-1\"}")]);
        assert_eq!(
            project_from_inspection(&result).expect("project"),
            "project-1"
        );
        result.structured_content = Some(serde_json::json!({"project_id": "project-2"}));
        assert_eq!(
            project_from_inspection(&result).expect("structured project"),
            "project-2"
        );
        result.is_error = Some(true);
        assert!(project_from_inspection(&result).is_err());
        for value in ["", "../foreign", "https://foreign.test", "project/../other"] {
            let result = CallToolResult::success(vec![ContentBlock::text(
                serde_json::json!({"project_id":value}).to_string(),
            )]);
            assert!(project_from_inspection(&result).is_err());
        }
    }

    fn config() -> Config {
        Config::new("secret".to_string(), "https://nunu.ai/api").expect("config")
    }

    fn parse(value: serde_json::Value) -> DownloadRunFilesInput {
        serde_json::from_value(value).expect("input")
    }

    #[tokio::test]
    async fn a_run_link_is_enough_and_defaults_are_stable() {
        for link in [
            "https://nunu.ai/nexus/project-1/runs/c/player-1/details?tab=files",
            "/nexus/project-1/runs/m/player-1",
            "https://nunu.ai/api/v1/project/project-1/runs/player-1",
        ] {
            let input = resolve(parse(serde_json::json!({"run_id": link})), &config())
                .await
                .expect("resolve");
            assert_eq!(input.project_id, "project-1");
            assert_eq!(input.run_id, "player-1");
            assert_eq!(input.destination_dir, "downloads/project-1/player-1");
        }
    }

    #[tokio::test]
    async fn configured_project_and_legacy_path_still_work() {
        let mut config = config();
        config.project_id = Some("default-project".to_string());
        let input = resolve(
            parse(serde_json::json!({"run_id": "run-1", "path": "my-files"})),
            &config,
        )
        .await
        .expect("resolve");
        assert_eq!(input.project_id, "default-project");
        assert_eq!(input.destination_dir, "my-files");
        let linked = resolve(
            parse(serde_json::json!({"run_id": "/nexus/link-project/runs/c/run-1"})),
            &config,
        )
        .await
        .expect("link");
        assert_eq!(linked.project_id, "link-project");
    }

    #[tokio::test]
    async fn missing_context_has_actionable_errors_and_conflicting_links_fail() {
        let error = resolve(parse(serde_json::json!({})), &config())
            .await
            .expect_err("missing run")
            .to_string();
        assert!(error.contains("Which run"));
        assert!(error.contains("list_runs"));
        assert!(
            resolve(parse(serde_json::json!({"run_id": "run-1"})), &config())
                .await
                .expect_err("project")
                .to_string()
                .contains("full Nunu run link")
        );
        for value in [
            serde_json::json!({"run_id": "https://other.test/nexus/p/runs/c/r"}),
            serde_json::json!({"run_id": "https://nunu.ai/nexus/p/runs/c/r", "project_id": "other"}),
            serde_json::json!({"run_id": "../escape", "project_id": "project"}),
        ] {
            assert!(resolve(parse(value), &config()).await.is_err());
        }
    }

    #[test]
    fn plain_filenames_and_objects_without_player_ids_select_all_matching_players() {
        let files = ["player-1", "player-2"].map(|id| ArtifactDownload {
            composite_run_id: id.to_string(),
            player_number: 1,
            filename: "logs/output.txt".to_string(),
            size_bytes: 1,
            etag: "v1".to_string(),
            download_url: "https://storage.test/a".to_string(),
        });
        let input = parse(serde_json::json!({"files": ["logs/output.txt"]}));
        assert_eq!(
            serde_json::to_value(
                explicit_selectors(input.files.as_deref()).expect("API selectors")
            )
            .expect("serialized selectors"),
            serde_json::json!([{"file_ref":"artifact:logs/output.txt"}])
        );
        let selected =
            select_files(input.files.as_deref(), files.to_vec(), true).expect("matching players");
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[1].0.composite_run_id, "player-2");
        for selection in [
            serde_json::json!(["logs/output.txt", {"file_ref": "artifact:report.json"}]),
            serde_json::json!([{"file_ref": "artifact:logs/output.txt", "relative_path": "one.txt"}]),
            serde_json::json!(["logs/output.txt", {"run_id": "player-1", "file_ref": "artifact:logs/output.txt"}]),
            serde_json::json!([{"run_id": "player-3", "file_ref": "artifact:logs/output.txt"}]),
            serde_json::json!([]),
        ] {
            let input = parse(serde_json::json!({"files": selection}));
            assert!(select_files(input.files.as_deref(), files.to_vec(), true).is_err());
        }
    }

    async fn resolve(
        input: DownloadRunFilesInput,
        config: &Config,
    ) -> Result<DownloadRunFilesInput> {
        super::resolve(input, config, None).await
    }
}
