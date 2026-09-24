//! Per-installation capture identity. The private key never leaves this directory.
use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    PublicKeyData,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
struct Identity {
    certificate: String,
    key: String,
    name: String,
    fingerprint: String,
}
pub(crate) struct CaptureCa {
    pub issuer: Issuer<'static, KeyPair>,
    pub certificate: String,
    pub spki: String,
    pub name: String,
    pub fingerprint: String,
    pub path: PathBuf,
}
pub(crate) fn private_directory(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("缺少状态目录")?;
    private_directory(dir)?;
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}
impl CaptureCa {
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        private_directory(dir)?;
        let identity_path = dir.join("identity.json");
        let identity: Identity = if identity_path.exists() {
            serde_json::from_slice(&std::fs::read(&identity_path)?)
                .context("捕获证书损坏；请保留文件并重新配置，不能自动替换已信任的证书")?
        } else {
            let key = KeyPair::generate()?;
            let id = format!("{:x}", Sha256::digest(key.subject_public_key_info()));
            let name = format!("FFDownload Capture {}", &id[..8]);
            let mut params = CertificateParams::default();
            params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            params.key_usages = vec![
                KeyUsagePurpose::DigitalSignature,
                KeyUsagePurpose::KeyCertSign,
                KeyUsagePurpose::CrlSign,
            ];
            params.not_before = time::OffsetDateTime::now_utc() - time::Duration::days(1);
            params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(730);
            params.distinguished_name.push(DnType::CommonName, &name);
            let cert = params.self_signed(&key)?;
            let identity = Identity {
                certificate: cert.pem(),
                key: key.serialize_pem(),
                name,
                fingerprint: format!("{:X}", Sha256::digest(cert.der())),
            };
            atomic_write(&identity_path, &serde_json::to_vec(&identity)?)?;
            identity
        };
        let key = KeyPair::from_pem(&identity.key).context("无法读取捕获证书私钥")?;
        let spki = STANDARD.encode(Sha256::digest(key.subject_public_key_info()));
        let issuer = Issuer::from_ca_cert_pem(&identity.certificate, key)?;
        let path = dir.join("FFDownload-Capture.cer");
        if std::fs::read_to_string(&path).ok().as_deref() != Some(&identity.certificate) {
            atomic_write(&path, identity.certificate.as_bytes())?;
        }
        Ok(Self {
            issuer,
            spki,
            certificate: identity.certificate,
            name: identity.name,
            fingerprint: identity.fingerprint,
            path,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn same_installation_keeps_identity_and_exports_only_certificate() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let first = CaptureCa::load_or_create(a.path()).unwrap();
        let again = CaptureCa::load_or_create(a.path()).unwrap();
        let other = CaptureCa::load_or_create(b.path()).unwrap();
        assert_eq!(first.certificate, again.certificate);
        assert_eq!(first.spki, again.spki);
        assert_ne!(first.spki, other.spki);
        assert!(!std::fs::read_to_string(first.path)
            .unwrap()
            .contains("PRIVATE KEY"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(a.path().join("identity.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
}
