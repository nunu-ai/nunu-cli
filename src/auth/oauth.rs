use crate::auth::storage::{StoredCredential, StoredOAuthCredential};
use crate::error::{Error, Result};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::redirect::Policy;
use ring::{digest, rand as ring_rand};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use url::Url;

const CALLBACK_PATH: &str = "/callback";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub struct OAuthLoginOptions {
    pub mcp_url: String,
    pub open_browser: bool,
}

#[derive(Debug, Deserialize)]
struct ProtectedResourceMetadata {
    resource: String,
    authorization_servers: Vec<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct AuthorizationServerMetadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
    #[serde(default)]
    code_challenge_methods_supported: Vec<String>,
}

#[derive(Debug, Serialize)]
struct DynamicRegistrationRequest {
    client_name: &'static str,
    redirect_uris: Vec<String>,
    grant_types: Vec<&'static str>,
    response_types: Vec<&'static str>,
    token_endpoint_auth_method: &'static str,
    application_type: &'static str,
}

#[derive(Debug, Deserialize)]
struct DynamicRegistrationResponse {
    client_id: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    token_type: String,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Deserialize)]
struct OAuthErrorResponse {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

struct AuthorizationCallback {
    code: String,
    state: String,
}

/// Complete an OAuth 2.1 authorization-code flow with PKCE.
///
/// # Errors
///
/// Returns an error when discovery, registration, browser authorization,
/// callback validation, or token exchange fails.
pub async fn login_with_oauth(options: &OAuthLoginOptions) -> Result<StoredCredential> {
    let http = oauth_http_client()?;
    let mcp_url = parse_secure_url(&options.mcp_url, "MCP URL")?;
    let protected_metadata_url = protected_resource_metadata_url(&mcp_url)?;
    let protected_operation =
        format!("OAuth protected-resource discovery at '{protected_metadata_url}'");
    let protected: ProtectedResourceMetadata =
        get_json(&http, &protected_metadata_url, &protected_operation).await?;
    validate_advertised_url(&protected.resource, &mcp_url, "OAuth protected resource")?;

    let authorization_server = protected.authorization_servers.first().ok_or_else(|| {
        Error::AuthError("OAuth metadata did not name an authorization server".to_string())
    })?;
    let authorization_server = parse_secure_url(authorization_server, "authorization server URL")?;
    let metadata_url = authorization_server_metadata_url(&authorization_server)?;
    let metadata_operation = format!("OAuth authorization-server discovery at '{metadata_url}'");
    let metadata: AuthorizationServerMetadata =
        get_json(&http, &metadata_url, &metadata_operation).await?;

    validate_issuer(&metadata.issuer, &authorization_server)?;

    ensure_endpoint_is_secure(&metadata.authorization_endpoint, "authorization endpoint")?;
    ensure_endpoint_is_secure(&metadata.token_endpoint, "token endpoint")?;
    if !metadata.code_challenge_methods_supported.is_empty()
        && !metadata
            .code_challenge_methods_supported
            .iter()
            .any(|method| method.eq_ignore_ascii_case("S256"))
    {
        return Err(Error::AuthError(
            "The authorization server does not support PKCE-S256".to_string(),
        ));
    }

    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");

    let registration_endpoint = metadata.registration_endpoint.as_deref().ok_or_else(|| {
        Error::AuthError(
            "The authorization server does not support dynamic client registration".to_string(),
        )
    })?;
    ensure_endpoint_is_secure(registration_endpoint, "registration endpoint")?;
    let client_id = register_client(&http, registration_endpoint, &redirect_uri).await?;

    let verifier = random_url_safe(64)?;
    let challenge = URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, verifier.as_bytes()));
    let state = random_url_safe(32)?;
    let scope = select_scopes(&protected.scopes_supported, &metadata.scopes_supported);

    let mut authorization_url = Url::parse(&metadata.authorization_endpoint)
        .map_err(|error| Error::AuthError(format!("Invalid authorization endpoint: {error}")))?;
    {
        let mut query = authorization_url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", &client_id)
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state)
            .append_pair("resource", mcp_url.as_str());
        if let Some(scope) = scope.as_deref() {
            query.append_pair("scope", scope);
        }
    }

    eprintln!("Authorize Nunu in your browser:");
    eprintln!("{authorization_url}");
    if options.open_browser && !is_remote_shell() {
        match open::that_detached(authorization_url.as_str()) {
            Ok(()) => eprintln!("Opened the authorization page. Waiting for approval..."),
            Err(error) => eprintln!(
                "Could not open a browser ({error}). Open the URL above manually on this machine."
            ),
        }
    } else {
        eprintln!("Open the URL above in a browser on this machine. Waiting for approval...");
    }

    let callback = tokio::time::timeout(LOGIN_TIMEOUT, receive_callback(listener, &state))
        .await
        .map_err(|_| Error::AuthError("OAuth login timed out after 5 minutes".to_string()))??;

    let token_response = exchange_code(
        &http,
        &metadata.token_endpoint,
        &client_id,
        &callback.code,
        &verifier,
        &redirect_uri,
        mcp_url.as_str(),
    )
    .await?;

    let refresh_token = token_response.refresh_token.ok_or_else(|| {
        Error::AuthError("The authorization server did not return a refresh token".to_string())
    })?;
    validate_bearer_token_type(&token_response.token_type)?;

    Ok(StoredCredential::OAuth(StoredOAuthCredential {
        client_id,
        token_endpoint: metadata.token_endpoint,
        mcp_url: mcp_url.to_string(),
        access_token: token_response.access_token,
        refresh_token,
        expires_at: expiry_from_now(token_response.expires_in),
        scope: token_response.scope.or(scope),
    }))
}

