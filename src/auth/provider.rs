use crate::auth::oauth::{oauth_http_client, refresh_oauth_credential, unix_timestamp};
use crate::auth::storage::{CredentialStorage, StoredCredential};
use crate::error::{Error, Result};
use std::sync::Arc;
use tokio::sync::Mutex;

const REFRESH_EARLY_SECONDS: u64 = 60;

#[derive(Clone)]
pub enum AuthHeader {
    ApiKey(String),
    Bearer(String),
}

impl std::fmt::Debug for AuthHeader {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ApiKey(_) => formatter.write_str("ApiKey([REDACTED])"),
            Self::Bearer(_) => formatter.write_str("Bearer([REDACTED])"),
        }
    }
}

impl AuthHeader {
    pub fn apply(self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self {
            Self::ApiKey(api_key) => request.header("x-api-key", api_key),
            Self::Bearer(access_token) => request.bearer_auth(access_token),
        }
    }
}

#[derive(Clone)]
pub struct CredentialProvider {
    state: Arc<Mutex<StoredCredential>>,
    storage: Option<CredentialStorage>,
    http: reqwest::Client,
    oauth_resource: Option<String>,
}

impl std::fmt::Debug for CredentialProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CredentialProvider")
            .field("state", &"[REDACTED]")
            .field("storage", &self.storage)
            .field("oauth_resource", &self.oauth_resource)
            .field("http", &"reqwest::Client")
            .finish()
    }
}

impl CredentialProvider {
    /// Create an ephemeral API-key provider, normally for an environment
    /// variable or one-command override.
    ///
    /// # Errors
    ///
    /// Returns an error when the API key is empty or the HTTP client cannot be
    /// constructed.
    pub fn api_key(api_key: String) -> Result<Self> {
        if api_key.trim().is_empty() {
            return Err(Error::AuthError("API key cannot be empty".to_string()));
        }
        Ok(Self {
            state: Arc::new(Mutex::new(StoredCredential::ApiKey { api_key })),
            storage: None,
            oauth_resource: None,
            http: oauth_http_client()?,
        })
    }

    /// Load the active credential from persistent user storage.
    ///
    /// # Errors
    ///
    /// Returns an error when no saved credential exists or it cannot be read.
    pub fn load(storage: CredentialStorage) -> Result<Self> {
        let credential = storage.load()?.ok_or_else(|| {
            Error::AuthError(
                "No credentials found. Run 'nunu-cli auth login' or set NUNU_API_KEY.".to_string(),
            )
        })?;
        Self::from_credential(credential, Some(storage))
    }

    /// Create a provider around an already loaded credential.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client cannot be constructed.
    pub fn from_credential(
        credential: StoredCredential,
        storage: Option<CredentialStorage>,
    ) -> Result<Self> {
        Ok(Self {
            oauth_resource: match &credential {
                StoredCredential::OAuth(oauth) => Some(oauth.mcp_url.clone()),
                StoredCredential::ApiKey { .. } => None,
            },
            state: Arc::new(Mutex::new(credential)),
            storage,
            http: oauth_http_client()?,
        })
    }

    /// Check that saved OAuth credentials belong to this exact MCP resource.
    ///
    /// # Errors
    /// Returns an error for invalid URLs or a different deployment/resource.
    pub fn validate_mcp_destination(&self, mcp_url: &str) -> Result<()> {
        if let Some(resource) = &self.oauth_resource {
            let saved = crate::auth::oauth::parse_secure_url(resource, "Saved MCP resource")?;
            let destination = crate::auth::oauth::parse_secure_url(mcp_url, "MCP destination")?;
            if saved != destination {
                return Err(Error::AuthError(
                    "Saved OAuth credentials belong to a different MCP resource. Log in to the selected deployment.".to_string(),
                ));
            }
        }
        Ok(())
    }

    /// Check that uploads target the API of the saved OAuth deployment.
    ///
    /// # Errors
    /// Returns an error when the API does not belong to the saved deployment.
    pub fn validate_api_destination(&self, api_url: &str) -> Result<()> {
        if self.oauth_resource.is_some() {
            let mut destination = crate::auth::oauth::parse_secure_url(api_url, "API destination")?;
            if destination.path().trim_end_matches('/') != "/api"
                || destination.query().is_some()
                || destination.fragment().is_some()
            {
                return Err(Error::AuthError(
                    "OAuth uploads require the deployment's /api endpoint".to_string(),
                ));
            }
            destination.set_path("/mcp");
            self.validate_mcp_destination(destination.as_str())?;
        }
        Ok(())
    }

