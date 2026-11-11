//! digest.rs
//! core::sql::digest
//!
//! SHA-1 and SHA-256 for the authentication scrambles and RSA-OAEP.
//! They live here because the crate carries no crypto dependency.
//!

// 1. Digests ------------------------------------------------------------------
pub(super) fn digest_sha1(message: &[u8]) -> [u8; 20] {
    // 1. 패딩
    let mut padded = message.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&(message.len() as u64 * 8).to_be_bytes());

    // 2. 블록 압축
    let mut state: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    for block in padded.as_chunks::<64>().0 {
        let mut schedule = [0u32; 80];
        for (word, bytes) in schedule.iter_mut().zip(block.as_chunks::<4>().0) {
            *word = u32::from_be_bytes(*bytes);
        }
        for index in 16..80 {
            schedule[index] = (schedule[index - 3]
                ^ schedule[index - 8]
                ^ schedule[index - 14]
                ^ schedule[index - 16])
                .rotate_left(1);
        }
        let mut work = state;
        for (index, word) in schedule.iter().enumerate() {
            let [a, b, c, d, e] = work;
            let (mixed, constant) = match index / 20 {
                0 => ((b & c) | (!b & d), 0x5A82_7999),
                1 => (b ^ c ^ d, 0x6ED9_EBA1),
                2 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6u32),
            };
            let rotated = a
                .rotate_left(5)
                .wrapping_add(mixed)
                .wrapping_add(e)
                .wrapping_add(constant)
                .wrapping_add(*word);
            work = [rotated, a, b.rotate_left(30), c, d];
        }
        for (total, added) in state.iter_mut().zip(work) {
            *total = total.wrapping_add(added);
        }
    }

    // 3. 빅엔디언 출력
    let mut digest = [0u8; 20];
    for (chunk, word) in digest.as_chunks_mut::<4>().0.iter_mut().zip(state) {
        *chunk = word.to_be_bytes();
    }
    digest
}
pub(super) fn digest_sha256(message: &[u8]) -> [u8; 32] {
    const ROUND_CONSTANTS: [u32; 64] = [
        0x428A2F98, 0x71374491, 0xB5C0FBCF, 0xE9B5DBA5, 0x3956C25B, 0x59F111F1, 0x923F82A4,
        0xAB1C5ED5, 0xD807AA98, 0x12835B01, 0x243185BE, 0x550C7DC3, 0x72BE5D74, 0x80DEB1FE,
        0x9BDC06A7, 0xC19BF174, 0xE49B69C1, 0xEFBE4786, 0x0FC19DC6, 0x240CA1CC, 0x2DE92C6F,
        0x4A7484AA, 0x5CB0A9DC, 0x76F988DA, 0x983E5152, 0xA831C66D, 0xB00327C8, 0xBF597FC7,
        0xC6E00BF3, 0xD5A79147, 0x06CA6351, 0x14292967, 0x27B70A85, 0x2E1B2138, 0x4D2C6DFC,
        0x53380D13, 0x650A7354, 0x766A0ABB, 0x81C2C92E, 0x92722C85, 0xA2BFE8A1, 0xA81A664B,
        0xC24B8B70, 0xC76C51A3, 0xD192E819, 0xD6990624, 0xF40E3585, 0x106AA070, 0x19A4C116,
        0x1E376C08, 0x2748774C, 0x34B0BCB5, 0x391C0CB3, 0x4ED8AA4A, 0x5B9CCA4F, 0x682E6FF3,
        0x748F82EE, 0x78A5636F, 0x84C87814, 0x8CC70208, 0x90BEFFFA, 0xA4506CEB, 0xBEF9A3F7,
        0xC67178F2,
    ];
    // 1. 패딩
    let mut padded = message.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&(message.len() as u64 * 8).to_be_bytes());

    // 2. 블록 압축
    let mut state: [u32; 8] = [
        0x6A09E667, 0xBB67AE85, 0x3C6EF372, 0xA54FF53A, 0x510E527F, 0x9B05688C, 0x1F83D9AB,
        0x5BE0CD19,
    ];
    for block in padded.as_chunks::<64>().0 {
        let mut schedule = [0u32; 64];
        for (word, bytes) in schedule.iter_mut().zip(block.as_chunks::<4>().0) {
            *word = u32::from_be_bytes(*bytes);
        }
        for index in 16..64 {
            let near = schedule[index - 15];
            let far = schedule[index - 2];
            schedule[index] = schedule[index - 16]
                .wrapping_add(near.rotate_right(7) ^ near.rotate_right(18) ^ (near >> 3))
                .wrapping_add(schedule[index - 7])
                .wrapping_add(far.rotate_right(17) ^ far.rotate_right(19) ^ (far >> 10));
        }
        let mut work = state;
        for (word, constant) in schedule.iter().zip(ROUND_CONSTANTS) {
            let [a, b, c, d, e, f, g, h] = work;
            let first = h
                .wrapping_add(e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25))
                .wrapping_add((e & f) ^ (!e & g))
                .wrapping_add(constant)
                .wrapping_add(*word);
            let second = (a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22))
                .wrapping_add((a & b) ^ (a & c) ^ (b & c));
            work = [
                first.wrapping_add(second),
                a,
                b,
                c,
                d.wrapping_add(first),
                e,
                f,
                g,
            ];
        }
        for (total, added) in state.iter_mut().zip(work) {
            *total = total.wrapping_add(added);
        }
    }

    // 3. 빅엔디언 출력
    let mut digest = [0u8; 32];
    for (chunk, word) in digest.as_chunks_mut::<4>().0.iter_mut().zip(state) {
        *chunk = word.to_be_bytes();
    }
    digest
}
#[cfg(test)]
mod tests {
    use super::super::error::MysqlError;
    use super::super::fake_server::{TEST_NONCE, TEST_PASSWORD};
    use super::super::handshake::{NATIVE_PLUGIN, SHA2_PLUGIN, build_auth_response};
    use super::*;

