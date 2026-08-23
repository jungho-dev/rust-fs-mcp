//! build.rs
//!
//! gnullvm 타겟은 libunwind.dll을 동적 링크한다(panic = "abort"여도 std가 backtrace/unwind
//! 테이블 지원을 위해 요구함). 이 DLL은 툴체인 bin에만 있고 PATH에는 없어 실행 시
//! "error while loading shared libraries: libunwind.dll"로 죽는다. 빌드마다
//! target/<profile> 옆에 자동 복사해 수동 조치 없이 항상 실행 가능하게 만든다.
//!

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

const DLL_NAME: &str = "libunwind.dll";

fn main() {
  println!("cargo:rerun-if-changed=build.rs");

  let target = env::var("TARGET").unwrap_or_default();
  if !target.contains("gnullvm") {
    return;
  }

  let Some(dll_src) = find_libunwind_dll(&target) else {
    println!("cargo:warning=libunwind.dll을 찾지 못함 (target={target}); 실행 시 로드 실패할 수 있음");
    return;
  };

  let Some(target_dir) = target_dir_from_out_dir() else {
    println!("cargo:warning=OUT_DIR에서 target 디렉터리 경로를 계산하지 못함");
    return;
  };

  let dest = target_dir.join(DLL_NAME);
  match std::fs::copy(&dll_src, &dest) {
    Ok(_) => println!("cargo:warning=libunwind.dll 복사 완료: {}", dest.display()),
    Err(e) => println!("cargo:warning=libunwind.dll 복사 실패 ({e}): {}", dest.display()),
  }
}

// 1. target 디렉터리 계산 -----------------------------------------------------
// OUT_DIR = <target_dir>/<profile>/build/<pkg>-<hash>/out 이므로 3단계 상위가 target_dir.
fn target_dir_from_out_dir() -> Option<PathBuf> {
  let out_dir = PathBuf::from(env::var("OUT_DIR").ok()?);
  out_dir.ancestors().nth(3).map(Path::to_path_buf)
}

// 2. libunwind.dll 탐색 -------------------------------------------------------
fn find_libunwind_dll(target: &str) -> Option<PathBuf> {
  let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
  let output = Command::new(rustc).args(["--print", "sysroot"]).output().ok()?;
  let sysroot = PathBuf::from(String::from_utf8(output.stdout).ok()?.trim().to_string());

  // host 툴체인 bin (host == target인 일반적인 경우)
  let host_bin = sysroot.join("bin").join(DLL_NAME);
  if host_bin.is_file() {
    return Some(host_bin);
  }

  // 크로스 컴파일 시 타겟 전용 rustlib bin
  let target_bin = sysroot.join("lib").join("rustlib").join(target).join("bin").join(DLL_NAME);
  if target_bin.is_file() {
    return Some(target_bin);
  }

  None
}
