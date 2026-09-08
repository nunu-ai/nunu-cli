use crate::auth::CredentialProvider;
use crate::error::{Error, Result};
use url::{Host, Url};

#[derive(Clone, Debug)]
pub struct Config {
    pub credential: CredentialProvider,
    pub api_url: String,
    pub project_id: Option<String>,
}

impl Config {
    /// Creates a new Config instance with the provided parameters.
    ///
    /// # Errors
    /// Returns an error if:
    /// - `token` is empty
    pub fn new(token: String, api_url: impl AsRef<str>) -> Result<Self> {
        Self::with_credential(CredentialProvider::api_key(token)?, api_url.as_ref(), None)
    }

    /// Create a configuration backed by a typed credential provider.
    ///
    /// # Errors
    ///
    /// Returns an error when the API URL or project ID is invalid.
    pub fn with_credential(
        credential: CredentialProvider,
        api_url: &str,
        project_id: Option<String>,
    ) -> Result<Self> {
        let api_url = validate_api_url(api_url)?;
        if let Some(project_id) = project_id.as_deref()
            && (project_id.is_empty()
                || !project_id.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
                }))
        {
            return Err(Error::ConfigError(
                "Project ID contains invalid characters".to_string(),
            ));
        }

        Ok(Self {
            credential,
            api_url,
            project_id,
        })
    }

    #[must_use]
    pub fn base_upload_url(&self) -> String {
        self.project_id.as_ref().map_or_else(
            || format!("{}/v1/builds", self.api_url),
            |project_id| format!("{}/v1/project/{project_id}/builds", self.api_url),
        )
    }
}

fn validate_api_url(value: &str) -> Result<String> {
    let mut url = Url::parse(value.trim())
        .map_err(|error| Error::ConfigError(format!("Invalid API URL: {error}")))?;
    let loopback = match url.host() {
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    };
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return Err(Error::ConfigError(
            "API URL must use HTTPS (HTTP is allowed only for localhost)".to_string(),
        ));
    }
    if url.username() != "" || url.password().is_some() {
        return Err(Error::ConfigError(
            "API URL must not contain embedded credentials".to_string(),
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(Error::ConfigError(
            "API URL must not contain a query or fragment".to_string(),
        ));
    }
    let path = url.path().trim_end_matches('/').to_string();
    url.set_path(&path);
    Ok(url.to_string().trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_legacy_and_project_scoped_upload_urls() {
        let legacy =
            Config::new("secret".to_string(), "https://nunu.ai/api/").expect("legacy config");
        assert_eq!(legacy.base_upload_url(), "https://nunu.ai/api/v1/builds");

        let scoped = Config::with_credential(
            CredentialProvider::api_key("secret".to_string()).expect("API key"),
            "https://nunu.ai/api",
            Some("project_123".to_string()),
        )
        .expect("scoped config");
        assert_eq!(
            scoped.base_upload_url(),
            "https://nunu.ai/api/v1/project/project_123/builds"
        );
    }

    #[test]
    fn rejects_urls_that_could_expose_credentials() {
        assert!(Config::new("secret".to_string(), "http://nunu.ai/api").is_err());
        assert!(Config::new("secret".to_string(), "https://user:pass@nunu.ai/api").is_err());
        assert!(Config::new("secret".to_string(), "http://127.0.0.1:8080/api").is_ok());
    }
}
