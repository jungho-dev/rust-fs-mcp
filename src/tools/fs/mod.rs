//! mod.rs
//! tools::fs::mod
//!
//! Collection of file / directory / metadata / image / HTTP read handlers.
//! Exposes the two edit variants file-edit (exact block replace) and file-edit-lines (1-based line range replace) together.
//! Submodules split by tool family: read, lines, write, dir, path_ops, edit, shared.
//!

mod dir;
mod edit;
mod lines;
mod path_ops;
mod read;
mod shared;
mod write;

pub use dir::{handle_dir_create, handle_dir_list};
pub use edit::{handle_file_edit, handle_file_edit_lines};
pub use lines::handle_file_read_line_range;
pub use path_ops::{handle_path_copy, handle_path_move, handle_path_remove, handle_path_stat};
pub use read::handle_file_read;
pub use write::handle_file_write;

// 1. Test fixtures ------------------------------------------------------------
#[cfg(test)]
fn make_temp_dir(prefix: &str) -> std::path::PathBuf {
    let dir = std::env::current_dir()
        .unwrap()
        .join("target")
        .join(format!(
            "{prefix}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
#[cfg(test)]
mod tests {
    use super::edit::edit_item;
    use super::lines::lines_item;
    use super::make_temp_dir;
    use super::read::read_item;
    use serde_json::json;

    #[test]
    fn euc_kr_file_reads_and_edits_keep_its_encoding() {
        let dir = make_temp_dir("rust-fs-mcp-euckr");
        let path = dir.join("legacy.txt");
        // "한글 line1\nline2 한글\n" in EUC-KR.
        let bytes = [
            &[0xC7, 0xD1, 0xB1, 0xDB][..],
            b" line1\nline2 ",
            &[0xC7, 0xD1, 0xB1, 0xDB],
            b"\n",
        ]
        .concat();
        std::fs::write(&path, &bytes).unwrap();

        let read = read_item(&json!({ "path": path.display().to_string() }), false);
        assert!(
            read.content[0]["text"]
                .as_str()
                .unwrap()
                .ends_with("한글 line1\nline2 한글\n"),
            "{read:?}"
        );
        assert_eq!(read.structured.as_ref().unwrap()["encoding"], "EUC-KR");

        let ranged = lines_item(
            &json!({ "path": path.display().to_string(), "start_line": 2, "line_count": 1 }),
            false,
        );
        assert!(!ranged.is_error, "{ranged:?}");
        assert!(
            ranged.content[0]["text"]
                .as_str()
                .unwrap()
                .ends_with("2: line2 한글"),
            "{ranged:?}"
        );

        let edited = edit_item(&json!({
            "file_path": path.display().to_string(),
            "old_string": "line2 한글",
            "new_string": "둘째 줄"
        }));
        assert!(!edited.is_error, "{edited:?}");
        let (expected, _, unmappable) = encoding_rs::EUC_KR.encode("한글 line1\n둘째 줄\n");
        assert!(!unmappable);
        assert_eq!(std::fs::read(&path).unwrap(), expected.as_ref());
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn utf16_bom_file_reads_as_text_not_binary() {
        let dir = make_temp_dir("rust-fs-mcp-utf16");
        let path = dir.join("wide.txt");
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "가나\nab\n".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        std::fs::write(&path, &bytes).unwrap();
        let read = read_item(&json!({ "path": path.display().to_string() }), false);
        assert!(
            read.content[0]["text"]
                .as_str()
                .unwrap()
                .ends_with("가나\nab\n"),
            "{read:?}"
        );
        let ranged = lines_item(
            &json!({ "path": path.display().to_string(), "start_line": 2 }),
            false,
        );
        assert!(
            ranged.content[0]["text"]
                .as_str()
                .unwrap()
                .ends_with("2: ab"),
            "{ranged:?}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
