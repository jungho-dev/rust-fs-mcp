//! gate.rs
//! tools::redis::gate
//!
//! redis-command gate: every command of a call is checked before anything is sent.
//! Rejects server escape and admin changes (R1), streaming (R2), connection state changes (R3),
//! Lua scripting (R4), blocking waits outside 1 ms to timeout_ms minus 500 ms (R5), and unpaired MULTI.
//! Names compare case-insensitively; the subcommand is argv[1].
//!

use regex::Regex;
use std::sync::LazyLock;

const ESCAPE_COMMANDS: [&str; 9] = [
    "MIGRATE",
    "REPLICAOF",
    "SLAVEOF",
    "FAILOVER",
    "SYNC",
    "PSYNC",
    "REPLCONF",
    "SHUTDOWN",
    "DEBUG",
];
const CONFIG_SUBS: [&str; 3] = ["GET", "HELP", "RESETSTAT"];
const MODULE_SUBS: [&str; 2] = ["LIST", "HELP"];
const CLUSTER_SUBS: [&str; 11] = [
    "INFO",
    "NODES",
    "SLOTS",
    "SHARDS",
    "KEYSLOT",
    "COUNTKEYSINSLOT",
    "GETKEYSINSLOT",
    "MYID",
    "MYSHARDID",
    "LINKS",
    "HELP",
];
const SENTINEL_SUBS: [&str; 10] = [
    "MASTERS",
    "MASTER",
    "REPLICAS",
    "SLAVES",
    "SENTINELS",
    "GET-MASTER-ADDR-BY-NAME",
    "INFO-CACHE",
    "CKQUORUM",
    "MYID",
    "HELP",
];
const STREAM_COMMANDS: [&str; 7] = [
    "SUBSCRIBE",
    "PSUBSCRIBE",
    "SSUBSCRIBE",
    "UNSUBSCRIBE",
    "PUNSUBSCRIBE",
    "SUNSUBSCRIBE",
    "MONITOR",
];
const STATE_COMMANDS: [&str; 4] = ["AUTH", "HELLO", "RESET", "QUIT"];
const STATE_CLIENT_SUBS: [&str; 2] = ["REPLY", "TRACKING"];
const SCRIPT_COMMANDS: [&str; 6] = [
    "EVAL",
    "EVALSHA",
    "EVAL_RO",
    "EVALSHA_RO",
    "FCALL",
    "FCALL_RO",
];
const SCRIPT_FUNCTION_SUBS: [&str; 2] = ["LOAD", "RESTORE"];
const SUB_COMMANDS: [&str; 6] = [
    "CONFIG", "MODULE", "CLUSTER", "CLIENT", "FUNCTION", "SENTINEL",
];
static BLOCK_TIME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9]+(\.[0-9]+)?$").expect("valid block time pattern"));

