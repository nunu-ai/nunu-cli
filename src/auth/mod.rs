//! Authentication, credential storage, and OAuth lifecycle support.

mod oauth;
mod provider;
mod storage;

pub use oauth::{OAuthLoginOptions, login_with_oauth};
pub use provider::{AuthHeader, CredentialProvider};
pub use storage::{CredentialStorage, StoredCredential, StoredOAuthCredential};

use crate::error::{Error, Result};

pub const DEFAULT_BASE_URL: &str = "https://nunu.ai";

/// Derive an API or MCP endpoint from a Nunu deployment base URL.
/// Localhost HTTP URLs are accepted for local development.
///
/// # Errors
///
/// Returns an error when the base URL is insecure, includes a path, or is not
/// a valid absolute URL.
pub fn endpoint_url(base_url: &str, endpoint: &str) -> Result<String> {
    let mut url = oauth::parse_secure_url(base_url, "Nunu base URL")?;
    if !matches!(url.path(), "" | "/") || url.query().is_some() || url.fragment().is_some() {
        return Err(Error::ConfigError(
            "Nunu base URL must contain only the scheme and host (for example, https://nunu.ai)"
                .to_string(),
        ));
    }
    url.set_path(&format!("/{}", endpoint.trim_matches('/')));
    Ok(url.to_string().trim_end_matches('/').to_string())
}

/// Save an API key in the configured credential store.
///
/// # Errors
///
/// Returns an error when the API key is empty or cannot be persisted.
pub fn save_api_key(storage: &CredentialStorage, api_key: String) -> Result<bool> {
    if api_key.trim().is_empty() {
        return Err(Error::AuthError("API key cannot be empty".to_string()));
    }

    storage.save(&StoredCredential::ApiKey { api_key })
}

/// Verify that a credential can initialize the configured remote MCP.
///
/// # Errors
///
/// Returns an error when the endpoint is invalid, unreachable, or rejects the
/// credential.
pub async fn validate_mcp_credential(mcp_url: &str, credential: StoredCredential) -> Result<()> {
    let url = oauth::parse_secure_url(mcp_url, "MCP URL")?;
    let provider = CredentialProvider::from_credential(credential, None)?;
    provider.validate_mcp_destination(mcp_url)?;
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {
                "name": "nunu-cli-auth-check",
                "version": env!("CARGO_PKG_VERSION")
            }
        }
    });
    let http = crate::tls::http_client_builder()?
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let response = provider
        .send_authenticated(
            http.post(url)
                .header("Accept", "application/json, text/event-stream")
                .header("MCP-Protocol-Version", "2025-11-25")
                .json(&request),
        )
        .await?;

    if response.status().is_success() {
        return Ok(());
    }

    let status = response.status();
    Err(Error::AuthError(format!(
        "The Nunu MCP rejected the credential with status {status}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_endpoints_from_one_base_url() {
        assert_eq!(
            endpoint_url("https://nunu.ai/", "api").expect("API URL"),
            "https://nunu.ai/api"
        );
        assert_eq!(
            endpoint_url("http://localhost:3000", "/mcp/").expect("local MCP URL"),
            "http://localhost:3000/mcp"
        );
    }

    #[test]
    fn rejects_ambiguous_or_insecure_base_urls() {
        assert!(endpoint_url("https://nunu.ai/something", "api").is_err());
        assert!(endpoint_url("http://nunu.ai", "api").is_err());
    }
}
