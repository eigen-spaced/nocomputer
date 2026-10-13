//! Device identity: a self-signed certificate and its private key, named by fingerprint.
//!
//! Peers are pinned by the SHA-256 of the whole certificate (DER), not by name or CA.
//! Certificates are never reissued, so the fingerprint is stable; a new identity
//! means pairing again. Validity dates are not checked for pinned certificates.

use crate::Error;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::fmt;
use std::str::FromStr;

/// Fixed TLS server name. Hostnames are never matched; identity comes from the pin.
pub const SERVER_NAME: &str = "nocomputer";
const CERT_SUBJECT: &str = "nocomputer device";
const ENCODING_MAGIC: &[u8; 5] = b"NCID1";

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    pub fn of(cert: &CertificateDer<'_>) -> Self {
        let digest = ring::digest::digest(&ring::digest::SHA256, cert.as_ref());
        let mut bytes = [0; 32];
        bytes.copy_from_slice(digest.as_ref());
        Fingerprint(bytes)
    }
}

/// Full SHA-256 in 16 groups of four hex digits. Never truncated for display.
impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, pair) in self.0.chunks(2).enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }
            write!(f, "{:02X}{:02X}", pair[0], pair[1])?;
        }
        Ok(())
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({self})")
    }
}

impl FromStr for Fingerprint {
    type Err = Error;

    /// Accepts 64 hex digits, optionally separated by spaces or colons.
    fn from_str(s: &str) -> Result<Self, Error> {
        let hex: Vec<u8> = s.bytes().filter(|b| *b != b' ' && *b != b':').collect();
        if hex.len() != 64 {
            return Err(Error::BadFingerprint);
        }
        let mut bytes = [0; 32];
        for (i, pair) in hex.chunks(2).enumerate() {
            let text = std::str::from_utf8(pair).map_err(|_| Error::BadFingerprint)?;
            bytes[i] = u8::from_str_radix(text, 16).map_err(|_| Error::BadFingerprint)?;
        }
        Ok(Fingerprint(bytes))
    }
}

impl serde::Serialize for Fingerprint {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for Fingerprint {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

pub struct Identity {
    cert: CertificateDer<'static>,
    key: PrivatePkcs8KeyDer<'static>,
}

impl Identity {
    /// New ECDSA P-256 key and self-signed certificate.
    pub fn generate() -> Result<Self, Error> {
        let key = rcgen::KeyPair::generate()?;
        let mut params = rcgen::CertificateParams::new(vec![SERVER_NAME.to_string()])?;
        params.distinguished_name = rcgen::DistinguishedName::new();
        params.distinguished_name.push(rcgen::DnType::CommonName, CERT_SUBJECT);
        let cert = params.self_signed(&key)?;
        Ok(Identity { cert: cert.der().clone(), key: PrivatePkcs8KeyDer::from(key.serialize_der()) })
    }

    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint::of(&self.cert)
    }

    pub(crate) fn cert(&self) -> CertificateDer<'static> {
        self.cert.clone()
    }

    pub(crate) fn key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(self.key.clone_key())
    }

    /// Serialized form for a [`crate::KeyStore`]. Contains the private key.
    pub fn to_bytes(&self) -> Vec<u8> {
        let (cert, key) = (self.cert.as_ref(), self.key.secret_pkcs8_der());
        let mut out = Vec::with_capacity(ENCODING_MAGIC.len() + 8 + cert.len() + key.len());
        out.extend_from_slice(ENCODING_MAGIC);
        for part in [cert, key] {
            out.extend_from_slice(&(part.len() as u32).to_be_bytes());
            out.extend_from_slice(part);
        }
        out
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self, Error> {
        let rest = data.strip_prefix(ENCODING_MAGIC.as_slice()).ok_or(Error::BadIdentity)?;
        let (cert, rest) = take_part(rest)?;
        let (key, rest) = take_part(rest)?;
        if !rest.is_empty() {
            return Err(Error::BadIdentity);
        }
        let identity = Identity { cert: CertificateDer::from(cert.to_vec()), key: PrivatePkcs8KeyDer::from(key.to_vec()) };
        // The key must load and belong to the certificate.
        let provider = rustls::crypto::ring::default_provider();
        let signing_key = provider.key_provider.load_private_key(identity.key()).map_err(|_| Error::BadIdentity)?;
        rustls::sign::CertifiedKey::new(vec![identity.cert()], signing_key)
            .keys_match()
            .map_err(|_| Error::BadIdentity)?;
        Ok(identity)
    }
}

fn take_part(data: &[u8]) -> Result<(&[u8], &[u8]), Error> {
    let len_bytes: [u8; 4] = data.get(..4).ok_or(Error::BadIdentity)?.try_into().unwrap();
    let len = u32::from_be_bytes(len_bytes) as usize;
    let rest = &data[4..];
    if rest.len() < len {
        return Err(Error::BadIdentity);
    }
    Ok(rest.split_at(len))
}

/// Shows the fingerprint only, never key material.
impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Identity({})", self.fingerprint())
    }
}
