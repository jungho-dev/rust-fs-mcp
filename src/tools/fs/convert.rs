//! convert.rs
//! tools::fs::convert
//!
//! Converts files a read cannot return as-is: PDF text through pdftotext, non-web images to WebP through magick.
//! Each path is one argv item with no shell; the magick coder prefix comes from a fixed list.
//!

use crate::core::process::external::{ExternalTool, run_external_bytes};
use std::path::Path;
use std::time::Duration;

const CONVERT_TIMEOUT: Duration = Duration::from_secs(60);
const PAGES_FORMAT_ERROR: &str = "pages must be \"N\" or \"N-M\" (1-based), e.g. \"1-5\"";

// PDF 쪽별 텍스트
pub(crate) fn pdf_text(path: &Path, pages: Option<(u32, u32)>) -> Result<String, String> {
    // 1. pdftotext 실행
    let mut args = vec![
        "-enc".to_string(),
        "UTF-8".to_string(),
        "-layout".to_string(),
    ];
    if let Some((first, last)) = pages {
        args.extend([
            "-f".to_string(),
            first.to_string(),
            "-l".to_string(),
            last.to_string(),
        ]);
    }
    args.push(path.to_string_lossy().into_owned());
    args.push("-".to_string());
    let output = run_external_bytes(ExternalTool::PdfToText, &args, None, CONVERT_TIMEOUT)?;
    if output.stdout_capped || output.status_code != Some(0) {
        return Err(format!(
            "pdftotext failed for {} (exit {:?}, output capped {}): {}",
            path.display(),
            output.status_code,
            output.stdout_capped,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    // 2. 텍스트 없는 PDF
    let text = String::from_utf8(output.stdout)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned());
    if text.trim().is_empty() {
        return Ok("(no extractable text: scanned or image-only pages)".to_string());
    }

    // 3. 폼피드 쪽 구분, 쪽 번호 머리줄
    let mut page_texts: Vec<&str> = text.split('\x0C').collect();
    if page_texts.last().is_some_and(|last| last.is_empty()) {
        page_texts.pop();
    }
    let first_page = pages.map_or(1, |(first, _)| first);
    let page_blocks: Vec<String> = (first_page..)
        .zip(page_texts)
        .map(|(page_number, page_text)| {
            format!(
                "--- page {page_number} ---\n{}",
                page_text.trim_end_matches(['\r', '\n'])
            )
        })
        .collect();
    Ok(page_blocks.join("\n"))
}
// pages 인자 해석
pub(crate) fn parse_pages(text: &str) -> Result<(u32, u32), String> {
    let (first_text, last_text) = text.split_once('-').unwrap_or((text, text));
    let digits_only = !first_text.is_empty()
        && !last_text.is_empty()
        && first_text
            .bytes()
            .chain(last_text.bytes())
            .all(|byte| byte.is_ascii_digit());
    let (true, Ok(first), Ok(last)) = (
        digits_only,
        first_text.parse::<u32>(),
        last_text.parse::<u32>(),
    ) else {
        return Err(PAGES_FORMAT_ERROR.to_string());
    };
    if first == 0 || last < first {
        return Err(PAGES_FORMAT_ERROR.to_string());
    }
    Ok((first, last))
}
// 비웹 이미지의 WebP 변환
pub(crate) fn image_to_webp(path: &Path, coder: &str) -> Result<Vec<u8>, String> {
    // 1. 고정 디코더 목록
    if !matches!(
        coder,
        "avif" | "heic" | "heif" | "bmp" | "tiff" | "ico" | "jxl" | "png" | "jpeg" | "gif" | "webp"
    ) {
        return Err(format!("unsupported image type: {coder}"));
    }

    // 2. 무손실 변환, 상한 초과 시 손실 재변환
    let source = format!("{coder}:{}[0]", path.display());
    let mut webp: Vec<u8> = Vec::new();
    for quality_args in [["-define", "webp:lossless=true"], ["-quality", "90"]] {
        let args: Vec<String> = [source.as_str(), "-auto-orient", "-resize", "2000x2000>"]
            .into_iter()
            .chain(quality_args)
            .chain(["webp:-"])
            .map(str::to_string)
            .collect();
        let output = run_external_bytes(ExternalTool::Magick, &args, None, CONVERT_TIMEOUT)?;
        if output.stdout_capped || output.status_code != Some(0) {
            return Err(format!(
                "magick failed for {} (exit {:?}, output capped {}): {}",
                path.display(),
                output.status_code,
                output.stdout_capped,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        webp = output.stdout;
        if webp.len() <= 3_500_000 {
            break;
        }
    }

    // 3. WebP 서명 확인
    if !webp.starts_with(b"RIFF") {
        return Err(format!(
            "magick did not produce WebP for {}",
            path.display()
        ));
    }
    Ok(webp)
}
#[cfg(test)]
mod tests {
    use super::super::make_temp_dir;
    use super::*;
    use std::process::Command;

    // 두 쪽 텍스트 PDF (PDF 1.4 리터럴)
    fn write_two_page_pdf(path: &Path) {
        let mut objects = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".to_string(),
        ];
        for contents in [5, 6] {
            objects.push(format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 200] /Resources << /Font << /F1 7 0 R >> >> /Contents {contents} 0 R >>"
            ));
        }
        for label in ["First page", "Second page"] {
            let stream = format!("BT /F1 18 Tf 40 100 Td ({label}) Tj ET");
            objects.push(format!(
                "<< /Length {} >>\nstream\n{stream}\nendstream",
                stream.len()
            ));
        }
        objects.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string());
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (number, body) in (1..).zip(&objects) {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{number} 0 obj\n{body}\nendobj\n").as_bytes());
        }
        let xref_offset = pdf.len();
        let entries: String = offsets
            .iter()
            .map(|offset| format!("{offset:010} 00000 n \n"))
            .collect();
        let size = objects.len() + 1;
        pdf.extend_from_slice(
            format!(
                "xref\n0 {size}\n0000000000 65535 f \n{entries}trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n"
            )
            .as_bytes(),
        );
        std::fs::write(path, pdf).unwrap();
    }

    #[test]
    fn page_ranges_parse_single_and_span() {
        assert_eq!(parse_pages("2"), Ok((2, 2)));
        assert_eq!(parse_pages("1-5"), Ok((1, 5)));
        assert_eq!(parse_pages("3-3"), Ok((3, 3)));
        for invalid in ["0", "5-2", "a", ""] {
            assert_eq!(
                parse_pages(invalid),
                Err(PAGES_FORMAT_ERROR.to_string()),
                "{invalid}"
            );
        }
    }
    #[test]
    fn avif_converts_to_webp() {
        let dir = make_temp_dir("rust-fs-mcp-convert-avif");
        let png = dir.join("source.png");
        let avif = dir.join("시험.avif");
        // magick 미설치 시 생략
        let made = Command::new("magick")
            .args(["-size", "4x4", "xc:red"])
            .arg(&png)
            .status()
            .is_ok_and(|status| status.success())
            && Command::new("magick")
                .arg(&png)
                .arg(&avif)
                .status()
                .is_ok_and(|status| status.success());
        if !made {
            eprintln!("skip avif_converts_to_webp: magick not available");
            std::fs::remove_dir_all(&dir).ok();
            return;
        }
        let webp = image_to_webp(&avif, "avif");
        std::fs::remove_dir_all(&dir).ok();
        let webp = webp.unwrap();
        assert!(
            webp.starts_with(b"RIFF"),
            "{:?}",
            &webp[..webp.len().min(16)]
        );
        assert_eq!(
            image_to_webp(&avif, "svg"),
            Err("unsupported image type: svg".to_string())
        );
    }
    #[test]
    fn pdf_page_range_extracts_only_that_page() {
        let dir = make_temp_dir("rust-fs-mcp-convert-pdf");
        let pdf = dir.join("two.pdf");
        write_two_page_pdf(&pdf);
        let extracted = pdf_text(&pdf, Some((2, 2)));
        std::fs::remove_dir_all(&dir).ok();
        // pdftotext 미설치 시 생략
        if let Err(error) = &extracted
            && error.starts_with("pdftotext not found")
        {
            eprintln!("skip pdf_page_range_extracts_only_that_page: {error}");
            return;
        }
        let text = extracted.unwrap();
        assert!(text.contains("--- page 2 ---"), "{text}");
        assert!(text.contains("Second page"), "{text}");
        assert!(!text.contains("First page"), "{text}");
        assert!(!text.contains("--- page 1 ---"), "{text}");
    }
}