pub(super) async fn refresh_oauth_credential(
    http: &reqwest::Client,
    credential: &StoredOAuthCredential,
) -> Result<StoredOAuthCredential> {
    ensure_endpoint_is_secure(&credential.token_endpoint, "token endpoint")?;

    let form = [
        ("grant_type", "refresh_token"),
        ("refresh_token", credential.refresh_token.as_str()),
        ("client_id", credential.client_id.as_str()),
        ("resource", credential.mcp_url.as_str()),
    ];
    let response = http
        .post(&credential.token_endpoint)
        .form(&form)
        .send()
        .await?;
    let token_response: TokenResponse =
        parse_json_response(response, "OAuth token refresh").await?;
    validate_bearer_token_type(&token_response.token_type)?;

    Ok(StoredOAuthCredential {
        client_id: credential.client_id.clone(),
        token_endpoint: credential.token_endpoint.clone(),
        mcp_url: credential.mcp_url.clone(),
        access_token: token_response.access_token,
        refresh_token: token_response
            .refresh_token
            .unwrap_or_else(|| credential.refresh_token.clone()),
        expires_at: expiry_from_now(token_response.expires_in),
        scope: token_response.scope.or_else(|| credential.scope.clone()),
    })
}

pub(super) fn oauth_http_client() -> Result<reqwest::Client> {
    crate::tls::ensure_crypto_provider()?;
    reqwest::Client::builder()
        .redirect(Policy::none())
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(Error::from)
}

async fn register_client(
    http: &reqwest::Client,
    registration_endpoint: &str,
    redirect_uri: &str,
) -> Result<String> {
    let request = DynamicRegistrationRequest {
        client_name: "Nunu CLI",
        redirect_uris: vec![redirect_uri.to_string()],
        grant_types: vec!["authorization_code", "refresh_token"],
        response_types: vec!["code"],
        token_endpoint_auth_method: "none",
        application_type: "native",
    };
    let response = http
        .post(registration_endpoint)
        .json(&request)
        .send()
        .await?;
    let registration: DynamicRegistrationResponse =
        parse_json_response(response, "OAuth client registration").await?;
    if registration.client_id.trim().is_empty() {
        return Err(Error::AuthError(
            "OAuth client registration returned an empty client ID".to_string(),
        ));
    }
    Ok(registration.client_id)
}

async fn exchange_code(
    http: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
    resource: &str,
) -> Result<TokenResponse> {
    let form = [
        ("grant_type", "authorization_code"),
        ("client_id", client_id),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", redirect_uri),
        ("resource", resource),
    ];
    let response = http.post(token_endpoint).form(&form).send().await?;
    parse_json_response(response, "OAuth code exchange").await
}

async fn get_json<T: for<'de> Deserialize<'de>>(
    http: &reqwest::Client,
    url: &Url,
    operation: &str,
) -> Result<T> {
    let response = http.get(url.clone()).send().await?;
    parse_json_response(response, operation).await
}

async fn parse_json_response<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
    operation: &str,
) -> Result<T> {
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        let oauth_error = serde_json::from_str::<OAuthErrorResponse>(&body).ok();
        if oauth_error
            .as_ref()
            .is_some_and(|error| error.error == "invalid_grant")
        {
            return Err(Error::AuthSessionExpired);
        }
        let detail = oauth_error.map_or_else(
            || "the authorization server rejected the request".to_string(),
            |error| match error.error_description {
                Some(description) => format!("{}: {description}", error.error),
                None => error.error,
            },
        );
        return Err(Error::AuthError(format!(
            "{operation} failed with status {status}: {detail}"
        )));
    }
    serde_json::from_str(&body)
        .map_err(|error| Error::AuthError(format!("{operation} returned invalid JSON: {error}")))
}