// 1. Batch gate ---------------------------------------------------------------
pub(super) fn check_command_batch(commands: &[Vec<String>], timeout_ms: u64) -> Result<(), String> {
    // 1. 차단 대기 밀리초 (형식 오류 = 원문)
    let find_block_ms = |name: &str, argv: &[String]| -> Option<Result<u64, String>> {
        let (raw, in_seconds) = match name {
            "BLPOP" | "BRPOP" | "BRPOPLPUSH" | "BLMOVE" | "BZPOPMIN" | "BZPOPMAX"
                if argv.len() > 2 =>
            {
                (argv.last()?, true)
            }
            "BLMPOP" | "BZMPOP" => (argv.get(1)?, true),
            "WAIT" => (argv.get(2)?, false),
            "WAITAOF" => (argv.get(3)?, false),
            "XREAD" | "XREADGROUP" => {
                // XREADGROUP GROUP 피연산자 제외
                let mut operands = 0;
                let position = argv
                    .get(1..)?
                    .iter()
                    .take_while(|word| !word.eq_ignore_ascii_case("STREAMS"))
                    .position(|word| {
                        if operands > 0 {
                            operands -= 1;
                            return false;
                        }
                        if name == "XREADGROUP" && word.eq_ignore_ascii_case("GROUP") {
                            operands = 2;
                            return false;
                        }
                        word.eq_ignore_ascii_case("BLOCK")
                    })?;
                (argv.get(position + 2)?, false)
            }
            _ => return None,
        };
        if !BLOCK_TIME.is_match(raw) {
            return Some(Err(raw.clone()));
        }
        let amount: f64 = raw.parse().ok()?;
        // 초 = 밀리초 버림 (1 ms 미만 = 0)
        let millis = if in_seconds {
            (amount * 1000.0).floor()
        } else {
            amount.ceil()
        };
        Some(Ok(millis as u64))
    };

    // 2. 명령별 판정
    let block_limit_ms = timeout_ms.saturating_sub(500);
    let mut open_multi: Option<usize> = None;
    for (index, argv) in commands.iter().enumerate() {
        let number = index + 1;
        let name = argv[0].to_ascii_uppercase();
        let sub = argv
            .get(1)
            .map(|word| word.to_ascii_uppercase())
            .unwrap_or_default();
        let label = if SUB_COMMANDS.contains(&name.as_str()) && !sub.is_empty() {
            format!("{name} {sub}")
        } else {
            name.clone()
        };
        let sub = sub.as_str();
        let escapes = ESCAPE_COMMANDS.contains(&name.as_str())
            || (name == "CONFIG" && !CONFIG_SUBS.contains(&sub))
            || (name == "MODULE" && !MODULE_SUBS.contains(&sub))
            || (name == "CLUSTER" && !CLUSTER_SUBS.contains(&sub))
            || (name == "SENTINEL" && !SENTINEL_SUBS.contains(&sub));
        let changes_state = STATE_COMMANDS.contains(&name.as_str())
            || (name == "CLIENT" && STATE_CLIENT_SUBS.contains(&sub));
        let scripts = SCRIPT_COMMANDS.contains(&name.as_str())
            || (name == "FUNCTION" && SCRIPT_FUNCTION_SUBS.contains(&sub));
        let block = find_block_ms(&name, argv);
        let reason = if escapes {
            Some(format!(
                "`{label}` can make the server reach an address outside config.toml or change the server itself; run it with redis-cli outside this tool"
            ))
        } else if STREAM_COMMANDS.contains(&name.as_str()) {
            Some(format!(
                "`{label}` streams messages without end; this tool runs request-reply commands only (PUBLISH and PUBSUB work)"
            ))
        } else if name == "SELECT" {
            Some(
                "`SELECT` would switch the cached connection's database; pass the db argument instead"
                    .to_string(),
            )
        } else if changes_state {
            Some(format!(
                "`{label}` changes the connection's identity, protocol, or reply flow, which come from config.toml"
            ))
        } else if scripts {
            Some(format!(
                "`{label}` runs server-side Lua, which can call MIGRATE and reach other addresses, so scripting is off"
            ))
        } else {
            match block {
                Some(Err(raw)) => Some(format!(
                    "`{label}` block time `{raw}` must be a plain decimal number"
                )),
                Some(Ok(block_ms)) if !(1..=block_limit_ms).contains(&block_ms) => Some(format!(
                    "`{label}` blocks for {block_ms} ms (0 = forever); use 1 to {block_limit_ms} ms (timeout_ms minus 500) so the reply arrives before the call times out"
                )),
                _ => None,
            }
        };
        if let Some(reason) = reason {
            return Err(format!(
                "Rejected by the redis-command gate (command {number}): {reason}"
            ));
        }

        // 3. MULTI 짝
        if name == "MULTI" && open_multi.is_some() {
            return Err(format!(
                "Rejected by the redis-command gate (command {number}): nested MULTI at command {number}"
            ));
        }
        match name.as_str() {
            "MULTI" => open_multi = Some(number),
            "EXEC" | "DISCARD" => open_multi = None,
            _ => {}
        }
    }

    // 4. 닫히지 않은 MULTI
    if let Some(number) = open_multi {
        return Err(format!(
            "Rejected by the redis-command gate (command {number}): MULTI at command {number} has no EXEC or DISCARD later in this call"
        ));
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    fn check_rows(rows: &[&[&str]], timeout_ms: u64) -> Result<(), String> {
        let commands: Vec<Vec<String>> = rows
            .iter()
            .map(|row| row.iter().map(|word| word.to_string()).collect())
            .collect();
        check_command_batch(&commands, timeout_ms)
    }
    fn rejection(rows: &[&[&str]]) -> String {
        check_rows(rows, 3_000).unwrap_err()
    }
    #[test]
    fn rejects_server_escape_and_admin_changes() {
        assert_eq!(
            rejection(&[&["GET", "k"], &["MIGRATE", "h", "1", "k", "0", "5"]]),
            "Rejected by the redis-command gate (command 2): `MIGRATE` can make the server reach an address outside config.toml or change the server itself; run it with redis-cli outside this tool"
        );
        assert!(rejection(&[&["config", "set", "dir", "/tmp"]]).contains("`CONFIG SET` can make"));
        assert!(rejection(&[&["CONFIG"]]).contains("`CONFIG` can make"));
        assert!(rejection(&[&["MODULE", "LOAD", "x.so"]]).contains("`MODULE LOAD`"));
        assert!(rejection(&[&["cluster", "meet", "h", "1"]]).contains("`CLUSTER MEET`"));
        assert!(rejection(&[&["replicaof", "h", "1"]]).contains("`REPLICAOF`"));
        assert_eq!(
            rejection(&[&["sentinel", "set", "m", "k", "v"]]),
            "Rejected by the redis-command gate (command 1): `SENTINEL SET` can make the server reach an address outside config.toml or change the server itself; run it with redis-cli outside this tool"
        );
        assert!(
            rejection(&[&["SENTINEL", "MONITOR", "m", "10.0.0.9", "6379", "2"]])
                .contains("`SENTINEL MONITOR` can make")
        );
    }
    #[test]
    fn allows_admin_reads_and_destructive_data_commands() {
        let allowed: &[&[&str]] = &[
            &["config", "get", "maxmemory"],
            &["CLUSTER", "INFO"],
            &["MODULE", "LIST"],
            &["XREAD", "COUNT", "2", "STREAMS", "s", "0"],
            &["XREAD", "STREAMS", "BLOCK", "0"],
            &["FLUSHDB"],
            &["KEYS", "*"],
            &["CLIENT", "KILL", "ID", "9"],
            &["FUNCTION", "LIST"],
            &["PUBLISH", "c", "m"],
            &["sentinel", "masters"],
        ];
        assert_eq!(check_rows(allowed, 3_000), Ok(()));
    }
    #[test]
    fn rejects_streaming_state_and_scripting() {
        assert_eq!(
            rejection(&[&["subscribe", "c"]]),
            "Rejected by the redis-command gate (command 1): `SUBSCRIBE` streams messages without end; this tool runs request-reply commands only (PUBLISH and PUBSUB work)"
        );
        assert_eq!(
            rejection(&[&["select", "1"]]),
            "Rejected by the redis-command gate (command 1): `SELECT` would switch the cached connection's database; pass the db argument instead"
        );
        assert_eq!(
            rejection(&[&["Hello", "3"]]),
            "Rejected by the redis-command gate (command 1): `HELLO` changes the connection's identity, protocol, or reply flow, which come from config.toml"
        );
        assert!(rejection(&[&["client", "reply", "off"]]).contains("`CLIENT REPLY` changes"));
        assert!(rejection(&[&["AUTH", "pw"]]).contains("`AUTH` changes"));
        assert_eq!(
            rejection(&[&["eval", "return 1", "0"]]),
            "Rejected by the redis-command gate (command 1): `EVAL` runs server-side Lua, which can call MIGRATE and reach other addresses, so scripting is off"
        );
        assert!(rejection(&[&["function", "load", "x"]]).contains("`FUNCTION LOAD` runs"));
        assert!(rejection(&[&["FCALL_RO", "f", "0"]]).contains("`FCALL_RO` runs"));
    }
    #[test]
    fn bounds_blocking_waits_by_the_call_timeout() {
        assert_eq!(
            rejection(&[&["blpop", "k", "0"]]),
            "Rejected by the redis-command gate (command 1): `BLPOP` blocks for 0 ms (0 = forever); use 1 to 2500 ms (timeout_ms minus 500) so the reply arrives before the call times out"
        );
        assert!(rejection(&[&["BLPOP", "k", "3"]]).contains("blocks for 3000 ms"));
        assert!(
            rejection(&[&["BZMPOP", "0", "1", "z", "MIN"]]).contains("`BZMPOP` blocks for 0 ms")
        );
        assert!(
            rejection(&[&["xread", "block", "0", "STREAMS", "s", "$"]]).contains("`XREAD` blocks")
        );
        assert!(
            rejection(&[&[
                "XREADGROUP",
                "GROUP",
                "g",
                "c",
                "BLOCK",
                "2600",
                "STREAMS",
                "s",
                ">"
            ]])
            .contains("blocks for 2600 ms")
        );
        assert!(rejection(&[&["WAIT", "1", "0"]]).contains("`WAIT` blocks for 0 ms"));
        assert!(rejection(&[&["WAITAOF", "1", "0", "0"]]).contains("`WAITAOF` blocks"));
        assert_eq!(
            rejection(&[&["BLPOP", "k", "0x0"]]),
            "Rejected by the redis-command gate (command 1): `BLPOP` block time `0x0` must be a plain decimal number"
        );
        assert!(rejection(&[&["BLPOP", "k", "1e3"]]).contains("block time `1e3`"));
        assert!(rejection(&[&["WAIT", "1", "-1"]]).contains("block time `-1`"));
        assert!(
            rejection(&[&[
                "XREADGROUP",
                "GROUP",
                "g",
                "c",
                "BLOCK",
                "0",
                "STREAMS",
                "s",
                ">"
            ]])
            .contains("`XREADGROUP` blocks for 0 ms")
        );
        let allowed: &[&[&str]] = &[
            &["BLPOP", "k", "1"],
            &["BRPOP", "a", "b", "2.5"],
            &["XREAD", "BLOCK", "2500", "STREAMS", "s", "$"],
            &["WAIT", "1", "100"],
            &["XREADGROUP", "GROUP", "g", "BLOCK", "STREAMS", "s", ">"],
        ];
        assert_eq!(check_rows(allowed, 3_000), Ok(()));
    }
    #[test]
    fn reads_xreadgroup_block_outside_the_group_operands() {
        assert!(
            rejection(&[&[
                "XREADGROUP",
                "BLOCK",
                "0",
                "GROUP",
                "g",
                "c",
                "STREAMS",
                "s",
                ">"
            ]])
            .contains("`XREADGROUP` blocks for 0 ms")
        );
        let counted: &[&[&str]] = &[&[
            "XREADGROUP",
            "GROUP",
            "g",
            "c",
            "COUNT",
            "1",
            "BLOCK",
            "100",
            "NOACK",
            "STREAMS",
            "s",
            ">",
        ]];
        assert_eq!(check_rows(counted, 3_000), Ok(()));
        assert!(
            check_rows(counted, 500)
                .unwrap_err()
                .contains("`XREADGROUP` blocks for 100 ms")
        );
        let consumer: &[&[&str]] = &[&["XREADGROUP", "GROUP", "g", "BLOCK", "STREAMS", "s", ">"]];
        assert_eq!(check_rows(consumer, 3_000), Ok(()));
    }
    #[test]
    fn truncates_second_block_times_to_whole_milliseconds() {
        assert_eq!(
            rejection(&[&["BLPOP", "k", "0.0001"]]),
            "Rejected by the redis-command gate (command 1): `BLPOP` blocks for 0 ms (0 = forever); use 1 to 2500 ms (timeout_ms minus 500) so the reply arrives before the call times out"
        );
        assert_eq!(check_rows(&[&["BLPOP", "k", "0.001"]], 3_000), Ok(()));
    }
    #[test]
    fn checks_multi_pairing() {
        let paired: &[&[&str]] = &[
            &["MULTI"],
            &["SET", "k", "v"],
            &["EXEC"],
            &["multi"],
            &["discard"],
        ];
        assert_eq!(check_rows(paired, 3_000), Ok(()));
        assert_eq!(
            rejection(&[&["MULTI"], &["multi"], &["EXEC"]]),
            "Rejected by the redis-command gate (command 2): nested MULTI at command 2"
        );
        assert_eq!(
            rejection(&[&["GET", "k"], &["MULTI"], &["SET", "k", "v"]]),
            "Rejected by the redis-command gate (command 2): MULTI at command 2 has no EXEC or DISCARD later in this call"
        );
    }
}
