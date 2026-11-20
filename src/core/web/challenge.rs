//! challenge.rs
//! core::web::challenge
//!
//! Bot-protection detection on a fetched response: names the vendor gate (Cloudflare, Vercel,
//! AWS WAF, DataDome, Akamai, generic JS check) from response headers and small challenge bodies.
//! A detected gate cannot be passed by a non-browser client, so callers switch to fallback sources.
//!

use ureq::http::HeaderMap;

pub const CLOUDFLARE_CHALLENGE: &str = "Cloudflare JS challenge";
pub const CLOUDFLARE_RATE_LIMIT: &str = "Cloudflare rate limit";
pub const VERCEL_CHALLENGE: &str = "Vercel bot challenge";
pub const AWS_WAF_CHALLENGE: &str = "AWS WAF challenge";
pub const DATADOME_BLOCK: &str = "DataDome bot protection";
pub const AKAMAI_BLOCK: &str = "Akamai bot protection";
pub const JS_CHALLENGE: &str = "JavaScript/cookie challenge";
// 본문 표지는 작은 응답에서만 본다 (대형 정상 문서의 우연한 문자열 오탐 방지)
const BODY_SCAN_LIMIT: usize = 256 * 1024;

// 1. Detection ----------------------------------------------------------------
pub fn detect_challenge(status: u16, headers: &HeaderMap, body: &[u8]) -> Option<&'static str> {
    let header_text = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.trim().to_ascii_lowercase())
            .unwrap_or_default()
    };

    // 1) 벤더가 명시한 mitigation 헤더
    if header_text("cf-mitigated") == "challenge" {
        return Some(CLOUDFLARE_CHALLENGE);
    }
    if header_text("x-vercel-mitigated") == "challenge" {
        return Some(VERCEL_CHALLENGE);
    }
    if matches!(
        header_text("x-amzn-waf-action").as_str(),
        "challenge" | "captcha"
    ) {
        return Some(AWS_WAF_CHALLENGE);
    }
    let blocked_status = matches!(status, 403 | 429 | 503);
    if blocked_status && headers.contains_key("x-datadome") {
        return Some(DATADOME_BLOCK);
    }

    // 2) server 헤더 + 차단 상태 + 챌린지 본문 표지
    if body.len() > BODY_SCAN_LIMIT {
        return None;
    }
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    let server = header_text("server");
    // Bot Fight Mode는 정상 페이지에도 /cdn-cgi/challenge-platform/scripts/jsd 를 주입한다.
    // 챌린지 페이지에만 있는 _cf_chl_opt·"Just a moment" 제목만 표지로 쓴다.
    let challenge_title = text.contains("<title>just a moment");
    let challenge_script = text.contains("_cf_chl_opt") || text.contains("/challenge-platform/h/");
    let cloudflare_page = if blocked_status {
        challenge_title || challenge_script
    } else {
        challenge_title && challenge_script
    };
    if cloudflare_page {
        return Some(CLOUDFLARE_CHALLENGE);
    }
    if server.contains("cloudflare") && status == 429 && text.contains("error code: 1015") {
        return Some(CLOUDFLARE_RATE_LIMIT);
    }
    if server.contains("akamaighost") && status == 403 && text.contains("access denied") {
        return Some(AKAMAI_BLOCK);
    }
    if blocked_status && text.contains("enable javascript and cookies to continue") {
        return Some(JS_CHALLENGE);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use ureq::http::HeaderValue;

    fn headers_of(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(*name, HeaderValue::from_static(value));
        }
        headers
    }

    #[test]
    fn detects_vendor_mitigation_headers() {
        for (pairs, expected) in [
            (vec![("cf-mitigated", "challenge")], CLOUDFLARE_CHALLENGE),
            (vec![("x-vercel-mitigated", "challenge")], VERCEL_CHALLENGE),
            (vec![("x-amzn-waf-action", "captcha")], AWS_WAF_CHALLENGE),
            (vec![("x-datadome", "protected")], DATADOME_BLOCK),
        ] {
            assert_eq!(
                detect_challenge(403, &headers_of(&pairs), b""),
                Some(expected)
            );
        }
    }

    #[test]
    fn detects_challenge_bodies() {
        let cloudflare = headers_of(&[("server", "cloudflare")]);
        let page = b"<html><head><title>Just a moment...</title></head><body><script>window._cf_chl_opt={}</script><script src=\"/cdn-cgi/challenge-platform/h/b/orchestrate/chl\"></script></body></html>";
        assert_eq!(
            detect_challenge(403, &cloudflare, page),
            Some(CLOUDFLARE_CHALLENGE)
        );
        // 200 상태로 온 챌린지 본문도 cloudflare server 헤더가 있으면 차단으로 본다
        assert_eq!(
            detect_challenge(200, &cloudflare, page),
            Some(CLOUDFLARE_CHALLENGE)
        );
        assert_eq!(
            detect_challenge(429, &cloudflare, b"error code: 1015"),
            Some(CLOUDFLARE_RATE_LIMIT)
        );
        let akamai = headers_of(&[("server", "AkamaiGHost")]);
        assert_eq!(
            detect_challenge(403, &akamai, b"<H1>Access Denied</H1>"),
            Some(AKAMAI_BLOCK)
        );
        assert_eq!(
            detect_challenge(
                403,
                &HeaderMap::new(),
                b"Enable JavaScript and cookies to continue"
            ),
            Some(JS_CHALLENGE)
        );
    }

    #[test]
    fn ignores_ordinary_responses() {
        let cloudflare = headers_of(&[("server", "cloudflare")]);
        assert_eq!(detect_challenge(200, &cloudflare, b"<html>ok</html>"), None);
        assert_eq!(detect_challenge(404, &cloudflare, b"not found"), None);
        // Bot Fight Mode jsd 주입 스크립트만 있는 정상 200·404 페이지는 챌린지가 아니다
        let injected = b"<html><head><title>Docs</title></head><body>ok<script src=\"/cdn-cgi/challenge-platform/scripts/jsd/main.js\"></script></body></html>";
        assert_eq!(detect_challenge(200, &cloudflare, injected), None);
        assert_eq!(detect_challenge(404, &cloudflare, injected), None);
        // 일반 403 본문은 벤더 판정 없이 상태 코드로만 남긴다
        assert_eq!(detect_challenge(403, &HeaderMap::new(), b"Forbidden"), None);
        // DataDome 헤더는 정상 응답에도 붙으므로 차단 상태일 때만 판정한다
        assert_eq!(
            detect_challenge(200, &headers_of(&[("x-datadome", "protected")]), b"ok"),
            None
        );
    }
}
