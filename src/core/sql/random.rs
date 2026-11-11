//! random.rs
//! core::sql::random
//!
//! OS random source for the RSA-OAEP seed: `BCryptGenRandom` on Windows, `/dev/urandom` elsewhere.
//!

use super::error::MysqlError;
#[cfg(not(windows))]
use std::io::Read;

// 1. OS random source ---------------------------------------------------------
#[cfg(windows)]
#[link(name = "bcrypt", kind = "raw-dylib")]
unsafe extern "system" {
    fn BCryptGenRandom(
        algorithm: *mut std::ffi::c_void,
        buffer: *mut u8,
        buffer_len: u32,
        flags: u32,
    ) -> i32;
}
#[cfg(windows)]
pub(super) fn fill_random_bytes(buffer: &mut [u8]) -> Result<(), MysqlError> {
    const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;
    let Ok(buffer_len) = u32::try_from(buffer.len()) else {
        return Err(MysqlError::Auth("random request too large".into()));
    };
    // SAFETY: buffer_len 바이트 쓰기 가능 버퍼, null 핸들은 시스템 RNG 플래그 전용
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buffer.as_mut_ptr(),
            buffer_len,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status != 0 {
        return Err(MysqlError::Auth(format!(
            "OS random source failed (status {status:#010x})"
        )));
    }
    Ok(())
}
#[cfg(not(windows))]
pub(super) fn fill_random_bytes(buffer: &mut [u8]) -> Result<(), MysqlError> {
    std::fs::File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(buffer))
        .map_err(|error| MysqlError::Auth(format!("OS random source failed: {error}")))
}
