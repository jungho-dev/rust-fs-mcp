//! fallback.rs
//! core::web::fallback
//!
//! Site-independent alternate source for a page a bot challenge blocks: the Wayback Machine raw
//! snapshot. The result is a FetchedPage the caller renders through the normal dump path, labeled
//! with the snapshot date so outdated content is never mistaken for the origin.
//!

use super::http::{FetchOptions, FetchedPage, http_fetch};
use serde_json::Value;
use std::net::{IpAddr, ToSocketAddrs};
use std::time::{Duration, Instant};

// Wayback으로 보내지 않을 query 키 (이름 기준, 대소문자 무시): 전체 일치 / 부분 일치
const SECRET_QUERY_KEYS: [&str; 9] = [
    "sid", "code", "otp", "sig", "key", "pass", "pwd", "state", "auth",
];
const SECRET_QUERY_PARTS: [&str; 12] = [
    "token",
    "secret",
    "password",
    "session",
    "signature",
    "credential",
    "apikey",
    "api_key",
    "access_key",
    "x-amz-",
    "x-goog-",
    "jwt",
];

#[derive(Clone, Debug)]
pub struct AlternatePage {
    pub page: FetchedPage,
    pub via: String,
    pub source_url: String,
}

// 1. Fallback entry -----------------------------------------------------------
// 실패 사유는 한 줄씩 모아 호출자가 원 응답 에러에 덧붙인다.
// 대체 요청 전체가 opts.timeout_ms 하나를 나눠 쓴다.
pub fn fetch_alternate(
    url: &str,
    opts: &FetchOptions,
    allow_private: bool,
) -> Result<AlternatePage, Vec<String>> {
    // 1) 대상 정리: fragment 제거, authority·host 추출
    let target = url.trim();
    let target = target.split('#').next().unwrap_or(target);
    let authority = target.split_once("://").map_or("", |(_, rest)| {
        rest.split(['/', '?']).next().unwrap_or(rest)
    });
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = match host.strip_prefix('[') {
        Some(bracketed) => bracketed.split(']').next().unwrap_or(bracketed),
        None => host.split(':').next().unwrap_or(host),
    };

    // 2) loopback으로 해석되는 대상 제외 (로컬 URL을 외부 서비스로 보내지 않음).
    //    해석 실패도 확인 불가로 보고 건너뛴다.
    let resolves_loopback = match (host, 80).to_socket_addrs() {
        Ok(mut addrs) => addrs.any(|addr| match addr.ip() {
            IpAddr::V6(v6) => {
                v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
            }
            ip => ip.is_loopback(),
        }),
        Err(_) => true,
    };
    if host.is_empty() || resolves_loopback {
        return Err(vec![
            "fallback skipped: loopback or unresolvable host has no public alternate source"
                .to_string(),
        ]);
    }

    // 3) userinfo나 서명·토큰·세션으로 보이는 query 키가 있으면 archive.org로 보내지 않는다
    if authority.contains('@') || query_carries_secret(target) {
        return Err(vec![
            "Wayback Machine: skipped because the URL carries credentials or a token-like query parameter"
                .to_string(),
        ]);
    }
    let deadline = Instant::now().checked_add(Duration::from_millis(opts.timeout_ms));
    fetch_wayback(target, opts, allow_private, deadline).map_err(|error| vec![error])
}

// 2. Wayback Machine ----------------------------------------------------------
// id_ 형식 스냅샷: 아카이브 툴바·링크 재작성 없는 원본 본문.
fn fetch_wayback(
    target: &str,
    opts: &FetchOptions,
    allow_private: bool,
    deadline: Option<Instant>,
) -> Result<AlternatePage, String> {
    // 1) 가장 가까운 200 스냅샷 조회
    let lookup = format!(
        "https://archive.org/wayback/available?url={}",
        percent_encode(target)
    );
    let lookup_page = fetch_budgeted(&lookup, opts, allow_private, deadline)
        .map_err(|error| format!("Wayback Machine: {error}"))?;
    if lookup_page.status >= 400 {
        return Err(format!(
            "Wayback Machine: availability lookup returned HTTP {}",
            lookup_page.status
        ));
    }
    let payload: Value = serde_json::from_str(&lookup_page.body_text())
        .map_err(|error| format!("Wayback Machine: invalid availability JSON: {error}"))?;
    let closest = &payload["archived_snapshots"]["closest"];
    if closest["available"].as_bool() != Some(true) || closest["status"].as_str() != Some("200") {
        return Err("Wayback Machine: no archived snapshot with HTTP 200".to_string());
    }
    let timestamp = closest["timestamp"]
        .as_str()
        .filter(|stamp| stamp.len() >= 8 && stamp.bytes().all(|byte| byte.is_ascii_digit()))
        .ok_or_else(|| "Wayback Machine: snapshot timestamp missing".to_string())?;

    // 2) 원본 스냅샷 본문 (스냅샷 자체가 챌린지 페이지를 저장한 경우 거부)
    let snapshot_url = format!("https://web.archive.org/web/{timestamp}id_/{target}");
    let page = fetch_budgeted(&snapshot_url, opts, allow_private, deadline)
        .map_err(|error| format!("Wayback Machine: {error}"))?;
    if page.status >= 400 || page.challenge.is_some() {
        let label = page
            .challenge
            .map(|label| format!(" ({label})"))
            .unwrap_or_default();
        return Err(format!(
            "Wayback Machine: snapshot {timestamp} returned HTTP {}{label}",
            page.status
        ));
    }
    let date = format!(
        "{}-{}-{}",
        &timestamp[0..4],
        &timestamp[4..6],
        &timestamp[6..8]
    );
    Ok(AlternatePage {
        page: FetchedPage {
            final_url: target.to_string(),
            ..page
        },
        via: format!("Wayback Machine snapshot {date}; content may be outdated"),
        source_url: snapshot_url,
    })
}

