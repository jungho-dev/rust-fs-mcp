//! rsa.rs
//! core::sql::rsa
//!
//! RSA-OAEP (SHA-1, MGF1) encryption with the server public key for `caching_sha2_password` full authentication.
//! Covers PEM/DER public-key parsing and big-integer modular exponentiation; no crypto dependency.
//!

use super::digest::digest_sha1;
use super::error::MysqlError;
use super::packet::{take_bytes, take_uint};
use base64::{Engine as _, engine::general_purpose};

// rsaEncryption 1.2.840.113549.1.1.1
const RSA_ENCRYPTION_OID: [u8; 9] = [0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x01];

// 1. RSA-OAEP -----------------------------------------------------------------
// PEM 공개키 해석 (modulus, exponent 순)
pub(super) fn parse_public_key(pem: &[u8]) -> Result<(Vec<u8>, Vec<u8>), MysqlError> {
    const PEM_HEADER: &str = "-----BEGIN PUBLIC KEY-----";
    const PEM_FOOTER: &str = "-----END PUBLIC KEY-----";
    let malformed = || MysqlError::Protocol("malformed server public key".into());

    // 1. PEM 본문 디코딩
    let text = std::str::from_utf8(pem).map_err(|_| malformed())?;
    let body = text
        .split_once(PEM_HEADER)
        .and_then(|(_, after_header)| after_header.split_once(PEM_FOOTER))
        .map(|(body, _)| body)
        .ok_or_else(malformed)?;
    let encoded: String = body
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace())
        .collect();
    let der = general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| malformed())?;

    // 2. SubjectPublicKeyInfo 해석
    let mut outer = der.as_slice();
    let mut key_info = take_der_element(&mut outer, 0x30)?;
    let mut algorithm = take_der_element(&mut key_info, 0x30)?;
    if take_der_element(&mut algorithm, 0x06)? != RSA_ENCRYPTION_OID {
        return Err(MysqlError::Protocol(
            "server public key is not an RSA key".into(),
        ));
    }
    let Some((0, mut key_bits)) = take_der_element(&mut key_info, 0x03)?.split_first() else {
        return Err(malformed());
    };
    let mut rsa_key = take_der_element(&mut key_bits, 0x30)?;

    // 3. 선행 0 제거 및 크기 검증
    let mut numbers = [Vec::new(), Vec::new()];
    for number in &mut numbers {
        let digits = take_der_element(&mut rsa_key, 0x02)?;
        let start = digits
            .iter()
            .position(|byte| *byte != 0)
            .unwrap_or(digits.len());
        *number = digits[start..].to_vec();
    }
    let [modulus, exponent] = numbers;
    if !(128..=1024).contains(&modulus.len()) || exponent.is_empty() || exponent.len() > 8 {
        return Err(MysqlError::Protocol(
            "unsupported server public key size".into(),
        ));
    }
    Ok((modulus, exponent))
}
fn take_der_element<'a>(input: &mut &'a [u8], tag: u8) -> Result<&'a [u8], MysqlError> {
    if take_uint(input, 1)? != u64::from(tag) {
        return Err(MysqlError::Protocol(
            "unexpected element in server public key".into(),
        ));
    }
    let content_len = match take_uint(input, 1)? {
        short @ 0..=0x7F => short,
        // 긴 형식 (빅엔디언 1~4바이트)
        long @ 0x81..=0x84 => take_bytes(input, long - 0x80)?
            .iter()
            .fold(0, |total, byte| (total << 8) | u64::from(*byte)),
        _ => {
            return Err(MysqlError::Protocol(
                "unsupported length in server public key".into(),
            ));
        }
    };
    take_bytes(input, content_len)
}
// RSAES-OAEP SHA-1 (빈 레이블)
pub(super) fn encrypt_oaep(
    message: &[u8],
    modulus: &[u8],
    exponent: &[u8],
    seed: &[u8; 20],
) -> Result<Vec<u8>, MysqlError> {
    // 1. 메시지 길이 상한
    let key_bytes = modulus.len();
    if key_bytes < 42 || message.len() > key_bytes - 42 {
        return Err(MysqlError::Auth(
            "password too long for the server RSA key".into(),
        ));
    }

    // 2. 데이터 블록
    let mut data_block = digest_sha1(&[]).to_vec();
    data_block.resize(key_bytes - 22 - message.len(), 0);
    data_block.push(0x01);
    data_block.extend_from_slice(message);

    // 3. 시드·데이터 블록 상호 마스킹
    let data_mask = expand_mgf1(seed, data_block.len());
    for (byte, mask_byte) in data_block.iter_mut().zip(data_mask) {
        *byte ^= mask_byte;
    }
    let seed_mask = expand_mgf1(&data_block, seed.len());
    let mut encoded = vec![0u8];
    encoded.extend(
        seed.iter()
            .zip(seed_mask)
            .map(|(seed_byte, mask_byte)| seed_byte ^ mask_byte),
    );
    encoded.extend_from_slice(&data_block);

    // 4. RSA 공개키 연산
    Ok(raise_power_mod(&encoded, exponent, modulus))
}
fn expand_mgf1(seed: &[u8], mask_len: usize) -> Vec<u8> {
    let mut mask = Vec::with_capacity(mask_len + 20);
    let mut counter = 0u32;
    while mask.len() < mask_len {
        let mut block = seed.to_vec();
        block.extend_from_slice(&counter.to_be_bytes());
        mask.extend_from_slice(&digest_sha1(&block));
        counter += 1;
    }
    mask.truncate(mask_len);
    mask
}
// 빅엔디언 모듈러 거듭제곱 (base < modulus)
fn raise_power_mod(base: &[u8], exponent: &[u8], modulus: &[u8]) -> Vec<u8> {
    // 1. 리틀엔디언 limb 변환
    let limb_count = modulus.len().div_ceil(8);
    let mut operands = [Vec::new(), Vec::new()];
    for (limbs, bytes) in operands.iter_mut().zip([base, modulus]) {
        *limbs = bytes
            .rchunks(8)
            .map(|chunk| {
                chunk
                    .iter()
                    .fold(0u64, |total, byte| (total << 8) | u64::from(*byte))
            })
            .collect();
        limbs.resize(limb_count, 0);
    }
    let [base_limbs, modulus_limbs] = operands;

    // 2. 제곱-곱 반복
    let mut power = vec![0u64; limb_count];
    if let Some(lowest) = power.first_mut() {
        *lowest = 1;
    }
    for byte in exponent {
        for shift in (0..8).rev() {
            power = multiply_mod(&power, &power, &modulus_limbs);
            if (byte >> shift) & 1 == 1 {
                power = multiply_mod(&power, &base_limbs, &modulus_limbs);
            }
        }
    }

    // 3. 모듈러스 폭 빅엔디언 출력
    let bytes: Vec<u8> = power
        .iter()
        .rev()
        .flat_map(|limb| limb.to_be_bytes())
        .collect();
    bytes[bytes.len() - modulus.len()..].to_vec()
}
// 이진 배가-가산 곱셈 (피연산자 < modulus)
fn multiply_mod(left: &[u64], right: &[u64], modulus: &[u64]) -> Vec<u64> {
    let mut product = vec![0u64; modulus.len()];
    for limb in right.iter().rev() {
        for shift in (0..64).rev() {
            let bit_set = (limb >> shift) & 1 == 1;
            // 1회차 배가, 2회차 left 가산
            for add_left in [false, true] {
                if add_left && !bit_set {
                    continue;
                }
                let mut carry = 0u128;
                for (total, left_limb) in product.iter_mut().zip(left) {
                    let addend = if add_left { *left_limb } else { *total };
                    let sum = u128::from(*total) + u128::from(addend) + carry;
                    *total = sum as u64;
                    carry = sum >> 64;
                }
                // 모듈러스 초과분 1회 감산
                if carry != 0 || product.iter().rev().ge(modulus.iter().rev()) {
                    let mut borrow = false;
                    for (total, modulus_limb) in product.iter_mut().zip(modulus) {
                        let (reduced, under_limb) = total.overflowing_sub(*modulus_limb);
                        let (reduced, under_borrow) = reduced.overflowing_sub(u64::from(borrow));
                        *total = reduced;
                        borrow = under_limb || under_borrow;
                    }
                }
            }
        }
    }
    product
}
#[cfg(test)]
mod tests {
    use super::super::fake_server::{TEST_NONCE, TEST_PASSWORD, TEST_PUBLIC_KEY};
    use super::super::handshake::encrypt_password;
    use super::super::random::fill_random_bytes;
    use super::*;

