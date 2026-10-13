//! Private identity storage in OS-protected stores: DPAPI (current user) on Windows and
//! the login Keychain on macOS. Other platforms have no store yet and refuse rather
//! than fall back to a plain file.

use crate::identity::Identity;
use crate::Error;
use std::sync::Mutex;

pub trait KeyStore: Send + Sync {
    fn load(&self) -> Result<Option<Vec<u8>>, Error>;
    fn store(&self, data: &[u8]) -> Result<(), Error>;
}

pub fn load_or_create_identity(store: &dyn KeyStore) -> Result<Identity, Error> {
    if let Some(data) = store.load()? {
        return Identity::from_bytes(&data);
    }
    let identity = Identity::generate()?;
    store.store(&identity.to_bytes())?;
    Ok(identity)
}

/// For tests only: nothing is persisted.
#[derive(Default)]
pub struct MemoryKeyStore(Mutex<Option<Vec<u8>>>);

impl KeyStore for MemoryKeyStore {
    fn load(&self) -> Result<Option<Vec<u8>>, Error> {
        Ok(self.0.lock().unwrap().clone())
    }

    fn store(&self, data: &[u8]) -> Result<(), Error> {
        *self.0.lock().unwrap() = Some(data.to_vec());
        Ok(())
    }
}

/// The platform's store. `state_dir` holds the encrypted blob where the store is a file.
/// `name` tells identities apart in shared stores (the macOS Keychain account), so the
/// pairing tool and the host app never read each other's key.
pub fn platform_keystore(state_dir: &std::path::Path, name: &str) -> Result<Box<dyn KeyStore>, Error> {
    #[cfg(windows)]
    {
        let _ = name;
        Ok(Box::new(dpapi::DpapiKeyStore::new(state_dir.join("identity.dpapi"))))
    }
    #[cfg(target_os = "macos")]
    {
        let _ = state_dir;
        Ok(Box::new(keychain::KeychainKeyStore { account: name.to_string() }))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (state_dir, name);
        Err(Error::KeyStore("no OS-protected key storage is implemented for this platform yet".into()))
    }
}

#[cfg(windows)]
pub mod dpapi {
    use super::KeyStore;
    use crate::Error;
    use std::path::PathBuf;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB};

    /// Ties the blob to this use, so another program's DPAPI blob can't be swapped in.
    const ENTROPY: &[u8] = b"nocomputer identity v1";

    /// A DPAPI blob, decryptable only by the same Windows user.
    pub struct DpapiKeyStore {
        path: PathBuf,
    }

    impl DpapiKeyStore {
        pub fn new(path: PathBuf) -> Self {
            DpapiKeyStore { path }
        }
    }

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 }
    }

    fn transform(data: &[u8], protect: bool) -> Result<Vec<u8>, Error> {
        let input = blob(data);
        let entropy = blob(ENTROPY);
        let mut output = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        // SAFETY: input/entropy point at live slices that DPAPI only reads; output is
        // allocated by DPAPI and freed with LocalFree below.
        let ok = unsafe {
            if protect {
                CryptProtectData(&input, std::ptr::null(), &entropy, std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut output)
            } else {
                CryptUnprotectData(&input, std::ptr::null_mut(), &entropy, std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut output)
            }
        };
        if ok == 0 {
            let op = if protect { "CryptProtectData" } else { "CryptUnprotectData" };
            return Err(Error::KeyStore(format!("{op}: {}", std::io::Error::last_os_error())));
        }
        // SAFETY: on success output describes a DPAPI-owned buffer of cbData bytes.
        let result = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
        unsafe { LocalFree(output.pbData as _) };
        Ok(result)
    }

    impl KeyStore for DpapiKeyStore {
        fn load(&self) -> Result<Option<Vec<u8>>, Error> {
            match std::fs::read(&self.path) {
                Ok(data) => transform(&data, false).map(Some),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            }
        }

        fn store(&self, data: &[u8]) -> Result<(), Error> {
            let protected = transform(data, true)?;
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = self.path.with_extension("tmp");
            std::fs::write(&tmp, protected)?;
            std::fs::rename(&tmp, &self.path)?;
            Ok(())
        }
    }
}

#[cfg(target_os = "macos")]
pub mod keychain {
    use super::KeyStore;
    use crate::Error;
    use security_framework::passwords::{get_generic_password, set_generic_password};

    const SERVICE: &str = "dev.nocomputer.identity";
    const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

    /// A generic-password item in the login Keychain. Its access control is bound to the
    /// code signature of the program that created it.
    pub struct KeychainKeyStore {
        pub account: String,
    }

    impl KeyStore for KeychainKeyStore {
        fn load(&self) -> Result<Option<Vec<u8>>, Error> {
            match get_generic_password(SERVICE, &self.account) {
                Ok(data) => Ok(Some(data)),
                Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
                Err(e) => Err(Error::KeyStore(format!("Keychain read: {e}"))),
            }
        }

        fn store(&self, data: &[u8]) -> Result<(), Error> {
            set_generic_password(SERVICE, &self.account, data).map_err(|e| Error::KeyStore(format!("Keychain write: {e}")))
        }
    }
}
