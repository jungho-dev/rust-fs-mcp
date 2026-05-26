# rust-fs-mcp

rust-fs-mcp는 기존 fs-mcp의 공개 tool 계약을 Rust stdio MCP 서버로 이식하는 프로젝트입니다.
stdin/stdout 기반 line JSON-RPC로 filesystem, search, git tool을 제공합니다.

이 프로젝트는 중요한 계약 형태를 유지합니다. 공개 tool 이름, batch-first 입력, args_path 계열 대용량 인자 참조,
정규화된 fs-mcp 응답 envelope를 그대로 유지합니다.

## 현재 상태

- tools/list가 21개 MCP tool을 노출하며 tool matrix integration test가 이를 검증합니다.
- 서버는 initialize, tools/list, tools/call, resources/list, resources/templates/list를 처리합니다.
- filesystem, search, git tool은 Rust 코드 경로에서 동작합니다.
- 구현된 git 표면은 git CLI를 호출하지 않습니다.
- search와 exclude-aware listing은 build output에 포함된 project-bundled 실행 파일을 호출합니다.
- resources는 현재 비어 있습니다. 현재 범위는 tool parity 우선입니다.

## Tool Surface

| 영역 | Tools |
| --- | --- |
| Files and directories | file-read, file-lines, file-write, dir-mk, dir-list |
| File mutation and metadata | file-copy, file-move, file-remove, file-infos, file-edit |
| Search | search-start, search-regex, search-get, search-stop |
| Git | git-cwd, git-status, git-add, git-commit, git-diff, git-show |
| Inspect | fs-inspect |

## Runtime Model

binary는 stdio server로 실행됩니다. 입력 한 줄은 JSON-RPC request 하나이며, 응답도 한 줄로 출력됩니다.

```powershell
cargo run --release
```