    #[test]
    fn digests_and_scrambles_match_reference_vectors() {
        let hex = |bytes: &[u8]| -> String { bytes.iter().map(|b| format!("{b:02x}")).collect() };
        let long_text = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        assert_eq!(
            hex(&digest_sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            hex(&digest_sha1(b"")),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
        assert_eq!(
            hex(&digest_sha1(long_text)),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        assert_eq!(
            hex(&digest_sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&digest_sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&digest_sha256(long_text)),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );

        // 패딩 경계 길이 (55, 56, 64)
        assert_eq!(
            hex(&digest_sha1(&[b'a'; 55])),
            "c1c8bbdc22796e28c0e15163d20899b65621d65a"
        );
        assert_eq!(
            hex(&digest_sha1(&[b'a'; 56])),
            "c2db330f6083854c99d4b5bfb6e8f29f201be699"
        );
        assert_eq!(
            hex(&digest_sha1(&[b'a'; 64])),
            "0098ba824b5c16427bd7a1122a5a442a25ec644d"
        );
        assert_eq!(
            hex(&digest_sha256(&[b'a'; 55])),
            "9f4390f8d30c2dd92ec9f095b65e2b9ae9b0a925a5258e241c9f1e910f734318"
        );
        assert_eq!(
            hex(&digest_sha256(&[b'a'; 56])),
            "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a"
        );
        assert_eq!(
            hex(&digest_sha256(&[b'a'; 64])),
            "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb"
        );

        // node:crypto 계산 스크램블
        let native = build_auth_response(NATIVE_PLUGIN, TEST_PASSWORD, TEST_NONCE).unwrap();
        assert_eq!(hex(&native), "0663a526a70e98d9737d71015341f258d7060375");
        let sha2 = build_auth_response(SHA2_PLUGIN, TEST_PASSWORD, TEST_NONCE).unwrap();
        assert_eq!(
            hex(&sha2),
            "d7de85d321789ae9572f5d5882bcb3b51af2e67dee28fdbe7a21767ee92e3f67"
        );
        assert!(
            build_auth_response(SHA2_PLUGIN, "", TEST_NONCE)
                .unwrap()
                .is_empty()
        );

        // 미지원 플러그인 및 짧은 난수
        let error =
            build_auth_response("mysql_clear_password", TEST_PASSWORD, TEST_NONCE).unwrap_err();
        assert!(
            matches!(&error, MysqlError::Auth(detail) if detail.contains("mysql_clear_password"))
        );
        assert!(!error.to_string().contains(TEST_PASSWORD));
        assert!(matches!(
            build_auth_response(NATIVE_PLUGIN, TEST_PASSWORD, b"short"),
            Err(MysqlError::Protocol(_))
        ));
    }
}
