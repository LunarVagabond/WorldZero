//! Certificate loading (or self-signed generation) for the TCP TLS
//! listener — and, per docs/specs/Networking_Spec.md, the same
//! cert/key the DTLS UDP channel reuses.

use std::path::Path;

use common::{Error, Result};
use sha2::{Digest, Sha256};

#[derive(Debug)]
pub struct CertMaterial {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
    pub fingerprint_sha256_hex: String,
}

/// `WZ_TLS_CERT_PATH`/`WZ_TLS_KEY_PATH` if both are set, otherwise a
/// freshly self-signed cert/key stored under `<config_dir>/certs/`
/// (generated once, reused on subsequent runs) — docs/specs/Networking_Spec.md,
/// "TLS (TCP channel)".
pub fn load_or_generate(config_dir: &Path) -> Result<CertMaterial> {
    match (
        std::env::var("WZ_TLS_CERT_PATH"),
        std::env::var("WZ_TLS_KEY_PATH"),
    ) {
        (Ok(cert_path), Ok(key_path)) => {
            load_from_files(Path::new(&cert_path), Path::new(&key_path))
        }
        _ => load_or_generate_self_signed(config_dir),
    }
}

fn load_from_files(cert_path: &Path, key_path: &Path) -> Result<CertMaterial> {
    let cert_pem = std::fs::read_to_string(cert_path).map_err(|e| {
        Error::wrap(
            "gateway",
            format!("failed to read WZ_TLS_CERT_PATH ({})", cert_path.display()),
            e,
        )
    })?;
    let key_pem = std::fs::read_to_string(key_path).map_err(|e| {
        Error::wrap(
            "gateway",
            format!("failed to read WZ_TLS_KEY_PATH ({})", key_path.display()),
            e,
        )
    })?;

    let cert_der = pem_to_der(&cert_pem, "CERTIFICATE")?;
    let key_der =
        pem_to_der(&key_pem, "PRIVATE KEY").or_else(|_| pem_to_der(&key_pem, "RSA PRIVATE KEY"))?;

    Ok(material_from_der(cert_der, key_der))
}

fn pem_to_der(pem: &str, label: &str) -> Result<Vec<u8>> {
    let parsed = pem::parse(pem)
        .map_err(|e| Error::wrap("gateway", "configured TLS cert/key is not valid PEM", e))?;
    if parsed.tag() != label {
        return Err(Error::new(
            "gateway",
            format!("expected a {label} PEM block, found {}", parsed.tag()),
        ));
    }
    Ok(parsed.into_contents())
}

fn load_or_generate_self_signed(config_dir: &Path) -> Result<CertMaterial> {
    let certs_dir = config_dir.join("certs");
    let cert_path = certs_dir.join("self_signed.cert.der");
    let key_path = certs_dir.join("self_signed.key.der");

    if cert_path.exists() && key_path.exists() {
        let cert_der = std::fs::read(&cert_path)
            .map_err(|e| Error::wrap("gateway", "failed to read cached self-signed cert", e))?;
        let key_der = std::fs::read(&key_path)
            .map_err(|e| Error::wrap("gateway", "failed to read cached self-signed key", e))?;
        return Ok(material_from_der(cert_der, key_der));
    }

    let generated = rcgen::generate_simple_self_signed(["localhost".to_string()])
        .map_err(|e| Error::wrap("gateway", "failed to generate a self-signed certificate", e))?;
    let cert_der = generated.cert.der().to_vec();
    let key_der = generated.signing_key.serialize_der();

    std::fs::create_dir_all(&certs_dir)
        .map_err(|e| Error::wrap("gateway", "failed to create certs directory", e))?;
    std::fs::write(&cert_path, &cert_der)
        .map_err(|e| Error::wrap("gateway", "failed to write generated cert", e))?;
    std::fs::write(&key_path, &key_der)
        .map_err(|e| Error::wrap("gateway", "failed to write generated key", e))?;

    Ok(material_from_der(cert_der, key_der))
}

