//! OS-backed protection for the saved GitHub token.
//!
//! The token file keeps its `{token, encrypted, user}` shape on every
//! platform, and old plaintext files stay readable:
//! - Windows: DPAPI blobs, exactly as before (current-user scoped).
//! - macOS: the token itself lives in the login Keychain; the file only
//!   stores a reference marker, so copying the data directory to another
//!   machine or account leaks nothing.
//! - Other platforms: no protection available; callers fall back to
//!   plaintext (same contract as a failed DPAPI call on Windows).

/// Protect `input` for storage in the token file.  `None` means the caller
/// should store plaintext.
pub fn protect(input: &[u8]) -> Option<Vec<u8>> {
    #[cfg(windows)]
    {
        dpapi(input, true)
    }
    #[cfg(target_os = "macos")]
    {
        keychain_store(input)?;
        Some(KEYCHAIN_MARKER.to_vec())
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = input;
        None
    }
}

/// Reverse [`protect`].  `None` means the stored value cannot be decrypted
/// here (wrong machine/account, or missing Keychain item).
pub fn unprotect(input: &[u8]) -> Option<Vec<u8>> {
    #[cfg(windows)]
    {
        dpapi(input, false)
    }
    #[cfg(target_os = "macos")]
    {
        if input == KEYCHAIN_MARKER {
            keychain_load()
        } else {
            None
        }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = input;
        None
    }
}

/// Drop any OS-stored secret (called when the user disconnects GitHub).
pub fn clear() {
    #[cfg(target_os = "macos")]
    {
        let _ = security_framework::passwords::delete_generic_password(SERVICE, ACCOUNT);
    }
}

#[cfg(windows)]
fn dpapi(input: &[u8], encrypt: bool) -> Option<Vec<u8>> {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CryptProtectData, CryptUnprotectData,
    };
    use windows::core::PCWSTR;

    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(input.len()).ok()?,
        pbData: input.as_ptr().cast_mut(),
    };
    let mut out_blob = CRYPT_INTEGER_BLOB::default();
    let result = unsafe {
        if encrypt {
            CryptProtectData(&in_blob, PCWSTR::null(), None, None, None, 0, &mut out_blob)
        } else {
            CryptUnprotectData(&in_blob, None, None, None, None, 0, &mut out_blob)
        }
    };
    if result.is_err() || out_blob.pbData.is_null() {
        return None;
    }
    let bytes =
        unsafe { std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize) }.to_vec();
    unsafe {
        LocalFree(Some(HLOCAL(out_blob.pbData.cast())));
    }
    Some(bytes)
}

#[cfg(target_os = "macos")]
const KEYCHAIN_MARKER: &[u8] = b"macos-keychain";
#[cfg(target_os = "macos")]
const SERVICE: &str = "dsh-marina";
#[cfg(target_os = "macos")]
const ACCOUNT: &str = "github-token";

#[cfg(target_os = "macos")]
fn keychain_store(input: &[u8]) -> Option<()> {
    // Replace any previous item so repeated saves stay idempotent.
    let _ = security_framework::passwords::delete_generic_password(SERVICE, ACCOUNT);
    security_framework::passwords::set_generic_password(SERVICE, ACCOUNT, input).ok()
}

#[cfg(target_os = "macos")]
fn keychain_load() -> Option<Vec<u8>> {
    security_framework::passwords::get_generic_password(SERVICE, ACCOUNT).ok()
}
