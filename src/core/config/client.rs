//! client.rs
//! core::config::client
//!
//! MCP host detection from the initialize clientInfo.name and the per-host result policy.
//! Known names match exactly; anything else is Unknown and keeps the built-in defaults.
//! The latest initialize replaces the recorded host, so later tools/call responses follow it.
//!

use crate::core::settings::CompressMode;
use std::sync::{PoisonError, RwLock};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ClientHost {
    ClaudeCode,
    Codex,
    // GitHub Copilot in VS Code
    VsCode,
    // GitHub Copilot CLI
    CopilotCli,
    GeminiCli,
    OpenCode,
    #[default]
    Unknown,
}
// Copilot CLI 파일 저장 임계 (20 KiB 미만)
const COPILOT_CLI_OUTPUT_BYTES: usize = 19_000;
static CLIENT_HOST: RwLock<ClientHost> = RwLock::new(ClientHost::Unknown);

impl ClientHost {
    // 1. Host classification --------------------------------------------------
    // 실측 이름 (VS Code 공식 빌드 미검증)
    pub fn from_client_name(name: &str) -> Self {
        match name {
            "claude-code" => Self::ClaudeCode,
            "codex-mcp-client" => Self::Codex,
            "Code - OSS" | "Visual Studio Code" | "Visual Studio Code - Insiders" => Self::VsCode,
            "copilot-cli" => Self::CopilotCli,
            "gemini-cli-mcp-client" => Self::GeminiCli,
            "opencode" => Self::OpenCode,
            _ => Self::Unknown,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::VsCode => "vscode",
            Self::CopilotCli => "copilot-cli",
            Self::GeminiCli => "gemini-cli",
            Self::OpenCode => "opencode",
            Self::Unknown => "unknown",
        }
    }
    // 2. Result policy --------------------------------------------------------
    // 본문·이미지 content 전달 호스트
    pub fn default_compress(self) -> CompressMode {
        match self {
            Self::Codex | Self::VsCode | Self::CopilotCli | Self::GeminiCli | Self::OpenCode => {
                CompressMode::Lossless
            }
            Self::ClaudeCode | Self::Unknown => CompressMode::Off,
        }
    }
    pub fn default_output_bytes(self) -> Option<usize> {
        match self {
            Self::CopilotCli => Some(COPILOT_CLI_OUTPUT_BYTES),
            _ => None,
        }
    }
}

// 3. Host store ---------------------------------------------------------------
pub fn record_client(name: &str) -> ClientHost {
    let host = ClientHost::from_client_name(name);
    *CLIENT_HOST.write().unwrap_or_else(PoisonError::into_inner) = host;
    host
}
pub fn current_client_host() -> ClientHost {
    *CLIENT_HOST.read().unwrap_or_else(PoisonError::into_inner)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_verified_names_exactly() {
        let cases = [
            ("claude-code", ClientHost::ClaudeCode),
            ("codex-mcp-client", ClientHost::Codex),
            ("Code - OSS", ClientHost::VsCode),
            ("Visual Studio Code", ClientHost::VsCode),
            ("Visual Studio Code - Insiders", ClientHost::VsCode),
            ("copilot-cli", ClientHost::CopilotCli),
            ("gemini-cli-mcp-client", ClientHost::GeminiCli),
            ("opencode", ClientHost::OpenCode),
        ];
        for (name, host) in cases {
            assert_eq!(ClientHost::from_client_name(name), host, "{name}");
        }
        for name in [
            "",
            "codex",
            "copilot",
            "mcp-test-client",
            "OpenCode",
            "visual studio code",
            "Visual Studio Code Extra",
        ] {
            assert_eq!(
                ClientHost::from_client_name(name),
                ClientHost::Unknown,
                "{name}"
            );
        }
    }
    #[test]
    fn keeps_default_policy_for_claude_and_unknown() {
        for host in [ClientHost::ClaudeCode, ClientHost::Unknown] {
            assert_eq!(host.default_compress(), CompressMode::Off);
            assert_eq!(host.default_output_bytes(), None);
        }
        for host in [
            ClientHost::Codex,
            ClientHost::VsCode,
            ClientHost::CopilotCli,
            ClientHost::GeminiCli,
            ClientHost::OpenCode,
        ] {
            assert_eq!(host.default_compress(), CompressMode::Lossless);
        }
        assert_eq!(ClientHost::CopilotCli.default_output_bytes(), Some(19_000));
        assert_eq!(ClientHost::Codex.default_output_bytes(), None);
        assert_eq!(ClientHost::VsCode.default_output_bytes(), None);
    }
}