fn material_from_der(cert_der: Vec<u8>, key_der: Vec<u8>) -> CertMaterial {
    let fingerprint_sha256_hex = hex_encode(&Sha256::digest(&cert_der));
    CertMaterial {
        cert_der,
        key_der,
        fingerprint_sha256_hex,
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_and_caches_a_self_signed_cert() {
        let dir = std::env::temp_dir().join(format!("wz-gateway-tls-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let first = load_or_generate_self_signed(&dir).unwrap();
        let second = load_or_generate_self_signed(&dir).unwrap();

        assert_eq!(
            first.cert_der, second.cert_der,
            "second call should reuse the cached cert, not generate a new one"
        );
        assert_eq!(first.fingerprint_sha256_hex.len(), 64);

        std::fs::remove_dir_all(&dir).ok();
    }

    // Real production deployments go through `load_from_files`
    // (`WZ_TLS_CERT_PATH`/`WZ_TLS_KEY_PATH`), not the self-signed
    // convenience path above — this had zero coverage before, which is
    // backwards risk-wise given it's what actual deployments use.

    fn temp_file(label: &str, contents: &[u8]) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);

        let path = std::env::temp_dir().join(format!(
            "wz-gateway-tls-test-{}-{label}-{n}",
            std::process::id(),
        ));
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn load_from_files_reads_a_real_cert_and_key() {
        let generated = rcgen::generate_simple_self_signed(["localhost".to_string()]).unwrap();
        let cert_der = generated.cert.der().to_vec();
        let key_der = generated.signing_key.serialize_der();

        let cert_pem = pem::Pem::new("CERTIFICATE", cert_der.clone()).to_string();
        let key_pem = pem::Pem::new("PRIVATE KEY", key_der.clone()).to_string();

        let cert_path = temp_file("cert", cert_pem.as_bytes());
        let key_path = temp_file("key", key_pem.as_bytes());

        let material = load_from_files(&cert_path, &key_path).unwrap();
        assert_eq!(material.cert_der, cert_der);
        assert_eq!(material.key_der, key_der);
        assert_eq!(material.fingerprint_sha256_hex.len(), 64);

        std::fs::remove_file(&cert_path).ok();
        std::fs::remove_file(&key_path).ok();
    }

    #[test]
    fn load_from_files_falls_back_to_the_rsa_private_key_label() {
        // `pem_to_der` only extracts the DER payload from the labeled PEM
        // block — it never parses the bytes as a real key — so arbitrary
        // bytes are enough to prove the "PRIVATE KEY" -> "RSA PRIVATE KEY"
        // label fallback (line 50) actually engages, without needing a
        // real RSA key on hand.
        let generated = rcgen::generate_simple_self_signed(["localhost".to_string()]).unwrap();
        let cert_pem = pem::Pem::new("CERTIFICATE", generated.cert.der().to_vec()).to_string();
        let fake_rsa_key_der = vec![1, 2, 3, 4, 5];
        let key_pem = pem::Pem::new("RSA PRIVATE KEY", fake_rsa_key_der.clone()).to_string();

        let cert_path = temp_file("cert-rsa", cert_pem.as_bytes());
        let key_path = temp_file("key-rsa", key_pem.as_bytes());

        let material = load_from_files(&cert_path, &key_path).unwrap();
        assert_eq!(material.key_der, fake_rsa_key_der);

        std::fs::remove_file(&cert_path).ok();
        std::fs::remove_file(&key_path).ok();
    }

    #[test]
    fn load_from_files_rejects_non_pem_content() {
        let cert_path = temp_file("cert-garbage", b"this is not PEM at all");
        let key_path = temp_file("key-garbage", b"neither is this");

        let err = load_from_files(&cert_path, &key_path).unwrap_err();
        assert!(err.to_string().contains("not valid PEM"), "{err}");

        std::fs::remove_file(&cert_path).ok();
        std::fs::remove_file(&key_path).ok();
    }

    #[test]
    fn load_from_files_rejects_a_pem_with_the_wrong_label() {
        // A cert file that's valid PEM, but not a CERTIFICATE block (e.g.
        // someone swapped the cert/key paths) must error clearly rather
        // than silently loading the wrong material.
        let wrong_label_pem = pem::Pem::new("EC PRIVATE KEY", vec![9, 9, 9]).to_string();
        let cert_path = temp_file("cert-wrong-label", wrong_label_pem.as_bytes());
        let key_path = temp_file("key-wrong-label", wrong_label_pem.as_bytes());

        let err = load_from_files(&cert_path, &key_path).unwrap_err();
        assert!(err.to_string().contains("CERTIFICATE"), "{err}");

        std::fs::remove_file(&cert_path).ok();
        std::fs::remove_file(&key_path).ok();
    }

    #[test]
    fn load_from_files_missing_file_errors_clearly() {
        let missing = std::env::temp_dir().join(format!(
            "wz-gateway-tls-test-does-not-exist-{}",
            std::process::id()
        ));

        let err = load_from_files(&missing, &missing).unwrap_err();
        assert!(err.to_string().contains("WZ_TLS_CERT_PATH"), "{err}");
    }
}
