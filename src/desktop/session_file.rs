//! Saved login for the desktop bridge: credentials sealed with the current Windows user's DPAPI key.
//!
//! The sealed payload carries the backend address and device id and is checked after decryption, so a file made
//! for another backend or device is refused instead of being sent. Credentials never leave this process: the
//! Python workbench only passes the file path. Other platforms have no protector and keep the session in memory.

use base64::Engine;
use cgeos_sdk_shared::host::SessionStore;
use cgeos_sdk_shared::protocol::{Code, Credentials};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const FORMAT_VERSION: u32 = 1;
#[cfg_attr(not(windows), allow(dead_code))]
const ENTROPY: &[u8] = b"cgeos2-workbench-session-v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionFileError {
    /// No saved session.
    Missing,
    /// Unreadable, undecryptable or structurally invalid; the file is useless and may be removed.
    Corrupt,
    /// Valid, but sealed for another backend address or device id; kept as is.
    Mismatch,
    Io(String),
}

impl std::fmt::Display for SessionFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => write!(f, "no saved session"),
            Self::Corrupt => write!(f, "saved session is damaged"),
            Self::Mismatch => write!(f, "saved session belongs to another backend or device"),
            Self::Io(message) => write!(f, "{message}"),
        }
    }
}

/// Seals and opens bytes for the current user.
pub trait Protector: Send + Sync {
    fn protect(&self, plain: &[u8]) -> Result<Vec<u8>, SessionFileError>;
    fn unprotect(&self, sealed: &[u8]) -> Result<Vec<u8>, SessionFileError>;
}

/// The platform protector: Windows DPAPI, or `None` where no per-user sealing is available.
pub fn platform_protector() -> Option<Arc<dyn Protector>> {
    #[cfg(windows)]
    {
        Some(Arc::new(dpapi::Dpapi))
    }
    #[cfg(not(windows))]
    {
        None
    }
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    format: u32,
    data: String,
}

#[derive(Serialize, Deserialize)]
struct Payload {
    base_url: String,
    device_id: String,
    credentials: Credentials,
}

fn same_address(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

#[derive(Clone)]
pub struct SessionFile {
    path: PathBuf,
    protector: Arc<dyn Protector>,
}

impl SessionFile {
    pub fn new(path: impl Into<PathBuf>, protector: Arc<dyn Protector>) -> Self {
        Self { path: path.into(), protector }
    }

    pub fn save(&self, base_url: &str, credentials: &Credentials) -> Result<(), SessionFileError> {
        let payload = Payload {
            base_url: base_url.trim_end_matches('/').to_owned(),
            device_id: credentials.device_id.clone(),
            credentials: credentials.clone(),
        };
        let plain = serde_json::to_vec(&payload).map_err(|_| SessionFileError::Corrupt)?;
        let sealed = self.protector.protect(&plain)?;
        let envelope = Envelope {
            format: FORMAT_VERSION,
            data: base64::engine::general_purpose::STANDARD.encode(sealed),
        };
        let text = serde_json::to_vec(&envelope).map_err(|_| SessionFileError::Corrupt)?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(io_error)?;
        }
        // Write beside the target and rename so a crash never leaves a half-written session.
        let temp = self.path.with_extension("tmp");
        std::fs::write(&temp, &text).map_err(io_error)?;
        std::fs::rename(&temp, &self.path).map_err(|error| {
            let _ = std::fs::remove_file(&temp);
            io_error(error)
        })
    }

    pub fn load(&self, base_url: &str, device: &str) -> Result<Credentials, SessionFileError> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err(SessionFileError::Missing),
            Err(error) => return Err(io_error(error)),
        };
        let envelope: Envelope = serde_json::from_slice(&bytes).map_err(|_| SessionFileError::Corrupt)?;
        if envelope.format != FORMAT_VERSION {
            return Err(SessionFileError::Corrupt);
        }
        let sealed = base64::engine::general_purpose::STANDARD
            .decode(envelope.data)
            .map_err(|_| SessionFileError::Corrupt)?;
        let plain = self.protector.unprotect(&sealed).map_err(|_| SessionFileError::Corrupt)?;
        let payload: Payload = serde_json::from_slice(&plain).map_err(|_| SessionFileError::Corrupt)?;
        payload.credentials.validate().map_err(|_| SessionFileError::Corrupt)?;
        if payload.credentials.device_id != payload.device_id {
            return Err(SessionFileError::Corrupt);
        }
        if !same_address(&payload.base_url, base_url) || payload.device_id != device {
            return Err(SessionFileError::Mismatch);
        }
        Ok(payload.credentials)
    }

    pub fn clear(&self) -> Result<(), SessionFileError> {
        Self::clear_path(&self.path)
    }

    /// Remove a saved session without needing a protector (nothing is decrypted to delete it).
    pub fn clear_path(path: &Path) -> Result<(), SessionFileError> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error(error)),
        }
    }
}