    /// Resolve the current request header, refreshing OAuth credentials first
    /// when needed.
    ///
    /// # Errors
    ///
    /// Returns an error when refreshing or persisting OAuth credentials fails.
    pub async fn header(&self) -> Result<AuthHeader> {
        let mut state = self.state.lock().await;
        match &*state {
            StoredCredential::ApiKey { api_key } => Ok(AuthHeader::ApiKey(api_key.clone())),
            StoredCredential::OAuth(credential)
                if credential.expires_at
                    > unix_timestamp().saturating_add(REFRESH_EARLY_SECONDS) =>
            {
                Ok(AuthHeader::Bearer(credential.access_token.clone()))
            }
            StoredCredential::OAuth(_) => {
                self.refresh_locked(&mut state, None).await?;
                match &*state {
                    StoredCredential::OAuth(credential) => {
                        Ok(AuthHeader::Bearer(credential.access_token.clone()))
                    }
                    StoredCredential::ApiKey { api_key } => Ok(AuthHeader::ApiKey(api_key.clone())),
                }
            }
        }
    }

    /// Send an authenticated request, refreshing OAuth credentials proactively
    /// and retrying one replayable request after an HTTP 401 response.
    ///
    /// # Errors
    ///
    /// Returns an error when authentication, token refresh, or the HTTP request
    /// fails.
    pub async fn send_authenticated(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        let retry = request.try_clone();
        let header = self.header().await?;
        let rejected_access_token = match &header {
            AuthHeader::Bearer(access_token) => Some(access_token.clone()),
            AuthHeader::ApiKey(_) => None,
        };
        let response = header.apply(request).send().await?;

        if response.status() != reqwest::StatusCode::UNAUTHORIZED {
            return Ok(response);
        }

        let (Some(retry), Some(rejected_access_token)) = (retry, rejected_access_token) else {
            return Ok(response);
        };
        self.refresh_after_unauthorized(&rejected_access_token)
            .await?;
        self.header()
            .await?
            .apply(retry)
            .send()
            .await
            .map_err(Into::into)
    }

    pub async fn snapshot(&self) -> StoredCredential {
        self.state.lock().await.clone()
    }

    pub(crate) async fn refresh_after_unauthorized(
        &self,
        rejected_access_token: &str,
    ) -> Result<()> {
        let mut state = self.state.lock().await;
        let StoredCredential::OAuth(credential) = &*state else {
            return Ok(());
        };
        if credential.access_token != rejected_access_token {
            return Ok(());
        }
        self.refresh_locked(&mut state, Some(rejected_access_token))
            .await
    }

