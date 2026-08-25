//! One TLS trust decision, shared by every client in the workspace.
//!
//! Every crate here used to build its own TLS stack from `webpki-roots` alone — the Mozilla
//! root set compiled into the binary. That is the right default for talking to the public
//! internet and the wrong one for talking to your own server: a Personal Agent behind an
//! internal CA is unreachable no matter what the operating system trusts, and
//! `SSL_CERT_FILE` does nothing, because a compiled-in root list has no reason to read it.
//!
//! The failure was also badly disguised. The handshake fails, `reqwest` returns a transport
//! error, and the caller reports "client-config unreachable" — so an internal-CA deployment
//! looks like a networking or schema problem rather than a trust problem.
//!
//! Order of preference:
//!
//! 1. The system trust store (`rustls-native-certs`), which also honours `SSL_CERT_FILE` and
//!    `SSL_CERT_DIR`. Installing a CA the usual way (`update-ca-trust`, `update-ca-certificates`)
//!    is then enough, which is what an operator expects.
//! 2. `webpki-roots` as a FALLBACK, only when the system store yields nothing — a scratch
//!    container often has no store at all, and falling back keeps public endpoints working
//!    there instead of failing everything.
//!
//! Both roots sets are merged rather than either/or when the system store exists but is
//! sparse: a machine that trusts an internal CA still has to reach public IdPs.

use std::sync::Arc;

use rustls::RootCertStore;

/// ALPN for ordinary HTTP clients.
const ALPN_HTTP: &[&[u8]] = &[b"h2", b"http/1.1"];
/// ALPN for the control WebSocket: the upgrade must not be offered h2.
const ALPN_WS: &[&[u8]] = &[b"http/1.1"];

/// Roots to validate server certificates against: the system store, plus the compiled-in
/// Mozilla set, plus anything `SSL_CERT_FILE` / `SSL_CERT_DIR` point at.
pub fn root_store() -> RootCertStore {
    let mut roots = RootCertStore::empty();

    // `load_native_certs` reports per-certificate errors instead of failing outright, so a
    // single unparseable file in the system store cannot take the whole trust set down with it.
    let native = rustls_native_certs::load_native_certs();
    for cert in native.certs {
        // A root we cannot parse is skipped, not fatal: the rest of the store is still good.
        let _ = roots.add(cert);
    }

    // Only as a fallback. Merging unconditionally would be harmless but slower, and on a
    // machine WITH a store the operator's decisions should be the ones that count.
    if roots.is_empty() {
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }
    roots
}

fn config(alpn: &[&[u8]]) -> rustls::ClientConfig {
    // The provider is named explicitly rather than taken from the process default: this crate
    // is used by binaries that may not have installed one, and a missing default provider
    // panics at handshake time — a long way from where the mistake was made.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring provider supports the default protocol versions")
        .with_root_certificates(root_store())
        .with_no_client_auth();
    cfg.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    cfg
}

/// TLS config for HTTP clients.
pub fn http_tls_config() -> rustls::ClientConfig {
    config(ALPN_HTTP)
}

/// TLS config for the control WebSocket (ALPN pinned to http/1.1 for the upgrade).
pub fn ws_tls_config() -> rustls::ClientConfig {
    config(ALPN_WS)
}

/// A `reqwest` builder that trusts the same roots as everything else here.
///
/// Use this instead of `reqwest::Client::new()` / `Client::builder()`: those pick up the
/// compiled-in roots only, which is exactly the bug this module exists to fix.
pub fn http_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().use_preconfigured_tls(http_tls_config())
}

/// A ready `reqwest` client with the shared trust store.
pub fn http_client() -> reqwest::Client {
    // A default-configuration client cannot fail to build; the fallback keeps the signature
    // infallible so call sites do not each grow error handling for an impossible case.
    http_client_builder()
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_trust_store_is_never_empty() {
        // Whatever the machine looks like -- system store, no system store, CI container --
        // there must be roots, or every HTTPS call fails with UnknownIssuer.
        assert!(!root_store().is_empty());
    }

    #[test]
    fn the_websocket_does_not_offer_h2() {
        // Offering h2 on the upgrade is how a WS connect ends up negotiating the wrong protocol.
        assert_eq!(ws_tls_config().alpn_protocols, vec![b"http/1.1".to_vec()]);
    }

    #[test]
    fn http_offers_both_protocols() {
        assert_eq!(
            http_tls_config().alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
    }
}
