//! main.rs
//! main
//!
//! Sole binary entry point that launches the stdio MCP server.
//! Calls protocol::server::run and exits non-zero on fatal startup errors.
//!

fn main() {
    if let Err(error) = rust_fs_mcp::server::run() {
        eprintln!("rust-fs-mcp fatal: {error}");
        std::process::exit(1);
    }
}