    async fn refresh_locked(
        &self,
        state: &mut StoredCredential,
        rejected_access_token: Option<&str>,
    ) -> Result<()> {
        let Some(storage) = &self.storage else {
            return Err(Error::AuthError(
                "The OAuth access token expired and has no persistent refresh session. Log in again."
                    .to_string(),
            ));
        };

        let lock_storage = storage.clone();
        let refresh_lock = tokio::task::spawn_blocking(move || lock_storage.lock_refresh())
            .await
            .map_err(|error| {
                Error::AuthError(format!("OAuth refresh lock task failed: {error}"))
            })??;

        // Another process may have rotated the refresh token while this process
        // was waiting for the lock. Re-read the credential before refreshing.
        let latest = storage.load()?.ok_or(Error::AuthSessionExpired)?;
        // A later login must never switch a running provider to another deployment
        // or authentication method.
        match (&*state, &latest) {
            (StoredCredential::OAuth(current), StoredCredential::OAuth(latest))
                if current.client_id == latest.client_id
                    && current.token_endpoint == latest.token_endpoint =>
            {
                self.validate_mcp_destination(&latest.mcp_url)?;
            }
            _ => return Err(Error::AuthSessionExpired),
        }
        if let StoredCredential::OAuth(credential) = &latest {
            let already_refreshed = rejected_access_token.map_or_else(
                || credential.expires_at > unix_timestamp().saturating_add(REFRESH_EARLY_SECONDS),
                |rejected| credential.access_token != rejected,
            );
            *state = latest;
            if already_refreshed {
                return Ok(());
            }
        }

        let StoredCredential::OAuth(credential) = &*state else {
            return Ok(());
        };
        let refreshed = match refresh_oauth_credential(&self.http, credential).await {
            Ok(refreshed) => refreshed,
            Err(Error::AuthSessionExpired) => {
                refresh_lock.delete()?;
                return Err(Error::AuthSessionExpired);
            }
            Err(error) => {
                return Err(Error::AuthError(format!(
                    "Could not refresh the OAuth session. Run 'nunu-cli auth login' again. {error}"
                )));
            }
        };
        let updated = StoredCredential::OAuth(refreshed);
        refresh_lock.save(&updated)?;
        *state = updated;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::{TcpListener, TcpStream};

    fn oauth_credential() -> StoredCredential {
        StoredCredential::OAuth(crate::auth::StoredOAuthCredential {
            client_id: "test-client".to_string(),
            token_endpoint: "http://127.0.0.1:1/token".to_string(),
            mcp_url: "https://nunu.ai/mcp".to_string(),
            access_token: "old-access".to_string(),
            refresh_token: "old-refresh".to_string(),
            expires_at: 0,
            scope: None,
        })
    }

    #[test]
    fn oauth_destinations_are_bound_to_the_saved_resource() {
        let provider =
            CredentialProvider::from_credential(oauth_credential(), None).expect("provider");
        assert!(
            provider
                .validate_mcp_destination("https://NUNU.ai:443/mcp")
                .is_ok()
        );
        assert!(
            provider
                .validate_api_destination("https://nunu.ai/api/")
                .is_ok()
        );
        for destination in [
            "https://other.example/mcp",
            "https://nunu.ai:8443/mcp",
            "http://localhost/mcp",
            "https://nunu.ai/other",
            "https://nunu.ai/mcp?other",
            "https://nunu.ai/mcp#other",
        ] {
            assert!(
                provider.validate_mcp_destination(destination).is_err(),
                "{destination}"
            );
        }
        for destination in [
            "https://other.example/api",
            "https://nunu.ai:8443/api",
            "https://nunu.ai/other",
        ] {
            assert!(
                crate::config::Config::with_credential(provider.clone(), destination, None)
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn logout_prevents_proactive_and_unauthorized_refresh() {
        let directory = tempfile::tempdir().expect("directory");
        let storage = CredentialStorage::file_only(directory.path().to_path_buf());
        storage.save(&oauth_credential()).expect("save");
        let provider = CredentialProvider::load(storage.clone()).expect("provider");
        storage.delete().expect("logout");
        assert!(matches!(
            provider.header().await,
            Err(Error::AuthSessionExpired)
        ));
        assert!(matches!(
            provider.refresh_after_unauthorized("old-access").await,
            Err(Error::AuthSessionExpired)
        ));
        assert!(storage.load().expect("load").is_none());
    }

    #[tokio::test]
    async fn refresh_rejects_a_login_to_another_deployment() {
        let directory = tempfile::tempdir().expect("directory");
        let storage = CredentialStorage::file_only(directory.path().to_path_buf());
        storage.save(&oauth_credential()).expect("save");
        let provider = CredentialProvider::load(storage.clone()).expect("provider");
        let StoredCredential::OAuth(mut replacement) = oauth_credential() else {
            unreachable!()
        };
        replacement.mcp_url = "https://other.example/mcp".to_string();
        replacement.access_token = "other-access".to_string();
        replacement.expires_at = unix_timestamp() + 3600;
        storage
            .save(&StoredCredential::OAuth(replacement))
            .expect("new login");
        assert!(provider.header().await.is_err());
        assert!(
            provider
                .refresh_after_unauthorized("old-access")
                .await
                .is_err()
        );
        assert!(
            matches!(storage.load().expect("load"), Some(StoredCredential::OAuth(oauth)) if oauth.access_token == "other-access")
        );
    }

    #[tokio::test]
    async fn logout_and_login_wait_for_in_flight_refresh() {
        for logout in [true, false] {
            let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .await
                .expect("listener");
            let address = listener.local_addr().expect("address");
            let directory = tempfile::tempdir().expect("directory");
            let storage = CredentialStorage::file_only(directory.path().to_path_buf());
            let StoredCredential::OAuth(mut credential) = oauth_credential() else {
                unreachable!()
            };
            credential.token_endpoint = format!("http://{address}/token");
            storage
                .save(&StoredCredential::OAuth(credential))
                .expect("save");
            let provider = CredentialProvider::load(storage.clone()).expect("provider");
            let refresh = tokio::spawn(async move { provider.header().await });
            let (mut stream, _) =
                tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept())
                    .await
                    .expect("refresh started")
                    .expect("accept");
            read_request(&mut stream).await;
            let mutation_storage = storage.clone();
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let mut mutation = tokio::task::spawn_blocking(move || {
                started_tx.send(()).expect("signal mutation");
                if logout {
                    mutation_storage.delete()
                } else {
                    mutation_storage
                        .save(&StoredCredential::ApiKey {
                            api_key: "new-login".to_string(),
                        })
                        .map(|_| ())
                }
            });
            started_rx.await.expect("mutation started");
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(100), &mut mutation)
                    .await
                    .is_err()
            );
            write_response(&mut stream, "200 OK", r#"{"access_token":"new-access","refresh_token":"new-refresh","token_type":"Bearer","expires_in":3600}"#).await;
            refresh
                .await
                .expect("refresh task")
                .expect("refresh succeeded");
            mutation
                .await
                .expect("mutation task")
                .expect("mutation succeeded");
            let saved = storage.load().expect("load");
            if logout {
                assert!(saved.is_none());
            } else {
                assert!(
                    matches!(saved, Some(StoredCredential::ApiKey { api_key }) if api_key == "new-login")
                );
            }
        }
    }

    async fn read_request(stream: &mut TcpStream) -> String {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 2048];
        loop {
            let count = stream.read(&mut buffer).await.expect("read request");
            if count == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..count]);
            let Some(headers_end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
                continue;
            };
            let headers_end = headers_end + 4;
            let headers = String::from_utf8_lossy(&request[..headers_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if request.len() >= headers_end + content_length {
                break;
            }
        }
        String::from_utf8(request).expect("UTF-8 request")
    }

    async fn write_response(stream: &mut TcpStream, status: &str, body: &str) {
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .await
            .expect("write response");
    }

    #[tokio::test]
    async fn retries_a_bearer_request_once_after_refreshing() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for step in 0..3 {
                let (mut stream, _) = listener.accept().await.expect("accept request");
                requests.push(read_request(&mut stream).await);
                match step {
                    0 => write_response(&mut stream, "401 Unauthorized", "").await,
                    1 => {
                        write_response(
                            &mut stream,
                            "200 OK",
                            r#"{"access_token":"new-access","refresh_token":"new-refresh","token_type":"Bearer","expires_in":3600}"#,
                        )
                        .await;
                    }
                    _ => write_response(&mut stream, "200 OK", r#"{"ok":true}"#).await,
                }
            }
            requests
        });

