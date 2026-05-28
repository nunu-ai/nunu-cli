use crate::error::{Error, Result};

#[derive(Clone, Debug)]
pub struct Config {
    pub token: String,
    pub api_url: String,
}

impl Config {
    /// Creates a new Config instance with the provided parameters.
    ///
    /// # Errors
    /// Returns an error if:
    /// - `token` is empty
    pub fn new(token: String, api_url: String) -> Result<Self> {
        if token.is_empty() {
            return Err(Error::ConfigError("API token cannot be empty".to_string()));
        }

        Ok(Self { token, api_url })
    }

    #[must_use]
    pub fn base_upload_url(&self) -> String {
        format!("{}/v1/builds", self.api_url)
    }
}