    #[test]
    fn encrypts_oaep_with_reference_key() {
        let hex = |bytes: &[u8]| -> String { bytes.iter().map(|b| format!("{b:02x}")).collect() };
        let (modulus, exponent) = parse_public_key(TEST_PUBLIC_KEY.as_bytes()).unwrap();
        assert_eq!(modulus.len(), 128);
        assert_eq!(exponent, [0x01, 0x00, 0x01]);

        // node:crypto 복호화 검증 암호문
        let encrypted = encrypt_oaep(b"secret-pw\0", &modulus, &exponent, &[0x5A; 20]).unwrap();
        assert_eq!(
            hex(&encrypted),
            "56057b7014bf559c7157a881341b511da1b290fc201fc77c551d59710df2cf9e\
             6dd32ae897d586f1fcc191eff418412a5b49dac66f8fb65ef97677a14c0ddbd7\
             08c460ee21490de5b6997134431d2732a3502a27df686d94b88002afbcf2815c\
             7d4966f7fe21a1fbafaf2ad83c16ba191b87704d3818d40d77e05780a266a4d4"
        );

        // 길이 초과 및 손상 키
        assert!(matches!(
            encrypt_oaep(&[0; 87], &modulus, &exponent, &[0; 20]),
            Err(MysqlError::Auth(_))
        ));
        let broken_key = b"-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----\n";
        assert!(matches!(
            parse_public_key(broken_key),
            Err(MysqlError::Protocol(_))
        ));
        assert!(parse_public_key(b"not a key").is_err());

        // OS 난수 시드
        let mut seeds = [[0u8; 20]; 2];
        for seed in &mut seeds {
            fill_random_bytes(seed).unwrap();
        }
        assert_ne!(seeds[0], seeds[1]);
        let encrypted =
            encrypt_password(TEST_PASSWORD, TEST_NONCE, TEST_PUBLIC_KEY.as_bytes()).unwrap();
        assert_eq!(encrypted.len(), 128);
    }
}
