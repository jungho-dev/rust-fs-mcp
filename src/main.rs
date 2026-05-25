//! Describe this crate or module.
//!
//! Provide the main responsibilities and usage context.

fn main() {
    if let Err(error) = rust_fs_mcp::server::run() {
        eprintln!("rust-fs-mcp fatal: {error}");
        std::process::exit(1);
    }
}
