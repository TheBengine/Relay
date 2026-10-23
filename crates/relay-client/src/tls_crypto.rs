//! Establish one explicit provider before HTTPS/WebSocket clients choose TLS.
//!
//! The SDK and HTTP stacks can enable both Rustls providers in a unified build.
//! Automatic provider inference then panics. Keep Rustls's standard AWS-LC
//! provider, certificate verification, native roots and protocol policy intact.
pub(crate) fn initialize() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        // Another initialized caller may win this process-wide, one-time race.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn unified_tls_features_have_a_stable_provider_before_client_configuration() {
        super::initialize();
        let before = rustls::crypto::CryptoProvider::get_default().unwrap();
        super::initialize();
        let after = rustls::crypto::CryptoProvider::get_default().unwrap();
        assert!(std::sync::Arc::ptr_eq(before, after));
        let _ = rustls::ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
    }
}
