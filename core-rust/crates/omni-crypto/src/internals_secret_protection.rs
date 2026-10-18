use std::sync::{OnceLock, RwLock};

const PLATFORM_SECRET_WRAP_KEY_ENV: &str = "OMNI_PLATFORM_SECRET_WRAP_KEY_HEX";
const PLATFORM_SECRET_WRAP_AAD: &[u8] = b"omni-platform-wrap-v1";

fn platform_wrap_key_override() -> &'static RwLock<Option<[u8; 32]>> {
    static OVERRIDE: OnceLock<RwLock<Option<[u8; 32]>>> = OnceLock::new();
    OVERRIDE.get_or_init(|| RwLock::new(None))
}

pub fn configure_platform_secret_wrap_key_hex(key_hex: &str) -> OmniResult<()> {
    let parsed = parse_platform_wrap_key_hex(key_hex)?;
    let mut guard = platform_wrap_key_override().write().map_err(|_| {
        OmniError::Internal("platform secret key override lock poisoned".to_string())
    })?;
    *guard = Some(parsed);
    Ok(())
}

#[cfg(test)]
pub fn clear_platform_secret_wrap_key_override_for_tests() {
    if let Ok(mut guard) = platform_wrap_key_override().write() {
        *guard = None;
    }
}

fn platform_wrap(bytes: &[u8]) -> OmniResult<Vec<u8>> {
    let key = platform_wrap_key()?;
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|error| OmniError::Internal(format!("platform wrapper key init failed: {error}")))?;
    let nonce = random_bytes(AES_GCM_NONCE_LEN)?;
    let sealed = cipher
        .encrypt(
            AesNonce::from_slice(&nonce),
            AeadPayload {
                msg: bytes,
                aad: PLATFORM_SECRET_WRAP_AAD,
            },
        )
        .map_err(|_| OmniError::Internal("platform secret wrap encryption failed".to_string()))?;
    let mut output = Vec::with_capacity(nonce.len() + sealed.len());
    output.extend_from_slice(&nonce);
    output.extend_from_slice(&sealed);
    Ok(output)
}

fn platform_unwrap(blob: &[u8]) -> OmniResult<Vec<u8>> {
    if blob.len() < AES_GCM_NONCE_LEN + AEAD_TAG_LEN {
        return Err(OmniError::InvalidInput(
            "platform wrapped secret blob is too short".to_string(),
        ));
    }

    let key = platform_wrap_key()?;
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|error| OmniError::Internal(format!("platform wrapper key init failed: {error}")))?;
    let (nonce, ciphertext_with_tag) = blob.split_at(AES_GCM_NONCE_LEN);
    cipher
        .decrypt(
            AesNonce::from_slice(nonce),
            AeadPayload {
                msg: ciphertext_with_tag,
                aad: PLATFORM_SECRET_WRAP_AAD,
            },
        )
        .map_err(|_| OmniError::InvalidInput("platform wrapped secret authentication failed".to_string()))
}

fn platform_wrap_key() -> OmniResult<[u8; 32]> {
    if let Ok(guard) = platform_wrap_key_override().read() {
        if let Some(value) = *guard {
            return Ok(value);
        }
    }

    let raw = std::env::var(PLATFORM_SECRET_WRAP_KEY_ENV).map_err(|_| {
        OmniError::Internal(format!(
            "{PLATFORM_SECRET_WRAP_KEY_ENV} must be set for platform wrapped secret protection"
        ))
    })?;
    parse_platform_wrap_key_hex(&raw)
}

fn parse_platform_wrap_key_hex(raw: &str) -> OmniResult<[u8; 32]> {
    let bytes = hex::decode(raw.trim()).map_err(|error| {
        OmniError::InvalidInput(format!(
            "invalid platform secret wrap key hex in {PLATFORM_SECRET_WRAP_KEY_ENV}: {error}"
        ))
    })?;
    if bytes.len() != 32 {
        return Err(OmniError::InvalidInput(format!(
            "{PLATFORM_SECRET_WRAP_KEY_ENV} must decode to 32 bytes, got {}",
            bytes.len()
        )));
    }

    let mut key = [0u8; 32];
    key.copy_from_slice(&bytes);
    Ok(key)
}

#[cfg(windows)]
fn windows_protect(bytes: &[u8]) -> OmniResult<Vec<u8>> {
    let mut input = DataBlob::from_slice(bytes);
    let mut output = DataBlob::default();

    unsafe {
        let result = crypt_protect_data(
            &mut input,
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
            &mut output,
        );
        if result == 0 {
            return Err(OmniError::Internal(
                "windows dpapi protect failed".to_string(),
            ));
        }
    }

    let protected = output.to_vec();
    output.free();
    Ok(protected)
}

#[cfg(windows)]
fn windows_unprotect(blob: &[u8]) -> OmniResult<Vec<u8>> {
    let mut input = DataBlob::from_slice(blob);
    let mut output = DataBlob::default();

    unsafe {
        let result = crypt_unprotect_data(
            &mut input,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
            &mut output,
        );
        if result == 0 {
            return Err(OmniError::Internal(
                "windows dpapi unprotect failed".to_string(),
            ));
        }
    }

    let plaintext = output.to_vec();
    output.free();
    Ok(plaintext)
}

#[cfg(windows)]
#[repr(C)]
struct DataBlob {
    cb_data: u32,
    pb_data: *mut u8,
}

#[cfg(windows)]
impl DataBlob {
    fn from_slice(slice: &[u8]) -> Self {
        Self {
            cb_data: slice.len() as u32,
            pb_data: slice.as_ptr() as *mut u8,
        }
    }

    fn to_vec(&self) -> Vec<u8> {
        if self.pb_data.is_null() || self.cb_data == 0 {
            return Vec::new();
        }
        unsafe { std::slice::from_raw_parts(self.pb_data, self.cb_data as usize).to_vec() }
    }

    fn free(&mut self) {
        if !self.pb_data.is_null() {
            unsafe {
                local_free(self.pb_data as *mut std::ffi::c_void);
            }
            self.pb_data = std::ptr::null_mut();
            self.cb_data = 0;
        }
    }
}

#[cfg(windows)]
impl Default for DataBlob {
    fn default() -> Self {
        Self {
            cb_data: 0,
            pb_data: std::ptr::null_mut(),
        }
    }
}

#[cfg(windows)]
#[link(name = "Crypt32")]
extern "system" {
    #[link_name = "CryptProtectData"]
    fn crypt_protect_data(
        p_data_in: *mut DataBlob,
        sz_data_descr: *const u16,
        p_optional_entropy: *mut DataBlob,
        pv_reserved: *mut std::ffi::c_void,
        p_prompt_struct: *mut std::ffi::c_void,
        dw_flags: u32,
        p_data_out: *mut DataBlob,
    ) -> i32;

    #[link_name = "CryptUnprotectData"]
    fn crypt_unprotect_data(
        p_data_in: *mut DataBlob,
        ppsz_data_descr: *mut *mut u16,
        p_optional_entropy: *mut DataBlob,
        pv_reserved: *mut std::ffi::c_void,
        p_prompt_struct: *mut std::ffi::c_void,
        dw_flags: u32,
        p_data_out: *mut DataBlob,
    ) -> i32;
}

#[cfg(windows)]
#[link(name = "Kernel32")]
extern "system" {
    #[link_name = "LocalFree"]
    fn local_free(h_mem: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
}