initialize request 예시:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}
```

tool listing request 예시:

```json
{"jsonrpc":"2.0","id":2,"method":"tools/list"}
```

tool call 예시:

```json
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"dir-list","arguments":{"items":[{"path":"."}]}}}
```

## Configuration

runtime configuration은 process memory에 저장됩니다.

| Key | 목적 |
| --- | --- |
| allowedDirectories | local filesystem과 cwd 기반 process 접근을 지정 root로 제한합니다. 비어 있으면 제한하지 않습니다. |
| blockedCommands | 내부 process lifecycle helper용 설정입니다. public MCP tool control로 노출하지 않습니다. |
| defaultShell | 내부 process lifecycle helper용 설정입니다. |
| RUST_FS_MCP_TOOL_PROFILE | 선택 process env profile입니다. fast-coding을 사용하면 tools/list에 fs-inspect만 노출합니다. |

allowedDirectories는 FS_MCP_ALLOWED_DIRECTORIES 환경 변수로 초기화할 수 있습니다. 값은 platform path-list
separator를 사용합니다. Windows 기본 shell은 powershell이고, 그 외 platform 기본값은 sh입니다.

기본 blocked command는 rm, rmdir, del, erase, format, mkfs, diskpart, shutdown, reboot, halt, poweroff입니다.

## Response Envelope

모든 tool call은 같은 envelope로 정규화됩니다.

- content는 MCP client 표시용 text를 담습니다.
- structuredContent.data.content는 정규화된 content block을 담습니다.
- structuredContent.data.structuredContent는 tool별 structured data를 담습니다.
- structuredContent.status는 success 또는 error입니다.
- structuredContent.schemaVersion은 1입니다.
- _meta.fsMcpResult는 status, duration, content type, structured-content 존재 여부를 반복 제공합니다.
- tool 실패 시 isError가 설정됩니다.

Batch tool은 result index, 원본 input 요약, per-item status, succeededCount, failedCount, totalCount를 반환합니다.

## Module Layout

| Path | 책임 |
| --- | --- |
| src/main.rs | binary entry point입니다. stdio MCP server를 실행하고 fatal startup error에서 non-zero로 종료합니다. |
| src/lib.rs | 안정적인 내부 호출 경로를 유지하는 public module export입니다. |
| src/protocol/server.rs | line JSON-RPC 처리, initialize response, tool call, empty resource 응답입니다. |
| src/protocol/catalog.rs | MCP tool catalog, tool annotation, JSON input schema입니다. |
| src/core/args_ref.rs | args_path, args_offset, args_length 기반 대용량 JSON argument 해석입니다. |
| src/core/batch.rs | batch 실행 결과 shape와 per-item summary입니다. |
| src/core/bundled.rs | project-bundled CLI 실행 파일 resolver와 timeout wrapper입니다. |
| src/core/config.rs | runtime config, path normalization, allowed-directory check, blocked command입니다. |
| src/core/response.rs | RawResult, display text, sanitization, timing, envelope normalization입니다. |
| src/tools/fs_tools.rs | file, directory, metadata, exact edit, image, HTTP read tool입니다. |
| src/tools/search_tools.rs | regex, literal, context, pagination을 지원하는 file/content search session입니다. |
| src/tools/inspect_tools.rs | 코딩 작업용 compact read-only filesystem inspection request를 처리합니다. |
| src/tools/git_tools.rs | repository file을 직접 다루는 git cwd, status, add, commit, diff, show입니다. |
| src/tools/process_tools.rs | 알려진 process session에 stdin을 보내는 interaction입니다. Lifecycle helper는 내부 구현이며 public MCP tool이 아닙니다. |
| tests/tool_matrix.rs | catalog tool 전체가 dispatch를 통해 호출 가능한지 검증하는 integration check입니다. |

자세한 request flow와 module contract는 ARCHITECTURE-ko.md를 참조하세요.

## Filesystem Tools

Filesystem tool은 읽기/쓰기 전에 runtime config boundary로 path를 검증합니다. Relative path는 현재 process
directory 기준으로 해석하고, ~로 시작하는 home path는 확장하며, lexical component를 정규화합니다.

지원 동작:

- Text, binary, image, directory read.
- offset과 length를 지원하는 1-based line read. file-lines는 bundled bat.exe를 사용합니다.
- Rewrite와 append write.
- depth, maxEntries, includeFiles, excludePatterns, allowMissing을 지원하는 directory creation/listing. dir-list는 bundled fd.exe를 사용합니다.
- Copy, move, recursive remove, metadata read, exact block replacement.
- redirect handling을 포함한 http:// URL read.

## Search Tools

search-start는 결과를 in-memory session에 저장하고 session id를 반환합니다. search-get은 저장된 결과를
pagination하고, search-stop은 session을 제거합니다.

Search 지원 항목:

- searchType: content 또는 files.
- Regex 또는 literal content search.
- ignoreCase, contextLines, includeHidden, filePattern, maxResults.
- Content search에서 binary file skip.
- content search는 bundled rg.exe를 사용하고 files search는 bundled fd.exe를 사용합니다.
- bundled 실행 파일은 vendor/tools에서 Cargo build 시 target/<profile>/tools로 복사됩니다.
- 추가 bundled utility는 jq.exe, sd.exe, hyperfine.exe, tokei.exe입니다. 현재 public MCP tool이 직접
  dispatch하지는 않습니다.

search-regex는 session 저장 없이 같은 search path를 실행합니다.

## Git Tools

Git tool은 path, pinned git-cwd, current directory에서 repository를 찾습니다. 구현은 repository file을 직접 읽고
씁니다.

구현된 동작:

- optional repository initialization을 지원하는 git-cwd.
- HEAD, index, worktree 비교 기반 git-status.
- index v2 entry와 loose blob object를 쓰는 git-add.
- tree/commit object를 쓰고 HEAD를 갱신하는 git-commit.
- staged, worktree, target, source/target 비교를 지원하는 git-diff.
- commit, tree, blob, revision의 file content를 보여주는 git-show.

Commit message는 English Conventional Commit header로 시작해야 합니다.

## Development

동작 변경 전 다음 check를 우선 실행합니다.

```powershell
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build
```

현재 test suite는 core behavior unit test와 전체 public tool matrix를 검증하는 integration test를 포함합니다.

## Known Limitations

- TLS-capable Rust HTTP client layer가 추가되기 전까지 HTTPS URL read는 거부됩니다.
- Git object access는 loose object를 읽습니다. refs는 packed-refs를 지원하지만 packfile object storage는
  지원하지 않습니다.
- Git index 지원은 version 2입니다.
- Submodule과 rename-aware diff는 구현되어 있지 않습니다.
- Process lifecycle control은 fs-mcp-compatible catalog의 public MCP tool이 아닙니다.
- MCP resources와 resource templates는 현재 empty list를 반환합니다.
