//! redis.rs
//! core::config::redis
//!
//! `[redis]` and `[redis.<name>]` sections of config.toml: the only Redis targets redis-command can reach.
//!

use super::settings::parse_string;
use crate::core::config::resolve_path;
use crate::core::settings::PasswordSource;
use std::collections::{BTreeMap, BTreeSet};

const DEFAULT_REDIS_TIMEOUT_MS: u64 = 10_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RedisTarget {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: Option<String>,
    pub password: PasswordSource,
    pub db: u32,
    pub connect_timeout_ms: u64,
    // 호출 인자 상한 겸용
    pub command_timeout_ms: u64,
    pub max_items: usize,
}
#[derive(Default)]
pub struct RedisSections {
    targets: Vec<RedisTarget>,
    // 대상 이름별 원시 값
    entries: BTreeMap<String, RedisEntries>,
    // 경고 낸 무효 구역
    rejected: BTreeSet<String>,
}
impl RedisSections {
    pub fn claims(section: &str) -> bool {
        section == "redis" || section.starts_with("redis.")
    }
    pub fn accept(
        &mut self,
        section: &str,
        key: &str,
        raw_value: &str,
        line_no: usize,
        warnings: &mut Vec<String>,
    ) {
        // 1. 대상 이름
        let name = section.strip_prefix("redis.").unwrap_or("default");
        let name_valid = (1..=32).contains(&name.len())
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
        if !name_valid && self.rejected.insert(section.to_string()) {
            warnings.push(format!(
                "redis section [{section}] needs a name of 1-32 letters, digits, _ or -; section ignored"
            ));
        }
        if !name_valid {
            return;
        }
        let entries = self.entries.entry(name.to_string()).or_default();

        // 2. 키별 값 (비밀번호 공백 보존)
        let text = parse_string(raw_value);
        let trimmed = text.map(str::trim).filter(|value| !value.is_empty());
        let number = raw_value.replace('_', "").parse::<u64>().ok();
        match (key, text, trimmed, number) {
            ("password", Some(text), _, _) => {
                entries.texts.insert(key.to_string(), text.to_string());
            }
            ("host" | "user" | "password_env" | "password_dpapi", _, Some(text), _) => {
                entries.texts.insert(key.to_string(), text.to_string());
            }
            ("host" | "user" | "password_env" | "password_dpapi" | "password", ..) => {
                warnings.push(format!(
                    "line {line_no}: redis.{name}.{key} must be a non-empty string"
                ));
            }
            (
                "port" | "connect_timeout_ms" | "command_timeout_ms" | "max_items",
                _,
                _,
                Some(number),
            ) if number > 0 => {
                entries.numbers.insert(key.to_string(), number);
            }
            ("port" | "connect_timeout_ms" | "command_timeout_ms" | "max_items", ..) => {
                warnings.push(format!(
                    "line {line_no}: redis.{name}.{key} must be a positive integer"
                ));
            }
            ("db", _, _, Some(number)) if number <= u64::from(u32::MAX) => {
                entries.numbers.insert(key.to_string(), number);
            }
            ("db", ..) => {
                warnings.push(format!(
                    "line {line_no}: redis.{name}.{key} must be a non-negative integer"
                ));
            }
            _ => warnings.push(format!(
                "line {line_no}: unknown key `redis.{name}.{key}` ignored"
            )),
        }
    }
    pub fn finish(self, warnings: &mut Vec<String>) -> Vec<RedisTarget> {
        let mut targets = self.targets;
        for (name, mut entries) in self.entries {
            // 1. 비밀번호 출처 (Env > Dpapi > Plain)
            let env_name = entries.texts.remove("password_env");
            let sealed_text = entries.texts.remove("password_dpapi");
            let plain = entries.texts.remove("password");
            let sealed_path = sealed_text.as_deref().map(resolve_path);
            if env_name.is_none()
                && let (Some(sealed_text), Some(Err(error))) = (&sealed_text, &sealed_path)
            {
                warnings.push(format!(
                    "redis.{name}.password_dpapi `{sealed_text}` unresolved: {error}"
                ));
            }
            let password = match (env_name, sealed_path, plain) {
                (Some(env_name), _, _) => PasswordSource::Env(env_name),
                (None, Some(Ok(sealed_path)), _) => PasswordSource::Dpapi(sealed_path),
                (None, Some(Err(_)), _) => PasswordSource::Unset,
                (None, None, Some(text)) => PasswordSource::Plain(text),
                (None, None, None) => PasswordSource::Unset,
            };

            // 2. 필수 키와 범위
            let Some(host) = entries.texts.remove("host") else {
                warnings.push(format!(
                    "redis.{name}.host is required; target {name} stays off"
                ));
                continue;
            };
            let Ok(port) = u16::try_from(entries.numbers.get("port").copied().unwrap_or(6379))
            else {
                warnings.push(format!("redis.{name}.port must be between 1 and 65535"));
                continue;
            };
            let db = entries.numbers.get("db").copied().unwrap_or(0);
            let max_items = entries.numbers.get("max_items").copied().unwrap_or(1_000);
            targets.push(RedisTarget {
                name,
                host,
                port,
                user: entries.texts.remove("user"),
                password,
                db: u32::try_from(db).unwrap_or(u32::MAX),
                connect_timeout_ms: entries
                    .numbers
                    .get("connect_timeout_ms")
                    .copied()
                    .unwrap_or(DEFAULT_REDIS_TIMEOUT_MS),
                command_timeout_ms: entries
                    .numbers
                    .get("command_timeout_ms")
                    .copied()
                    .unwrap_or(DEFAULT_REDIS_TIMEOUT_MS),
                max_items: usize::try_from(max_items).unwrap_or(usize::MAX),
            });
        }
        targets.sort_by(|left, right| left.name.cmp(&right.name));
        targets
    }
}
#[derive(Default)]
struct RedisEntries {
    texts: BTreeMap<String, String>,
    numbers: BTreeMap<String, u64>,
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::settings::parse_settings;