fn io_error(error: std::io::Error) -> SessionFileError {
    SessionFileError::Io(error.to_string())
}

/// SDK session store: the live credentials in memory, mirrored to the sealed file when one is bound.
///
/// A failed write never breaks the running session; it is remembered so the caller can say the login was not
/// persisted, and the stale file is removed so an older account can never be restored by mistake. The SDK clears
/// the store when the server rejects the credentials, which removes the file as well.
pub struct PersistingStore {
    memory: Mutex<Option<Credentials>>,
    file: Option<(SessionFile, String)>,
    failure: Mutex<Option<String>>,
}

impl PersistingStore {
    pub fn memory_only() -> Self {
        Self { memory: Mutex::new(None), file: None, failure: Mutex::new(None) }
    }

    pub fn with_file(file: SessionFile, base_url: &str) -> Self {
        Self { memory: Mutex::new(None), file: Some((file, base_url.to_owned())), failure: Mutex::new(None) }
    }

    /// Put restored credentials in memory without rewriting the file.
    pub fn seed(&self, credentials: Credentials) {
        if let Ok(mut memory) = self.memory.lock() {
            *memory = Some(credentials);
        }
    }

    pub fn persists(&self) -> bool {
        self.file.is_some()
    }

    pub fn take_failure(&self) -> Option<String> {
        self.failure.lock().ok().and_then(|mut failure| failure.take())
    }
}

impl SessionStore for PersistingStore {
    fn load(&self) -> Result<Option<Credentials>, Code> {
        self.memory.lock().map(|memory| memory.clone()).map_err(|_| Code::Unavailable)
    }

    fn save(&self, credentials: &Credentials) -> Result<(), Code> {
        self.memory
            .lock()
            .map(|mut memory| *memory = Some(credentials.clone()))
            .map_err(|_| Code::Unavailable)?;
        if let Some((file, base_url)) = &self.file {
            if let Err(error) = file.save(base_url, credentials) {
                let _ = file.clear();
                if let Ok(mut failure) = self.failure.lock() {
                    *failure = Some(error.to_string());
                }
            }
        }
        Ok(())
    }

    fn clear(&self) -> Result<(), Code> {
        self.memory
            .lock()
            .map(|mut memory| *memory = None)
            .map_err(|_| Code::Unavailable)?;
        match &self.file {
            Some((file, _)) => file.clear().map_err(|_| Code::Unavailable),
            None => Ok(()),
        }
    }
}

#[cfg(windows)]
mod dpapi {
    use super::{Protector, SessionFileError, ENTROPY};
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    /// Windows DPAPI scoped to the current user (no machine scope, no UI prompts).
    pub struct Dpapi;

    fn blob(bytes: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB { cbData: bytes.len() as u32, pbData: bytes.as_ptr() as *mut u8 }
    }

    fn take(output: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        // SAFETY: DPAPI allocated `cbData` bytes at `pbData` with LocalAlloc; they are copied out and then freed.
        unsafe {
            let bytes = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
            LocalFree(output.pbData as _);
            bytes
        }
    }

    impl Protector for Dpapi {
        fn protect(&self, plain: &[u8]) -> Result<Vec<u8>, SessionFileError> {
            let (input, entropy) = (blob(plain), blob(ENTROPY));
            let mut output = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
            // SAFETY: all pointers refer to live locals for the duration of the call.
            let ok = unsafe {
                CryptProtectData(
                    &input,
                    std::ptr::null(),
                    &entropy,
                    std::ptr::null(),
                    std::ptr::null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output,
                )
            };
            if ok == 0 {
                return Err(SessionFileError::Io("Windows DPAPI encryption failed".into()));
            }
            Ok(take(output))
        }

        fn unprotect(&self, sealed: &[u8]) -> Result<Vec<u8>, SessionFileError> {
            let (input, entropy) = (blob(sealed), blob(ENTROPY));
            let mut output = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
            // SAFETY: as above; the description out-pointer is not requested.
            let ok = unsafe {
                CryptUnprotectData(
                    &input,
                    std::ptr::null_mut(),
                    &entropy,
                    std::ptr::null(),
                    std::ptr::null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output,
                )
            };
            if ok == 0 {
                return Err(SessionFileError::Corrupt);
            }
            Ok(take(output))
        }
    }
}
