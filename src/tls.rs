use rustls::crypto::CryptoProvider;

use crate::error::{Error, Result};

/// Ensure rustls has the Ring cryptography provider selected.
///
/// Reqwest's `rustls-no-provider` feature keeps provider selection under the
/// application's control. This function is safe to call more than once and
/// tolerates another thread installing a provider concurrently.
pub(crate) fn ensure_crypto_provider() -> Result<()> {
    if rustls::crypto::CryptoProvider::get_default().is_some() {
        return Ok(());
    }

    let _ = rustls::crypto::ring::default_provider().install_default();
    if CryptoProvider::get_default().is_none() {
        return Err(Error::ConfigError(
            "Failed to initialize the TLS cryptography provider".to_string(),
        ));
    }
    Ok(())
}
