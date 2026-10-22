use super::Client;
use crate::error::{Error, Result};
use futures::StreamExt as _;
use reqwest::{StatusCode, header};
use serde::{Deserialize, Serialize};
use std::{io::Write as _, time::Duration};
use url::Url;

#[derive(Debug, Clone, Serialize)]
pub struct ArtifactSelector {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub run_id: String,
    pub file_ref: String,
}

#[derive(Debug, Deserialize)]
pub struct ArtifactManifest {
    pub players: Vec<ArtifactPlayer>,
    pub files: Vec<ArtifactDownload>,
}

#[derive(Debug, Deserialize)]
pub struct ArtifactPlayer {
    pub id: String,
    pub player_number: u64,
}

// Do not derive Debug: download_url contains a temporary storage credential.
#[derive(Clone, Deserialize)]
pub struct ArtifactDownload {
    pub composite_run_id: String,
    pub player_number: u64,
    pub filename: String,
    pub size_bytes: u64,
    pub etag: String,
    pub download_url: String,
}

impl std::fmt::Debug for ArtifactDownload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArtifactDownload")
            .field("filename", &self.filename)
            .field("size_bytes", &self.size_bytes)
            .field("etag", &self.etag)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// Resolve artifact identities to short-lived storage URLs at the trusted API.
    ///
    /// # Errors
    /// Returns an error if authorization, discovery, or response validation fails.
    pub async fn artifact_manifest(
        &self,
        run_id: &str,
        files: Option<&[ArtifactSelector]>,
    ) -> Result<ArtifactManifest> {
        let mut url = self.project_resource_url("runs", run_id)?;
        url.path_segments_mut()
            .map_err(|()| Error::ConfigError("Invalid API URL".to_string()))?
            .push("artifact-downloads");
        let body = files.map_or_else(
            || serde_json::json!({}),
            |files| serde_json::json!({ "files": files }),
        );
        let response = self
            .config
            .credential
            .send_authenticated(
                self.http
                    .post(url)
                    .timeout(Duration::from_secs(30))
                    .json(&body),
            )
            .await?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::ApiError(format!(
                "Artifact discovery failed - Status {status}: {body}"
            )));
        }
        let manifest: ArtifactManifest = response.json().await?;
        if manifest.files.len() > 1000 {
            return Err(Error::ApiError(
                "Artifact manifest exceeds 1000 files".to_string(),
            ));
        }
        Ok(manifest)
    }

    /// Stream an artifact into an already-open, empty temporary file. Storage
    /// requests never receive the user's API key or OAuth token.
    ///
    /// # Errors
    /// Returns an error for a failed request, changed version, incorrect size,
    /// or filesystem failure. Storage credentials are omitted from errors.
    pub async fn download_signed_artifact(
        &self,
        artifact: &ArtifactDownload,
        destination: &mut std::fs::File,
    ) -> Result<()> {
        let url = storage_url(&artifact.download_url)?;
        let metadata = destination.metadata()?;
        if !metadata.is_file() || metadata.len() != 0 {
            return Err(Error::ApiError(
                "Download destination must be an empty regular file".to_string(),
            ));
        }
        let etag = header::HeaderValue::from_str(&artifact.etag)
            .map_err(|_| Error::ApiError("Invalid artifact ETag".to_string()))?;
        let response = self
            .download_http
            .get(url)
            .header(header::IF_MATCH, etag)
            .send()
            .await
            .map_err(|error| Error::HttpError(error.without_url()))?;
        if response.status() != StatusCode::OK {
            return Err(Error::ApiError(format!(
                "Storage returned HTTP {}",
                response.status()
            )));
        }
        if response
            .headers()
            .get(header::ETAG)
            .and_then(|value| value.to_str().ok())
            != Some(artifact.etag.as_str())
        {
            return Err(Error::ApiError(
                "The artifact changed; retry the failed file to fetch its new version".to_string(),
            ));
        }
        let mut total = 0;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| Error::HttpError(error.without_url()))?;
            if chunk.len() as u64 > artifact.size_bytes - total {
                return Err(Error::ApiError(
                    "Storage sent more bytes than the manifest specifies".to_string(),
                ));
            }
            // Complete writes before yielding so cancellation can safely remove the temp file.
            destination.write_all(&chunk)?;
            total += chunk.len() as u64;
        }
        destination.flush()?;
        if total != artifact.size_bytes {
            return Err(Error::ApiError(format!(
                "Incomplete artifact: expected {} bytes, received {total}",
                artifact.size_bytes
            )));
        }
        Ok(())
    }
}

