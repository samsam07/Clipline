//! TLS setup for the mesh (D6; locked decisions #7 TLS-over-TCP, #10 trusted-LAN no-auth).
//!
//! The mesh is a **trusted LAN with no authentication** — pairing/identity keys are
//! Phase 2. So TLS here provides **confidentiality only**: each node generates an
//! ephemeral self-signed certificate at startup (never persisted — there is no TOFU/cert
//! pinning without authentication), the server presents it, and the client **accepts any
//! certificate** ([`AcceptAnyServerCert`]). The handshake *signature* is still verified,
//! so the peer must possess the key for the cert it presents — that is the one bit of
//! integrity available without a trust anchor.
//!
//! ⚠️ **Phase 2** replaces [`AcceptAnyServerCert`] with a real pairing/identity verifier;
//! this is the single, well-contained place trust is skipped. The `ring` provider is used
//! (not the default aws-lc-rs) to avoid a NASM/C build dependency on Windows-first.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{
    ring, verify_tls12_signature, verify_tls13_signature, WebPkiSupportedAlgorithms,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, ServerConfig, SignatureScheme};

use crate::error::MeshError;

/// Build the `(client, server)` TLS configs for the mesh from a fresh self-signed cert.
pub(crate) fn build_tls() -> Result<(Arc<ClientConfig>, Arc<ServerConfig>), MeshError> {
    let provider = Arc::new(ring::default_provider());

    // Ephemeral self-signed cert, regenerated every launch (not persisted — no auth, so
    // nothing pins it). The subject name is cosmetic: the client accepts any cert.
    let certified = rcgen::generate_simple_self_signed(vec!["clipline".to_owned()])
        .map_err(|e| MeshError::Tls(format!("self-signed cert: {e}")))?;
    let cert_der: CertificateDer<'static> = certified.cert.der().clone();
    let key_der =
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der()));

    let server = ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| MeshError::Tls(format!("server versions: {e}")))?
        .with_no_client_auth() // server-cert only; no client cert in v1 (D6)
        .with_single_cert(vec![cert_der], key_der)
        .map_err(|e| MeshError::Tls(format!("server cert: {e}")))?;

    let verifier = Arc::new(AcceptAnyServerCert {
        supported: provider.signature_verification_algorithms,
    });
    let client = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| MeshError::Tls(format!("client versions: {e}")))?
        .dangerous() // the accept-any verifier below is the Phase-2 swap point
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();

    Ok((Arc::new(client), Arc::new(server)))
}

/// Accepts **any** server certificate (D6; locked #10) — the LAN is trusted and there is
/// no identity to verify against in v1. The handshake signature is still checked, so the
/// server proves possession of its (untrusted) key. NEVER use off a trusted LAN.
#[derive(Debug)]
struct AcceptAnyServerCert {
    supported: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        // No trust anchor to check against (Phase 2 adds one).
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.supported)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.supported)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.supported.supported_schemes()
    }
}