fn protected_resource_metadata_url(resource: &Url) -> Result<Url> {
    let mut metadata = resource.clone();
    let resource_path = resource.path().trim_start_matches('/');
    let path = if resource_path.is_empty() {
        "/.well-known/oauth-protected-resource".to_string()
    } else {
        format!("/.well-known/oauth-protected-resource/{resource_path}")
    };
    metadata.set_path(&path);
    metadata.set_query(None);
    metadata.set_fragment(None);
    ensure_url_is_secure(&metadata, "protected-resource metadata URL")?;
    Ok(metadata)
}

fn authorization_server_metadata_url(server: &Url) -> Result<Url> {
    let mut metadata = server.clone();
    let issuer_path = server.path().trim_matches('/');
    let path = if issuer_path.is_empty() {
        "/.well-known/oauth-authorization-server".to_string()
    } else {
        format!("/.well-known/oauth-authorization-server/{issuer_path}")
    };
    metadata.set_path(&path);
    metadata.set_query(None);
    metadata.set_fragment(None);
    ensure_url_is_secure(&metadata, "authorization-server metadata URL")?;
    Ok(metadata)
}

pub(super) fn parse_secure_url(value: &str, label: &str) -> Result<Url> {
    let url =
        Url::parse(value).map_err(|error| Error::AuthError(format!("Invalid {label}: {error}")))?;
    ensure_url_is_secure(&url, label)?;
    Ok(url)
}

fn ensure_endpoint_is_secure(value: &str, label: &str) -> Result<()> {
    let _ = parse_secure_url(value, label)?;
    Ok(())
}

fn validate_issuer(advertised: &str, expected: &Url) -> Result<()> {
    validate_advertised_url(advertised, expected, "OAuth authorization server issuer")
}

fn validate_advertised_url(advertised: &str, expected: &Url, label: &str) -> Result<()> {
    let advertised = parse_secure_url(advertised, label)?;
    if advertised == *expected {
        return Ok(());
    }
    Err(Error::AuthError(format!(
        "{label} mismatch: expected '{expected}', received '{advertised}'"
    )))
}

fn ensure_url_is_secure(url: &Url, label: &str) -> Result<()> {
    let localhost = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if url.scheme() != "https" && !(url.scheme() == "http" && localhost) {
        return Err(Error::AuthError(format!(
            "{label} must use HTTPS (HTTP is allowed only for localhost)"
        )));
    }
    if url.username() != "" || url.password().is_some() {
        return Err(Error::AuthError(format!(
            "{label} must not contain embedded credentials"
        )));
    }
    Ok(())
}

fn select_scopes(resource_scopes: &[String], server_scopes: &[String]) -> Option<String> {
    if !resource_scopes.is_empty() {
        return Some(resource_scopes.join(" "));
    }

    let supported = server_scopes
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let defaults = ["openid", "email", "profile"]
        .into_iter()
        .filter(|scope| supported.contains(scope))
        .collect::<Vec<_>>();
    (!defaults.is_empty()).then(|| defaults.join(" "))
}

