use crate::error::Result;
use directories::ProjectDirs;
use log::debug;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Configuration loaded from JSON file
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FileConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_token: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_url: Option<String>,
}

impl FileConfig {
    /// Load config from a specific path
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or parsed
    pub fn load_from_path(path: &PathBuf) -> Result<Self> {
        debug!("Loading config from: {}", path.display());
        let contents = std::fs::read_to_string(path)?;
        let config: FileConfig = serde_json::from_str(&contents)?;
        Ok(config)
    }

    /// Load config with fallback priority:
    /// 1. Explicit path (if provided)
    /// 2. Project directory (./nunu.json or ./.nunu/config.json)
    /// 3. User config directory (~/.config/nunu/config.json)
    ///
    /// # Errors
    ///
    /// Returns an error only if an explicit path is provided but cannot be read
    pub fn load_with_fallback(explicit_path: Option<&PathBuf>) -> Result<Self> {
        // If explicit path is provided, it must succeed
        if let Some(path) = explicit_path {
            return Self::load_from_path(path);
        }

        // Try project directory locations
        let project_paths = vec![
            PathBuf::from("./nunu.json"),
            PathBuf::from("./.nunu/config.json"),
            PathBuf::from("./config.json"),
            PathBuf::from("./.config.json"),
        ];

        for path in &project_paths {
            if path.exists() {
                match Self::load_from_path(path) {
                    Ok(config) => {
                        debug!("Loaded config from project directory: {}", path.display());
                        return Ok(config);
                    }
                    Err(e) => {
                        debug!("Failed to load config from {}: {}", path.display(), e);
                    }
                }
            }
        }

        // Try user config directory
        if let Some(proj_dirs) = ProjectDirs::from("", "", "nunu") {
            let user_config_path = proj_dirs.config_dir().join("config.json");
            if user_config_path.exists() {
                match Self::load_from_path(&user_config_path) {
                    Ok(config) => {
                        debug!(
                            "Loaded config from user directory: {}",
                            user_config_path.display()
                        );
                        return Ok(config);
                    }
                    Err(e) => {
                        debug!(
                            "Failed to load config from {}: {}",
                            user_config_path.display(),
                            e
                        );
                    }
                }
            }
        }

        // No config file found, return empty config
        debug!("No config file found, using defaults");
        Ok(FileConfig::default())
    }

    /// Merge with another config, preferring values from self
    #[must_use]
    pub fn merge_with(&self, other: &FileConfig) -> Self {
        FileConfig {
            api_token: self.api_token.clone().or_else(|| other.api_token.clone()),
            project_id: self.project_id.clone().or_else(|| other.project_id.clone()),
            api_url: self.api_url.clone().or_else(|| other.api_url.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_merge_with() {
        let config1 = FileConfig {
            api_token: Some("token1".to_string()),
            project_id: None,
            api_url: Some("url1".to_string()),
        };

        let config2 = FileConfig {
            api_token: Some("token2".to_string()),
            project_id: Some("project2".to_string()),
            api_url: Some("url2".to_string()),
        };

        let merged = config1.merge_with(&config2);

        assert_eq!(merged.api_token, Some("token1".to_string()));
        assert_eq!(merged.project_id, Some("project2".to_string()));
        assert_eq!(merged.api_url, Some("url1".to_string()));
    }
}