        let temporary_directory = tempfile::tempdir().expect("create temp directory");
        let storage = CredentialStorage::file_only(PathBuf::from(temporary_directory.path()));
        storage
            .save(&StoredCredential::OAuth(
                crate::auth::StoredOAuthCredential {
                    client_id: "test-client".to_string(),
                    token_endpoint: format!("http://{address}/token"),
                    mcp_url: format!("http://{address}/mcp"),
                    access_token: "old-access".to_string(),
                    refresh_token: "old-refresh".to_string(),
                    expires_at: unix_timestamp() + 3600,
                    scope: None,
                },
            ))
            .expect("save test credential");
        let provider = CredentialProvider::load(storage.clone()).expect("credential provider");
        let response = provider
            .send_authenticated(reqwest::Client::new().get(format!("http://{address}/resource")))
            .await
            .expect("authenticated request");
        assert!(response.status().is_success());

        let requests = server.await.expect("test server task");
        assert!(requests[0].contains("authorization: Bearer old-access"));
        assert!(requests[1].starts_with("POST /token "));
        assert!(requests[1].contains("refresh_token=old-refresh"));
        assert!(requests[2].contains("authorization: Bearer new-access"));

        assert!(matches!(
            storage.load().expect("load rotated credential"),
            Some(StoredCredential::OAuth(credential))
                if credential.access_token == "new-access"
                    && credential.refresh_token == "new-refresh"
        ));
    }
}