fn random_url_safe(byte_count: usize) -> Result<String> {
    use ring_rand::SecureRandom as _;

    let random = ring_rand::SystemRandom::new();
    let mut bytes = vec![0_u8; byte_count];
    random
        .fill(&mut bytes)
        .map_err(|_| Error::AuthError("The operating system RNG failed".to_string()))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

async fn receive_callback(
    listener: TcpListener,
    expected_state: &str,
) -> Result<AuthorizationCallback> {
    loop {
        let (mut stream, peer) = listener.accept().await?;
        if !peer.ip().is_loopback() {
            continue;
        }

        match read_callback_request(&mut stream).await {
            Ok(callback) => {
                if let Err(error) = verify_state(expected_state, &callback.state) {
                    write_browser_response(
                        &mut stream,
                        "400 Bad Request",
                        "Nunu CLI could not complete authentication. Return to the terminal for details.",
                    )
                    .await?;
                    return Err(error);
                }
                write_browser_response(
                    &mut stream,
                    "200 OK",
                    "Nunu CLI is authenticated. You can close this window.",
                )
                .await?;
                return Ok(callback);
            }
            Err(error) => {
                write_browser_response(
                    &mut stream,
                    "400 Bad Request",
                    "Nunu CLI could not complete authentication. Return to the terminal for details.",
                )
                .await?;
                return Err(error);
            }
        }
    }
}

async fn read_callback_request(stream: &mut TcpStream) -> Result<AuthorizationCallback> {
    let mut buffer = vec![0_u8; 16 * 1024];
    let mut bytes_read = 0;
    while bytes_read < buffer.len() {
        let read = stream.read(&mut buffer[bytes_read..]).await?;
        if read == 0 {
            break;
        }
        bytes_read += read;
        if buffer[..bytes_read]
            .windows(4)
            .any(|window| window == b"\r\n\r\n")
        {
            break;
        }
    }
    if bytes_read == 0 {
        return Err(Error::AuthError("OAuth callback was empty".to_string()));
    }
    if !buffer[..bytes_read]
        .windows(4)
        .any(|window| window == b"\r\n\r\n")
    {
        return Err(Error::AuthError(
            "OAuth callback headers were incomplete or too large".to_string(),
        ));
    }

    let request = std::str::from_utf8(&buffer[..bytes_read])
        .map_err(|_| Error::AuthError("OAuth callback was not valid HTTP".to_string()))?;
    let request_line = request
        .lines()
        .next()
        .ok_or_else(|| Error::AuthError("OAuth callback was missing a request line".to_string()))?;
    let mut parts = request_line.split_whitespace();
    if parts.next() != Some("GET") {
        return Err(Error::AuthError(
            "OAuth callback used an unsupported HTTP method".to_string(),
        ));
    }
    let target = parts
        .next()
        .ok_or_else(|| Error::AuthError("OAuth callback was missing its URL".to_string()))?;
    let callback_url = Url::parse(&format!("http://127.0.0.1{target}"))
        .map_err(|_| Error::AuthError("OAuth callback URL was invalid".to_string()))?;
    if callback_url.path() != CALLBACK_PATH {
        return Err(Error::AuthError(
            "OAuth callback used an unexpected path".to_string(),
        ));
    }

    let parameters = callback_url
        .query_pairs()
        .collect::<std::collections::HashMap<_, _>>();
    if let Some(error) = parameters.get("error") {
        let description = parameters
            .get("error_description")
            .map_or_else(|| error.as_ref(), AsRef::as_ref);
        return Err(Error::AuthError(format!(
            "Authorization was denied: {description}"
        )));
    }

    let code = parameters
        .get("code")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::AuthError("OAuth callback did not contain a code".to_string()))?;
    let state = parameters
        .get("state")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::AuthError("OAuth callback did not contain state".to_string()))?;

    Ok(AuthorizationCallback {
        code: code.to_string(),
        state: state.to_string(),
    })
}

async fn write_browser_response(stream: &mut TcpStream, status: &str, message: &str) -> Result<()> {
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Nunu CLI</title></head>\
         <body><main><h1>{message}</h1></main></body></html>"
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

fn verify_state(expected: &str, received: &str) -> Result<()> {
    let expected = expected.as_bytes();
    let received = received.as_bytes();
    let lengths_match = expected.len() == received.len();
    let difference = expected
        .iter()
        .zip(received)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        });

    if lengths_match && difference == 0 {
        Ok(())
    } else {
        Err(Error::AuthError(
            "OAuth callback state did not match".to_string(),
        ))
    }
}

fn validate_bearer_token_type(token_type: &str) -> Result<()> {
    if token_type.eq_ignore_ascii_case("bearer") {
        Ok(())
    } else {
        Err(Error::AuthError(format!(
            "Unsupported OAuth token type '{token_type}'"
        )))
    }
}

fn expiry_from_now(expires_in: Option<u64>) -> u64 {
    unix_timestamp().saturating_add(expires_in.unwrap_or(3600))
}

pub(super) fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn is_remote_shell() -> bool {
    ["SSH_TTY", "SSH_CONNECTION", "SSH_CLIENT"]
        .iter()
        .any(|name| std::env::var_os(name).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_protected_resource_metadata_url() {
        let resource = Url::parse("https://nunu.ai/mcp").expect("valid URL");
        assert_eq!(
            protected_resource_metadata_url(&resource)
                .expect("metadata URL")
                .as_str(),
            "https://nunu.ai/.well-known/oauth-protected-resource/mcp"
        );
    }

    #[test]
    fn builds_path_aware_authorization_metadata_url() {
        let server = Url::parse("https://example.supabase.co/auth/v1").expect("valid URL");
        assert_eq!(
            authorization_server_metadata_url(&server)
                .expect("metadata URL")
                .as_str(),
            "https://example.supabase.co/.well-known/oauth-authorization-server/auth/v1"
        );
    }

    #[test]
    fn rejects_insecure_remote_urls() {
        assert!(parse_secure_url("http://example.com/mcp", "MCP URL").is_err());
        assert!(parse_secure_url("http://127.0.0.1:8080/mcp", "MCP URL").is_ok());
    }

    #[test]
    fn chooses_resource_scopes_first() {
        assert_eq!(
            select_scopes(
                &["project:read".to_string(), "project:write".to_string()],
                &["openid".to_string()]
            ),
            Some("project:read project:write".to_string())
        );
    }
}
