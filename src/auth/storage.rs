use crate::error::{Error, Result};
use directories::ProjectDirs;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

const KEYRING_SERVICE: &str = "nunu-cli";
const KEYRING_USER: &str = "default";

#[derive(Clone, Serialize, Deserialize)]
pub struct StoredOAuthCredential {
    pub client_id: String,
    pub token_endpoint: String,
    pub mcp_url: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

impl std::fmt::Debug for StoredOAuthCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StoredOAuthCredential")
            .field("client_id", &self.client_id)
            .field("token_endpoint", &self.token_endpoint)
            .field("mcp_url", &self.mcp_url)
            .field("access_token", &"[REDACTED]")
            .field("refresh_token", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .field("scope", &self.scope)
            .finish()
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StoredCredential {
    ApiKey { api_key: String },
    OAuth(StoredOAuthCredential),
}

impl std::fmt::Debug for StoredCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ApiKey { .. } => formatter
                .debug_struct("ApiKey")
                .field("api_key", &"[REDACTED]")
                .finish(),
            Self::OAuth(credential) => formatter
                .debug_struct("OAuth")
                .field("client_id", &credential.client_id)
                .field("token_endpoint", &credential.token_endpoint)
                .field("mcp_url", &credential.mcp_url)
                .field("access_token", &"[REDACTED]")
                .field("refresh_token", &"[REDACTED]")
                .field("expires_at", &credential.expires_at)
                .field("scope", &credential.scope)
                .finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StorageBackend {
    Keyring,
    File,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StorageMetadata {
    backend: StorageBackend,
}

/// User-scoped credential persistence.
///
/// The native credential store is preferred. A user-readable-only JSON file is
/// used when the native store is unavailable (common on headless Linux hosts).
#[derive(Clone, Debug)]
pub struct CredentialStorage {
    directory: PathBuf,
    use_keyring: bool,
}

impl CredentialStorage {
    /// Locate the operating-system-specific Nunu configuration directory.
    ///
    /// # Errors
    ///
    /// Returns an error when a user configuration directory cannot be located.
    pub fn discover() -> Result<Self> {
        let project_dirs = ProjectDirs::from("", "", "nunu").ok_or_else(|| {
            Error::ConfigError("Could not determine the user configuration directory".to_string())
        })?;

        Ok(Self {
            directory: project_dirs.config_dir().to_path_buf(),
            use_keyring: true,
        })
    }

    #[cfg(test)]
    pub(crate) fn file_only(directory: PathBuf) -> Self {
        Self {
            directory,
            use_keyring: false,
        }
    }

    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Load the active saved credential.
    ///
    /// # Errors
    ///
    /// Returns an error when saved credential metadata or contents are invalid.
    pub fn load(&self) -> Result<Option<StoredCredential>> {
        match self.read_metadata()? {
            Some(StorageMetadata {
                backend: StorageBackend::Keyring,
            }) => self.load_from_keyring(),
            Some(StorageMetadata {
                backend: StorageBackend::File,
            }) => self.load_from_file(),
            None => {
                if let Some(credential) = self.load_from_keyring()? {
                    return Ok(Some(credential));
                }
                self.load_from_file()
            }
        }
    }

    /// Persist a credential, preferring the operating-system credential store.
    ///
    /// Returns `true` when the permission-restricted file fallback was used.
    ///
    /// # Errors
    ///
    /// Returns an error when neither storage backend can persist the credential.
    pub fn save(&self, credential: &StoredCredential) -> Result<bool> {
        self.ensure_directory()?;
        let serialized = serde_json::to_string(credential)?;

        if self.use_keyring && Self::save_to_keyring(&serialized).is_ok() {
            self.write_metadata(StorageBackend::Keyring)?;
            remove_if_exists(&self.credentials_path())?;
            return Ok(false);
        }

        atomic_write(&self.credentials_path(), serialized.as_bytes())?;
        self.write_metadata(StorageBackend::File)?;
        Ok(true)
    }

    /// Remove credentials from every supported local backend.
    ///
    /// # Errors
    ///
    /// Returns an error if permission-restricted credential files cannot be
    /// removed. Missing credentials are not an error.
    pub fn delete(&self) -> Result<()> {
        if self.use_keyring
            && let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
        {
            let _ = entry.delete_credential();
        }

        remove_if_exists(&self.credentials_path())?;
        remove_if_exists(&self.metadata_path())?;
        Ok(())
    }

    /// Acquire the cross-process credential refresh lock.
    ///
    /// # Errors
    ///
    /// Returns an error when the lock file cannot be created or locked.
    pub fn lock_refresh(&self) -> Result<CredentialRefreshLock> {
        self.ensure_directory()?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.lock_path())?;
        file.lock_exclusive().map_err(|error| {
            Error::AuthError(format!("Failed to lock OAuth credentials: {error}"))
        })?;
        Ok(CredentialRefreshLock { file })
    }

    fn ensure_directory(&self) -> Result<()> {
        fs::create_dir_all(&self.directory)?;
        set_directory_permissions(&self.directory)?;
        Ok(())
    }

    fn credentials_path(&self) -> PathBuf {
        self.directory.join("credentials.json")
    }

    fn metadata_path(&self) -> PathBuf {
        self.directory.join("credential-storage.json")
    }

    fn lock_path(&self) -> PathBuf {
        self.directory.join("credentials.lock")
    }

    fn read_metadata(&self) -> Result<Option<StorageMetadata>> {
        let path = self.metadata_path();
        if !path.exists() {
            return Ok(None);
        }
        let contents = fs::read_to_string(&path).map_err(|error| {
            Error::ConfigError(format!(
                "Failed to read credential metadata '{}': {error}",
                path.display()
            ))
        })?;
        serde_json::from_str(&contents).map(Some).map_err(|error| {
            Error::ConfigError(format!(
                "Failed to parse credential metadata '{}': {error}",
                path.display()
            ))
        })
    }

    fn write_metadata(&self, backend: StorageBackend) -> Result<()> {
        let serialized = serde_json::to_vec_pretty(&StorageMetadata { backend })?;
        atomic_write(&self.metadata_path(), &serialized)
    }

    fn load_from_file(&self) -> Result<Option<StoredCredential>> {
        let path = self.credentials_path();
        if !path.exists() {
            return Ok(None);
        }

        validate_file_permissions(&path)?;
        let contents = fs::read_to_string(&path)?;
        serde_json::from_str(&contents).map(Some).map_err(|error| {
            Error::ConfigError(format!(
                "Failed to parse saved credentials '{}': {error}",
                path.display()
            ))
        })
    }

    fn load_from_keyring(&self) -> Result<Option<StoredCredential>> {
        if !self.use_keyring {
            return Ok(None);
        }

        let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER) else {
            return Ok(None);
        };
        let serialized = match entry.get_password() {
            Ok(value) => value,
            Err(keyring::Error::NoEntry) => return Ok(None),
            Err(error) => {
                return Err(Error::AuthError(format!(
                    "Could not read credentials from the operating-system credential store: {error}"
                )));
            }
        };

        serde_json::from_str(&serialized).map(Some).map_err(|error| {
            Error::ConfigError(format!(
                "Saved credentials in the operating-system credential store are invalid: {error}"
            ))
        })
    }

    fn save_to_keyring(serialized: &str) -> std::result::Result<(), keyring::Error> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)?;
        entry.set_password(serialized)
    }
}

