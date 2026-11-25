//! encoding.rs
//! core::text::encoding
//!
//! Text decoding for non-UTF-8 sources (UTF-16 with BOM, EUC-KR/CP949 fallback) plus the matching
//! re-encoding used by edits, so a legacy file keeps its original byte encoding after a change.
//!

use encoding_rs::{EUC_KR, Encoding, UTF_8, UTF_16BE, UTF_16LE};
use std::borrow::Cow;

// 1. Decoded text -------------------------------------------------------------
pub struct DecodedText {
    pub text: String,
    pub encoding: &'static Encoding,
    pub bom: bool,
    pub lossy: bool,
}
impl DecodedText {
    // UTF-8 무표기 (기존 응답 형태 유지)
    pub fn legacy_label(&self) -> Option<&'static str> {
        (self.encoding != UTF_8).then(|| self.encoding.name())
    }
}
// 2. Whole-buffer decode ------------------------------------------------------
// 유효 UTF-8 무변환 통과
pub fn decode_bytes(bytes: Vec<u8>) -> DecodedText {
    if let Some((encoding, bom_len)) = Encoding::for_bom(&bytes)
        && encoding != UTF_8
    {
        let (text, had_errors) = encoding.decode_without_bom_handling(&bytes[bom_len..]);
        return DecodedText {
            text: text.into_owned(),
            encoding,
            bom: true,
            lossy: had_errors,
        };
    }
    // BOM 없는 UTF-16 (NUL 포함 UTF-8 판정보다 우선)
    if let Some((encoding, text)) = sniff_utf16(&bytes) {
        return DecodedText {
            text,
            encoding,
            bom: false,
            lossy: false,
        };
    }
    match String::from_utf8(bytes) {
        Ok(text) => DecodedText {
            text,
            encoding: UTF_8,
            bom: false,
            lossy: false,
        },
        Err(error) => {
            let bytes = error.into_bytes();
            if let Some(text) = EUC_KR.decode_without_bom_handling_and_without_replacement(&bytes)
                && looks_korean(&text)
            {
                return DecodedText {
                    text: text.into_owned(),
                    encoding: EUC_KR,
                    bom: false,
                    lossy: false,
                };
            }
            DecodedText {
                text: String::from_utf8_lossy(&bytes).into_owned(),
                encoding: UTF_8,
                bom: false,
                lossy: true,
            }
        }
    }
}
pub fn has_utf16_bom(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF])
}
// BOM 또는 무BOM 스니프
pub fn looks_utf16(bytes: &[u8]) -> bool {
    has_utf16_bom(bytes) || sniff_utf16(bytes).is_some()
}
// 잘린 앞부분 표본용: 끝의 홀수 바이트·외톨이 상위 서로게이트를 떼고도 판정
pub fn looks_utf16_prefix(bytes: &[u8]) -> bool {
    let even = bytes.len() & !1;
    looks_utf16(bytes)
        || looks_utf16(&bytes[..even])
        || (even >= 4 && looks_utf16(&bytes[..even - 2]))
}
// 3. Single-line decode -------------------------------------------------------
// EUC-KR 디코딩 여부 동반
pub fn decode_line(bytes: &[u8]) -> (Cow<'_, str>, bool) {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return (Cow::Borrowed(text), false);
    }
    if let Some(text) = EUC_KR.decode_without_bom_handling_and_without_replacement(bytes)
        && looks_korean(&text)
    {
        return (Cow::Owned(text.into_owned()), true);
    }
    (String::from_utf8_lossy(bytes), false)
}
// CP1252 등 오탐 방지: 비ASCII 문자의 절반 이상이 한글일 때만 EUC-KR 채택
fn looks_korean(text: &str) -> bool {
    let mut non_ascii = 0usize;
    let mut hangul = 0usize;
    for ch in text.chars().filter(|ch| !ch.is_ascii()) {
        non_ascii += 1;
        if matches!(ch, '\u{AC00}'..='\u{D7A3}' | '\u{1100}'..='\u{11FF}' | '\u{3130}'..='\u{318F}')
        {
            hangul += 1;
        }
    }
    hangul > 0 && hangul * 2 >= non_ascii
}
// 0x00 위치 다수결(75%) 후 전체 디코드, NUL·제어 문자 없을 때만 채택
// (U+xx00 한글 음절은 반대쪽 자리에 0x00 을 두므로 90% 는 실제 UTF-16 을 놓침)
fn sniff_utf16(bytes: &[u8]) -> Option<(&'static Encoding, String)> {
    if bytes.len() < 2 || !bytes.len().is_multiple_of(2) {
        return None;
    }
    let mut even_zeros = 0usize;
    let mut odd_zeros = 0usize;
    for position in memchr::memchr_iter(0, bytes) {
        if position % 2 == 0 {
            even_zeros += 1;
        } else {
            odd_zeros += 1;
        }
    }
    let total_zeros = even_zeros + odd_zeros;
    // NUL 이 섞인 유효 UTF-8 ("hello\0") 은 0x00 이 40% 이상일 때만 후보 (ASCII 위주 UTF-16 은 약 50%)
    if total_zeros * 5 < bytes.len() * 2 && std::str::from_utf8(bytes).is_ok() {
        return None;
    }
    let encoding = if total_zeros == 0 {
        return None;
    } else if even_zeros * 4 >= total_zeros * 3 {
        UTF_16BE
    } else if odd_zeros * 4 >= total_zeros * 3 {
        UTF_16LE
    } else {
        return None;
    };
    let (text, had_errors) = encoding.decode_without_bom_handling(bytes);
    if had_errors {
        return None;
    }
    let has_control = text
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\t' | '\n' | '\r' | '\x0C'));
    if has_control {
        return None;
    }
    Some((encoding, text.into_owned()))
}
// 4. Re-encode for writes -----------------------------------------------------
// UTF-16 직접 직렬화 (라이브러리 미지원)
pub fn encode_text<'a>(
    text: &'a str,
    encoding: &'static Encoding,
    bom: bool,
) -> Result<Cow<'a, [u8]>, String> {
    if encoding == UTF_8 {
        return Ok(Cow::Borrowed(text.as_bytes()));
    }
    if encoding == UTF_16LE || encoding == UTF_16BE {
        let little = encoding == UTF_16LE;
        let mut out = Vec::with_capacity(text.len() * 2 + 2);
        if bom {
            out.extend_from_slice(if little { &[0xFF, 0xFE] } else { &[0xFE, 0xFF] });
        }
        for unit in text.encode_utf16() {
            out.extend_from_slice(&if little {
                unit.to_le_bytes()
            } else {
                unit.to_be_bytes()
            });
        }
        return Ok(Cow::Owned(out));
    }
    let (bytes, _, unmappable) = encoding.encode(text);
    if unmappable {
        return Err(format!(
            "text contains characters that {} cannot represent",
            encoding.name()
        ));
    }
    Ok(Cow::Owned(bytes.into_owned()))
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_euc_kr_when_utf8_is_invalid() {
        let decoded = decode_bytes(vec![0xC7, 0xD1, 0xB1, 0xDB, b'\n']);
        assert_eq!(decoded.text, "한글\n");
        assert_eq!(decoded.legacy_label(), Some("EUC-KR"));
        assert!(!decoded.lossy);
    }
    #[test]
    fn keeps_utf8_untouched() {
        let decoded = decode_bytes("한글 utf8".as_bytes().to_vec());
        assert_eq!(decoded.text, "한글 utf8");
        assert_eq!(decoded.legacy_label(), None);
    }
    #[test]
    fn decodes_utf16_bom_and_round_trips() {
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "가a\n".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let decoded = decode_bytes(bytes.clone());
        assert_eq!(decoded.text, "가a\n");
        assert!(decoded.bom);
        let encoded = encode_text(&decoded.text, decoded.encoding, decoded.bom).unwrap();
        assert_eq!(encoded.as_ref(), bytes.as_slice());
    }
    #[test]
    fn sniffs_bomless_utf16_with_hangul_u_xx00() {
        for big_endian in [false, true] {
            let bytes: Vec<u8> = "가a\r\nb\r\n"
                .encode_utf16()
                .flat_map(|unit| {
                    if big_endian {
                        unit.to_be_bytes()
                    } else {
                        unit.to_le_bytes()
                    }
                })
                .collect();
            assert!(looks_utf16(&bytes), "big_endian={big_endian}");
            let decoded = decode_bytes(bytes.clone());
            assert_eq!(decoded.text, "가a\r\nb\r\n");
            assert!(!decoded.bom);
            let name = if big_endian { "UTF-16BE" } else { "UTF-16LE" };
            assert_eq!(decoded.encoding.name(), name);
            let encoded = encode_text(&decoded.text, decoded.encoding, decoded.bom).unwrap();
            assert_eq!(encoded.as_ref(), bytes.as_slice());
        }
    }
    #[test]
    fn binary_with_nul_is_not_utf16() {
        assert!(!looks_utf16(b"ab\0cd"));
        // NUL 이 섞인 유효 UTF-8 은 UTF-16 으로 읽지 않는다
        assert!(!looks_utf16(b"hello\0"));
        assert!(!looks_utf16(b"foo\0bar\0"));
        assert_eq!(decode_bytes(b"foo\0bar\0".to_vec()).encoding, UTF_8);
        let png_like = [
            0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, b'I', b'H',
            b'D', b'R',
        ];
        assert!(!looks_utf16(&png_like));
    }
    #[test]
    fn euc_kr_round_trip_and_unmappable_rejection() {
        let encoded = encode_text("한글", EUC_KR, false).unwrap();
        assert_eq!(encoded.as_ref(), &[0xC7, 0xD1, 0xB1, 0xDB]);
        assert!(encode_text("🙂", EUC_KR, false).is_err());
    }
    #[test]
    fn western_bytes_valid_in_uhc_are_not_reported_as_korean() {
        // UHC 유효 서구권 바이트 오탐 방지
        let decoded = decode_bytes(vec![b'c', b'a', b'f', 0xE9, 0xE9]);
        assert_eq!(decoded.legacy_label(), None);
        assert!(decoded.lossy);
    }
    #[test]
    fn decode_line_flags_legacy_lines() {
        let (text, legacy) = decode_line(&[0xC7, 0xD1]);
        assert_eq!(text, "한");
        assert!(legacy);
        let (text, legacy) = decode_line(b"plain");
        assert_eq!(text, "plain");
        assert!(!legacy);
    }
}
