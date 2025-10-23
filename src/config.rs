use crate::error::{Error, Result};

#[derive(Clone, Debug)]
pub struct Config {
    pub token: String,
    pub project_id: String,
    pub api_url: String,
}

impl Config {
    /// Creates a new Config instance with the provided parameters.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - `token` is empty
    /// - `project_id` is empty
    pub fn new(token: String, project_id: String, api_url: String) -> Result<Self> {
        if token.is_empty() {
            return Err(Error::ConfigError("API token cannot be empty".to_string()));
        }
        if project_id.is_empty() {
            return Err(Error::ConfigError("Project ID cannot be empty".to_string()));
        }

        Ok(Self {
            token,
            project_id,
            api_url,
        })
    }

    #[must_use]
    pub fn base_upload_url(&self) -> String {
        format!("{}/nexus/projects/{}/builds", self.api_url, self.project_id)
    }
}