// 3. Shared request helpers ---------------------------------------------------
// 공용 deadline의 남은 예산만 timeout으로 넘겨, 요청 수와 무관하게 대체 경로 시간을 묶는다.
fn fetch_budgeted(
    url: &str,
    opts: &FetchOptions,
    allow_private: bool,
    deadline: Option<Instant>,
) -> Result<FetchedPage, String> {
    let Some(deadline) = deadline else {
        return http_fetch(url, opts, allow_private);
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(format!(
            "fallback time budget of {}ms exhausted before {url}",
            opts.timeout_ms
        ));
    }
    let budgeted = FetchOptions {
        timeout_ms: (remaining.as_millis() as u64).max(1),
        ..opts.clone()
    };
    http_fetch(url, &budgeted, allow_private)
}

// 서명·토큰·세션으로 보이는 query 키가 하나라도 있으면 true (값은 보지 않고 키 이름만 본다)
fn query_carries_secret(url: &str) -> bool {
    let Some((_, query)) = url.split_once('?') else {
        return false;
    };
    let query = query.split('#').next().unwrap_or(query);
    query.split('&').any(|pair| {
        let name = pair
            .split('=')
            .next()
            .unwrap_or(pair)
            .trim()
            .to_ascii_lowercase();
        SECRET_QUERY_KEYS.contains(&name.as_str())
            || SECRET_QUERY_PARTS.iter().any(|part| name.contains(part))
    })
}

// RFC 3986 unreserved 외 모든 byte를 %XX로 (query 값 전체를 한 매개변수로 싣기 위함)
fn percent_encode(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len() * 3);
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_loopback_hosts_without_network() {
        for url in [
            "http://127.0.0.1:8080/page",
            "http://localhost/x",
            "http://[::1]:3000/",
            "http://[0:0:0:0:0:0:0:1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://127.1.2.3/",
        ] {
            let failures = fetch_alternate(url, &FetchOptions::default(), false).unwrap_err();
            assert!(failures[0].contains("loopback"), "{url}: {failures:?}");
        }
    }

    #[test]
    fn keeps_query_and_credential_urls_away_from_wayback() {
        // IP literal은 DNS 없이 해석되므로 Wayback 전송 판정까지만 가고 네트워크를 쓰지 않는다
        for url in [
            "https://8.8.8.8/doc?token=secret",
            "https://8.8.8.8/f?X-Amz-Signature=abc&X-Amz-Expires=60",
            "https://user:pw@8.8.8.8/doc",
        ] {
            let failures = fetch_alternate(url, &FetchOptions::default(), false).unwrap_err();
            assert_eq!(failures.len(), 1, "{url}: {failures:?}");
            assert!(
                failures[0].contains("token-like query"),
                "{url}: {failures:?}"
            );
        }
    }

    #[test]
    fn flags_token_like_query_keys_only() {
        for url in [
            "https://h/p?access_token=x",
            "https://h/p?q=1&Code=abc",
            "https://h/p?sessionId=1",
            "https://h/p?apiKey=1",
        ] {
            assert!(query_carries_secret(url), "{url}");
        }
        for url in [
            "https://h/p",
            "https://h/p?q=rust&page=2",
            "https://h/p?id=5#access_token=fragment-only",
        ] {
            assert!(!query_carries_secret(url), "{url}");
        }
    }

    #[test]
    fn percent_encodes_query_values() {
        assert_eq!(
            percent_encode("https://medium.com/@a/b?c=1&d"),
            "https%3A%2F%2Fmedium.com%2F%40a%2Fb%3Fc%3D1%26d"
        );
    }
}
