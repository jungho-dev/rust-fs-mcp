use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor_dir = manifest_dir.join("vendor").join("tools");
    println!("cargo:rerun-if-changed={}", vendor_dir.display());

    if !vendor_dir.exists() {
        return;
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let profile = env::var("PROFILE").unwrap();
    let target_dir = profile_target_dir(&out_dir, &profile);
    let tools_dir = target_dir.join("tools");

    copy_tree(&vendor_dir, &tools_dir);
}

fn profile_target_dir(out_dir: &Path, profile: &str) -> PathBuf {
    let mut current = out_dir.to_path_buf();
    while current.file_name().and_then(|name| name.to_str()) != Some(profile) {
        if !current.pop() {
            return out_dir.to_path_buf();
        }
    }

    current
}

fn copy_tree(source: &Path, target: &Path) {
    let Ok(entries) = fs::read_dir(source) else {
        return;
    };

    fs::create_dir_all(target).unwrap();
    for entry in entries.flatten() {
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        if source_path.is_dir() {
            copy_tree(&source_path, &target_path);
        } else {
            println!("cargo:rerun-if-changed={}", source_path.display());
            fs::copy(&source_path, &target_path).unwrap();
        }
    }
}
