//! password.rs
//! core::sql::password
//!
//! Resolves the MySQL password from its configured source: an environment variable,
//! a DPAPI-sealed file (Windows, current user), or the plain config value.
//! The value goes to the caller only; nothing here logs or caches it.
//!

use crate::core::settings::PasswordSource;
use std::env;
use std::fs;

// 1. Password resolution ------------------------------------------------------
pub fn resolve_password(source: &PasswordSource) -> Result<String, String> {
    match source {
        PasswordSource::Unset => Ok(String::new()),
        PasswordSource::Plain(text) => Ok(text.clone()),
        PasswordSource::Env(name) => {
            env::var(name).map_err(|_| format!("mysql.password_env variable {name} is not set"))
        }
        PasswordSource::Dpapi(sealed_path) => {
            let sealed = fs::read(sealed_path).map_err(|error| {
                format!(
                    "Failed to read mysql.password_dpapi file {}: {error}",
                    sealed_path.display()
                )
            })?;
            let unsealed = unseal_dpapi(&sealed)?;
            let text = String::from_utf8(unsealed)
                .map_err(|_| "mysql.password_dpapi content is not UTF-8".to_string())?;
            // 편집기 후행 개행 제거
            Ok(text.trim_end_matches(['\r', '\n']).to_string())
        }
    }
}
// 2. DPAPI unseal -------------------------------------------------------------
#[cfg(windows)]
fn unseal_dpapi(sealed: &[u8]) -> Result<Vec<u8>, String> {
    use std::ffi::c_void;
    use std::ptr;

    // DATA_BLOB 배치
    #[repr(C)]
    struct DataBlob {
        len: u32,
        data: *mut u8,
    }
    #[link(name = "crypt32")]
    unsafe extern "system" {
        fn CryptUnprotectData(
            sealed: *const DataBlob,
            description: *mut *mut u16,
            entropy: *const DataBlob,
            reserved: *mut c_void,
            prompt: *const c_void,
            flags: u32,
            unsealed: *mut DataBlob,
        ) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }
    const CRYPTPROTECT_UI_FORBIDDEN: u32 = 0x1;

    let len = u32::try_from(sealed.len())
        .map_err(|_| "mysql.password_dpapi file is too large".to_string())?;
    let input = DataBlob {
        len,
        data: sealed.as_ptr().cast_mut(),
    };
    let mut output = DataBlob {
        len: 0,
        data: ptr::null_mut(),
    };
    // SAFETY: 입력은 호출 중에만 읽기, 출력 버퍼는 API 할당
    let unsealed = unsafe {
        CryptUnprotectData(
            &input,
            ptr::null_mut(),
            ptr::null(),
            ptr::null_mut(),
            ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if unsealed == 0 || output.data.is_null() {
        return Err(format!(
            "Failed to unseal the mysql.password_dpapi file: {} (seal it as the current Windows user)",
            std::io::Error::last_os_error()
        ));
    }
    let size = output.len as usize;
    // SAFETY: API가 돌려준 size 바이트 버퍼, 복사 후 소거와 1회 해제
    let plain = unsafe {
        let plain = std::slice::from_raw_parts(output.data, size).to_vec();
        ptr::write_bytes(output.data, 0, size);
        LocalFree(output.data.cast());
        plain
    };
    Ok(plain)
}
#[cfg(not(windows))]
fn unseal_dpapi(_sealed: &[u8]) -> Result<Vec<u8>, String> {
    Err("mysql.password_dpapi needs Windows DPAPI; use password_env on this platform".to_string())
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_plain_unset_and_missing_env() {
        assert_eq!(resolve_password(&PasswordSource::Unset).unwrap(), "");
        let plain = PasswordSource::Plain("sample pass".to_string());
        assert_eq!(resolve_password(&plain).unwrap(), "sample pass");
        let missing = PasswordSource::Env("RUST_FS_MCP_TEST_UNSET_PASSWORD".to_string());
        let error = resolve_password(&missing).unwrap_err();
        assert!(error.contains("RUST_FS_MCP_TEST_UNSET_PASSWORD"), "{error}");
    }
    #[test]
    fn rejects_bytes_that_are_not_a_sealed_blob() {
        let dir = std::env::temp_dir().join(format!("fs-mcp-dpapi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sealed_path = dir.join("not-sealed.dpapi");
        std::fs::write(&sealed_path, b"plain bytes, not a DPAPI blob").unwrap();
        let error = resolve_password(&PasswordSource::Dpapi(sealed_path)).unwrap_err();
        assert!(error.contains("password_dpapi"), "{error}");
        assert!(!error.contains("plain bytes"), "{error}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn unseals_a_blob_sealed_by_the_current_user() {
        use std::ffi::c_void;
        use std::ptr;

        #[repr(C)]
        struct DataBlob {
            len: u32,
            data: *mut u8,
        }
        #[link(name = "crypt32")]
        unsafe extern "system" {
            fn CryptProtectData(
                plain: *const DataBlob,
                description: *const u16,
                entropy: *const DataBlob,
                reserved: *mut c_void,
                prompt: *const c_void,
                flags: u32,
                sealed: *mut DataBlob,
            ) -> i32;
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn LocalFree(memory: *mut c_void) -> *mut c_void;
        }

        let sample = b"sample-pass \xed\x95\x9c\r\n";
        let input = DataBlob {
            len: sample.len() as u32,
            data: sample.as_ptr().cast_mut(),
        };
        let mut output = DataBlob {
            len: 0,
            data: ptr::null_mut(),
        };
        // SAFETY: 입력은 호출 중에만 읽기, 출력 버퍼는 복사 후 1회 해제
        let sealed = unsafe {
            let sealed_ok = CryptProtectData(
                &input,
                ptr::null(),
                ptr::null(),
                ptr::null_mut(),
                ptr::null(),
                0x1,
                &mut output,
            );
            assert_ne!(sealed_ok, 0, "CryptProtectData failed");
            let sealed = std::slice::from_raw_parts(output.data, output.len as usize).to_vec();
            LocalFree(output.data.cast());
            sealed
        };
        let dir = std::env::temp_dir().join(format!("fs-mcp-dpapi-seal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sealed_path = dir.join("sample.dpapi");
        std::fs::write(&sealed_path, &sealed).unwrap();
        let resolved = resolve_password(&PasswordSource::Dpapi(sealed_path)).unwrap();
        assert_eq!(resolved, "sample-pass \u{d55c}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