fn storage_url(value: &str) -> Result<Url> {
    let url = Url::parse(value).map_err(|_| Error::ApiError("Invalid storage URL".to_string()))?;
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if !(url.scheme() == "https" || url.scheme() == "http" && loopback)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::ApiError(
            "Storage URL must use HTTPS and contain no embedded credentials or fragment"
                .to_string(),
        ));
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    #[tokio::test]
    async fn rejects_partial_responses_changed_objects_and_redirects() {
        for (status, extra, expected_size, succeeds) in [
            ("200 OK", "ETag: \"v1\"\r\n", 7, true),
            ("200 OK", "ETag: \"v1\"\r\n", 6, false),
            ("200 OK", "ETag: \"v1\"\r\n", 8, false),
            ("200 OK", "ETag: \"v2\"\r\n", 7, false),
            (
                "206 Partial Content",
                "ETag: \"v1\"\r\nContent-Range: bytes 0-6/7\r\n",
                7,
                false,
            ),
            ("412 Precondition Failed", "", 7, false),
            (
                "302 Found",
                "Location: https://example.invalid/leak\r\n",
                7,
                false,
            ),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("listener");
            let address = listener.local_addr().expect("address");
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.expect("request");
                let mut bytes = [0; 4096];
                let count = socket.read(&mut bytes).await.expect("read");
                let request = String::from_utf8_lossy(&bytes[..count]).to_ascii_lowercase();
                assert!(!request.contains("range:"));
                assert!(request.contains("if-match: \"v1\""));
                assert!(!request.contains("x-api-key"));
                assert!(!request.contains("authorization:"));
                socket.write_all(format!("HTTP/1.1 {status}\r\n{extra}Content-Length: 7\r\nConnection: close\r\n\r\ncontent").as_bytes()).await.expect("response");
            });
            let mut temporary = tempfile::tempfile().expect("temporary file");
            let artifact = ArtifactDownload {
                composite_run_id: "player-1".to_string(),
                player_number: 1,
                filename: "file.txt".to_string(),
                size_bytes: expected_size,
                etag: "\"v1\"".to_string(),
                download_url: format!("http://{address}/file?signature=private"),
            };
            let client = Client::new(
                Config::new("secret".to_string(), format!("http://{address}/api")).expect("config"),
            )
            .expect("client");
            let result = client
                .download_signed_artifact(&artifact, &mut temporary)
                .await;
            assert_eq!(result.is_ok(), succeeds);
            if let Err(error) = result {
                assert!(!format!("{error:?}").contains("signature=private"));
            }
            let written = temporary.metadata().expect("metadata").len();
            assert!(written <= expected_size);
            if succeeds {
                assert_eq!(written, expected_size);
            }
            server.await.expect("server");
        }
    }

    #[tokio::test]
    async fn transport_errors_do_not_expose_storage_credentials() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.expect("request");
            drop(socket);
        });
        let artifact = ArtifactDownload {
            composite_run_id: "player-1".to_string(),
            player_number: 1,
            filename: "file.txt".to_string(),
            size_bytes: 7,
            etag: "\"v1\"".to_string(),
            download_url: format!("http://{address}/file?signature=private"),
        };
        let client = Client::new(
            Config::new("secret".to_string(), format!("http://{address}/api")).expect("config"),
        )
        .expect("client");
        let mut temporary = tempfile::tempfile().expect("temporary file");
        let error = client
            .download_signed_artifact(&artifact, &mut temporary)
            .await
            .expect_err("connection closed");
        assert!(!format!("{error:?}").contains("signature=private"));
        assert!(!format!("{error:#}").contains("signature=private"));
        server.await.expect("server");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn replacing_the_temporary_path_with_a_symlink_cannot_redirect_writes() {
        use std::{
            io::{Read as _, Seek as _},
            os::unix::fs::symlink,
        };

        let root = tempfile::tempdir().expect("workspace");
        let outside = tempfile::NamedTempFile::new().expect("outside file");
        std::fs::write(outside.path(), b"preserve me").expect("outside contents");
        let mut temporary = tempfile::NamedTempFile::new_in(root.path()).expect("temporary file");
        let temporary_path = temporary.path().to_path_buf();
        let outside_path = outside.path().to_path_buf();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request");
            let mut bytes = [0; 4096];
            assert!(socket.read(&mut bytes).await.expect("read") > 0);
            std::fs::remove_file(&temporary_path).expect("replace temporary path");
            symlink(outside_path, temporary_path)
                .expect("install symlink while request is pending");
            socket.write_all(b"HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nContent-Length: 7\r\nConnection: close\r\n\r\ncontent").await.expect("response");
        });
        let artifact = ArtifactDownload {
            composite_run_id: "player-1".to_string(),
            player_number: 1,
            filename: "file.txt".to_string(),
            size_bytes: 7,
            etag: "\"v1\"".to_string(),
            download_url: format!("http://{address}/file"),
        };
        let client = Client::new(
            Config::new("secret".to_string(), format!("http://{address}/api")).expect("config"),
        )
        .expect("client");
        client
            .download_signed_artifact(&artifact, temporary.as_file_mut())
            .await
            .expect("write through the original handle");
        server.await.expect("server");
        assert_eq!(
            std::fs::read(outside.path()).expect("outside contents"),
            b"preserve me"
        );
        temporary.as_file_mut().rewind().expect("rewind");
        let mut bytes = Vec::new();
        temporary
            .as_file_mut()
            .read_to_end(&mut bytes)
            .expect("original file");
        assert_eq!(bytes, b"content");
    }
}
