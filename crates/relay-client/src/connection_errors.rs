//! Actionable connection hints without exposing URLs or authentication material.
use std::error::Error;

pub(crate) const WINDOWS_CONNECTION_BLOCKED: &str =
    "Windows blocked the connection. Check your VPN or security app, then try again.";

/// Only the typed Windows socket error establishes this hint. PermissionDenied
/// alone also describes files; HTTP 403, certificate errors and strings do not.
pub(crate) fn windows_connection_blocked(error: &(dyn Error + 'static)) -> bool {
    #[cfg(windows)]
    {
        let mut current = Some(error);
        // Third-party source chains are not necessarily finite or acyclic.
        for _ in 0..24 {
            let Some(error) = current else { break };
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.raw_os_error() == Some(10013))
            {
                return true;
            }
            current = error.source();
        }
    }
    #[cfg(not(windows))]
    let _ = error;
    false
}

pub(crate) fn hint_or(error: &(dyn Error + 'static), fallback: &'static str) -> &'static str {
    if windows_connection_blocked(error) {
        WINDOWS_CONNECTION_BLOCKED
    } else {
        fallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fmt, io};

    #[derive(Debug)]
    struct Wrapped(Box<dyn Error + Send + Sync>);
    impl fmt::Display for Wrapped {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("https://private.invalid/?token=must-not-escape")
        }
    }
    impl Error for Wrapped {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            Some(self.0.as_ref())
        }
    }

    #[test]
    #[cfg(windows)]
    fn recognizes_typed_socket_denial_through_wrappers_without_echoing_details() {
        let error = Wrapped(Box::new(Wrapped(Box::new(io::Error::from_raw_os_error(
            10013,
        )))));
        assert_eq!(hint_or(&error, "fallback"), WINDOWS_CONNECTION_BLOCKED);
        assert!(!hint_or(&error, "fallback").contains("token"));
    }

    #[test]
    fn unrelated_permissions_certificate_status_and_strings_keep_their_own_fallback() {
        for error in [
            io::Error::from_raw_os_error(13),
            io::Error::from_raw_os_error(5),
            io::Error::from_raw_os_error(10061),
            io::Error::new(io::ErrorKind::PermissionDenied, "file access denied"),
            io::Error::other("403 Forbidden; socket 10013; certificate rejected"),
        ] {
            assert_eq!(hint_or(&Wrapped(Box::new(error)), "fallback"), "fallback");
        }
    }

    #[derive(Debug)]
    struct Cycle;
    impl fmt::Display for Cycle {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("cyclic source")
        }
    }
    impl Error for Cycle {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            Some(self)
        }
    }

    #[test]
    fn malformed_error_chain_is_bounded() {
        assert_eq!(hint_or(&Cycle, "fallback"), "fallback");
    }

    #[test]
    #[ignore = "Read-only diagnostic of the named public Friends health endpoint; no auth."]
    fn owned_public_friends_health_error_classification() {
        crate::tls_crypto::initialize();
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(8))
            .build()
            .unwrap();
        match client
            .get("https://relay-accounts.tail787aee.ts.net/people/api/health")
            .send()
        {
            Ok(response) => println!(
                "OWNED_HEALTH: status={} policy_hint=false tls_verified=true auth_sent=false",
                response.status().as_u16()
            ),
            Err(error) => {
                println!(
                    "OWNED_HEALTH: policy_hint={} connect={} timeout={} tls_verification_unchanged=true auth_sent=false",
                    windows_connection_blocked(&error), error.is_connect(), error.is_timeout()
                );
                assert!(
                    windows_connection_blocked(&error),
                    "Different connection failure; do not label it a Windows policy denial"
                );
            }
        }
    }
}