    fn parse_redis(text: &str) -> (Vec<RedisTarget>, Vec<String>) {
        let (settings, warnings) = parse_settings(text);
        (settings.redis, warnings)
    }
    #[test]
    fn parses_single_default_section_with_defaults() {
        let (targets, warnings) = parse_redis("[redis]\nhost = \"127.0.0.1\"\n");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            targets,
            [RedisTarget {
                name: "default".to_string(),
                host: "127.0.0.1".to_string(),
                port: 6379,
                user: None,
                password: PasswordSource::Unset,
                db: 0,
                connect_timeout_ms: 10_000,
                command_timeout_ms: 10_000,
                max_items: 1_000,
            }]
        );
    }
    #[test]
    fn parses_named_targets_sorted_by_name() {
        let text = "[redis.zeta]\nhost = 'z.example.invalid'\n[redis.alpha_1]\nhost = \" a.example.invalid \"\nport = 6380\nuser = \"reader\"\ndb = 3\nconnect_timeout_ms = 2_000\ncommand_timeout_ms = 5000\nmax_items = 50\n";
        let (targets, warnings) = parse_redis(text);
        assert!(warnings.is_empty(), "{warnings:?}");
        let names: Vec<&str> = targets.iter().map(|target| target.name.as_str()).collect();
        assert_eq!(names, ["alpha_1", "zeta"]);
        assert_eq!(
            targets[0],
            RedisTarget {
                name: "alpha_1".to_string(),
                host: "a.example.invalid".to_string(),
                port: 6380,
                user: Some("reader".to_string()),
                password: PasswordSource::Unset,
                db: 3,
                connect_timeout_ms: 2_000,
                command_timeout_ms: 5_000,
                max_items: 50,
            }
        );
        assert_eq!(targets[1].port, 6379);
    }
    #[test]
    fn warns_on_invalid_values_and_unknown_keys() {
        let text =
            "[redis.a]\nhost = 5\nhost = \"h\"\nport = 0\ndb = -1\nmax_items = x\ncolour = 1\n";
        let (targets, warnings) = parse_redis(text);
        assert_eq!(
            warnings,
            [
                "line 2: redis.a.host must be a non-empty string",
                "line 4: redis.a.port must be a positive integer",
                "line 5: redis.a.db must be a non-negative integer",
                "line 6: redis.a.max_items must be a positive integer",
                "line 7: unknown key `redis.a.colour` ignored",
            ]
        );
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].port, 6379);
    }
    #[test]
    fn ignores_sections_with_invalid_names() {
        let long_name = "n".repeat(33);
        let text = format!(
            "[redis.bad name!]\nhost = \"h\"\nport = 1\n[redis.]\nhost = \"h\"\n[redis.{long_name}]\nhost = \"h\"\n[redis.{}]\nhost = \"h\"\n",
            "m".repeat(32)
        );
        let (targets, warnings) = parse_redis(&text);
        assert_eq!(
            warnings,
            [
                "redis section [redis.bad name!] needs a name of 1-32 letters, digits, _ or -; section ignored".to_string(),
                "redis section [redis.] needs a name of 1-32 letters, digits, _ or -; section ignored".to_string(),
                format!("redis section [redis.{long_name}] needs a name of 1-32 letters, digits, _ or -; section ignored"),
            ]
        );
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].name, "m".repeat(32));
    }
    #[test]
    fn drops_targets_without_host_or_with_bad_port() {
        let text = "[redis.nohost]\nport = 6380\n[redis.p]\nhost = \"h\"\nport = 70000\n";
        let (targets, warnings) = parse_redis(text);
        assert!(targets.is_empty());
        assert_eq!(
            warnings,
            [
                "redis.nohost.host is required; target nohost stays off",
                "redis.p.port must be between 1 and 65535",
            ]
        );
    }
    #[test]
    fn picks_password_source_in_order() {
        let sealed_text = std::env::temp_dir().join("fs-mcp-redis.dpapi");
        let sealed_text = sealed_text.to_string_lossy();
        let text = format!(
            "[redis.a]\nhost = \"h\"\npassword = \"p\"\npassword_dpapi = '{sealed_text}'\npassword_env = \"SAMPLE_REDIS_PASS\"\n[redis.b]\nhost = \"h\"\npassword = \"p\"\npassword_dpapi = '{sealed_text}'\n[redis.c]\nhost = \"h\"\npassword = ' spaced '\n"
        );
        let (targets, warnings) = parse_redis(&text);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            targets[0].password,
            PasswordSource::Env("SAMPLE_REDIS_PASS".to_string())
        );
        assert_eq!(
            targets[1].password,
            PasswordSource::Dpapi(resolve_path(sealed_text.as_ref()).unwrap())
        );
        assert_eq!(
            targets[2].password,
            PasswordSource::Plain(" spaced ".to_string())
        );
    }
    #[test]
    fn plain_password_stays_out_of_debug_output() {
        let text = "[redis]\nhost = \"h\"\npassword = 'sentinel-secret'\n";
        let (settings, warnings) = parse_settings(text);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            settings.redis[0].password,
            PasswordSource::Plain("sentinel-secret".to_string())
        );
        assert!(!format!("{settings:?}").contains("sentinel-secret"));
        assert!(!format!("{:?}", settings.redis[0]).contains("sentinel-secret"));
    }
}