pub struct CredentialRefreshLock {
    file: File,
}

impl Drop for CredentialRefreshLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        Error::ConfigError(format!(
            "Credential path '{}' has no parent",
            path.display()
        ))
    })?;
    fs::create_dir_all(parent)?;
    set_directory_permissions(parent)?;

    let temporary_path = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("nunu-credentials"),
        std::process::id()
    ));

    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    set_create_file_mode(&mut options);
    let mut file = options.open(&temporary_path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    fs::rename(&temporary_path, path)?;
    set_file_permissions(path)?;
    Ok(())
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn set_create_file_mode(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
}

#[cfg(not(unix))]
fn set_create_file_mode(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn set_directory_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_directory_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn validate_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path)?.permissions().mode();
    if mode & 0o077 != 0 {
        return Err(Error::AuthError(format!(
            "Credential file '{}' is accessible by other users; run chmod 600 on it",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_fallback_round_trip_and_delete() {
        let temporary_directory = tempfile::tempdir().expect("create temp directory");
        let storage = CredentialStorage::file_only(temporary_directory.path().join("nunu"));
        let credential = StoredCredential::ApiKey {
            api_key: "nunu_test_secret".to_string(),
        };

        assert!(storage.save(&credential).expect("save credential"));
        assert!(matches!(
            storage.load().expect("load credential"),
            Some(StoredCredential::ApiKey { api_key }) if api_key == "nunu_test_secret"
        ));

        storage.delete().expect("delete credential");
        assert!(storage.load().expect("load deleted credential").is_none());
    }

    #[test]
    fn debug_output_redacts_secrets() {
        let credential = StoredCredential::OAuth(StoredOAuthCredential {
            client_id: "client".to_string(),
            token_endpoint: "https://auth.example/token".to_string(),
            mcp_url: "https://example.com/mcp".to_string(),
            access_token: "access-secret".to_string(),
            refresh_token: "refresh-secret".to_string(),
            expires_at: 42,
            scope: None,
        });

        let debug = format!("{credential:?}");
        assert!(!debug.contains("access-secret"));
        assert!(!debug.contains("refresh-secret"));

        let StoredCredential::OAuth(oauth) = credential else {
            unreachable!("test credential is OAuth");
        };
        let oauth_debug = format!("{oauth:?}");
        assert!(!oauth_debug.contains("access-secret"));
        assert!(!oauth_debug.contains("refresh-secret"));
    }

    #[cfg(unix)]
    #[test]
    fn file_fallback_uses_and_enforces_user_only_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let temporary_directory = tempfile::tempdir().expect("create temp directory");
        let storage = CredentialStorage::file_only(temporary_directory.path().join("nunu"));
        storage
            .save(&StoredCredential::ApiKey {
                api_key: "secret".to_string(),
            })
            .expect("save credential");

        let credentials_path = storage.credentials_path();
        assert_eq!(
            fs::metadata(&credentials_path)
                .expect("credential metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        fs::set_permissions(&credentials_path, fs::Permissions::from_mode(0o644))
            .expect("make credential file unsafe");
        assert!(storage.load().is_err());
    }
}
