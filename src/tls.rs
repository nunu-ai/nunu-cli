use rustls::crypto::CryptoProvider;

use crate::error::{Error, Result};

/// Create an HTTP client builder that trusts both platform and bundled Mozilla roots.
///
/// The bundled roots keep HTTPS usable in minimal Linux environments that do not
/// install a system CA bundle. Reqwest merges these roots with native roots when
/// the platform verifier supports doing so.
pub(crate) fn http_client_builder() -> Result<reqwest::ClientBuilder> {
    ensure_crypto_provider()?;
    let bundled_roots = webpki_root_certs::TLS_SERVER_ROOT_CERTS
        .iter()
        .map(|certificate| reqwest::Certificate::from_der(certificate.as_ref()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(reqwest::Client::builder().tls_certs_merge(bundled_roots))
}

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

#[cfg(test)]
mod tests {
    #[test]
    fn builds_client_with_bundled_roots() {
        super::http_client_builder()
            .expect("configure HTTP client")
            .build()
            .expect("build HTTP client");
    }
}
