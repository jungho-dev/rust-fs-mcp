# Architecture

이 문서는 현재 source tree에 구현된 rust-fs-mcp architecture를 설명합니다.

## Goals

rust-fs-mcp는 네 가지 제약을 기준으로 설계됩니다.

- 공개 fs-mcp tool 이름과 request shape를 안정적으로 유지합니다.
- 모든 tool result를 정규화된 response envelope 안에 둡니다.
- search, listing, packaged utility용 project-owned CLI 실행 파일을 Cargo build output에 포함합니다.
- configuration, search session, git cwd, process session state를 process-local로 명시적으로 유지합니다.

## High-Level Flow

```text
MCP client
  |
  | line JSON-RPC over stdin/stdout
  v
src/main.rs
  |
  v
protocol::server::run
  |
  +-- initialize ------------------> server metadata and capabilities
  +-- tools/list ------------------> protocol::catalog::tool_catalog
  +-- tools/call ------------------> tools::dispatch_tool_call
  +-- resources/list --------------> empty list
  +-- resources/templates/list ----> empty list
```

tools/call request는 tool name과 optional JSON arguments를 들고 dispatch_tool_call로 들어갑니다. Dispatcher는
args_path reference를 먼저 해석하고, matching tool handler를 호출한 뒤 RawResult를 public response envelope로
정규화합니다.

## Request Lifecycle

1. src/main.rs가 rust_fs_mcp::server::run을 호출합니다.
2. protocol::server가 stdin을 line 단위로 읽습니다.
3. 비어 있지 않은 각 line을 JSON-RPC로 parse합니다.
4. notification method는 response를 반환하지 않습니다.
5. initialize는 protocol version, capabilities, server info, server instructions를 반환합니다.
6. tools/list는 catalog entry와 input schema를 반환합니다.
7. tools/call은 params.name과 params.arguments를 추출합니다.
8. tools::dispatch_tool_call은 args_path, args_offset, args_length를 해석합니다.
9. concrete tool handler가 RawResult를 반환합니다.
10. core::response::normalize_tool_result가 MCP content, structuredContent, _meta, isError field를 만듭니다.

## Module Responsibilities

| Module | 역할 |
| --- | --- |
| main | Binary entry point와 fatal error handling입니다. |
| lib | core, protocol, tools module을 re-export합니다. |
| protocol::server | JSON-RPC line protocol, method routing, initialize response, empty resource handler입니다. |
| protocol::catalog | Public tool registry, tool description, annotation, JSON schema입니다. |
| core::args_ref | args_path와 optional character slicing 기반 large argument indirection입니다. |
| core::batch | Shared batch execution과 structured batch result format입니다. |
| core::bundled | project-bundled CLI 실행 파일을 resolve하고 timeout과 함께 실행합니다. |
| core::config | Runtime configuration, path resolution, allowedDirectories enforcement, blocked command lookup입니다. |
| core::response | RawResult type, content sanitization, display text, response timing, public envelope normalization입니다. |
| tools::mod | Tool name dispatcher와 cross-tool argument resolution boundary입니다. |
| tools::fs_tools | File, directory, metadata, edit, image, HTTP read behavior입니다. |
| tools::search_tools | Search execution, in-memory search session, pagination, regex/literal matching입니다. |
| tools::inspect_tools | 코딩 작업용 compact read-only filesystem inspection collection입니다. |
| tools::git_tools | git CLI 없이 지원 범위의 repository discovery, status/add/commit/diff/show를 처리합니다. |
| tools::process_tools | 내부 managed process helper입니다. Public catalog에는 process tool을 노출하지 않습니다. |
| tests::tool_matrix | Public tool surface의 catalog와 dispatch coverage를 검증합니다. |

## State Model

서버는 runtime state를 process memory에 저장합니다.

| State | Owner | Backing type | Lifetime |
| --- | --- | --- | --- |
| Runtime config | core::config | OnceLock<Mutex<RuntimeConfig>> | Process lifetime |
| Search sessions | tools::search_tools | OnceLock<Mutex<HashMap<String, SearchSession>>> | search-stop 또는 process exit까지 |
| Git cwd | tools::git_tools | OnceLock<Mutex<Option<PathBuf>>> | 변경 또는 process exit까지 |
| Process sessions | tools::process_tools | OnceLock<Mutex<HashMap<i64, ProcSession>>> | process exit, kill, server exit까지 |

Tool call이 요청한 filesystem/git write를 제외하면 서버는 state를 별도로 persist하지 않습니다.

## Configuration Boundary

core::config는 공유 path 및 process-safety boundary입니다.

Path handling:

- ~는 USERPROFILE 또는 HOME 기준으로 확장합니다.
- Relative path는 process current directory에 결합합니다.
- Lexical component를 정규화합니다.
- Path resolution 뒤 allowedDirectories를 검사합니다.
- allowedDirectories가 비어 있으면 local path access를 제한하지 않습니다.
- target_path는 parent directory boundary도 검사합니다.
- RUST_FS_MCP_TOOL_PROFILE=fast-coding은 tools/list를 fs-inspect로 제한하며 dispatch 호환성은 유지합니다.

Internal process command handling:

- Lifecycle helper는 첫 command token을 blockedCommands와 비교합니다.
- 해당 helper를 내부에서 사용할 때 cwd, command_path, input_path는 allowedDirectories를 통과해야 합니다.
- 이 lifecycle helper는 public MCP tool로 export하지 않습니다.

## Tool Dispatch Boundary

tools::dispatch_tool_call은 protocol layer에서 들어오는 유일한 public tool execution entry입니다.

세 단계로 동작합니다.

1. Start time을 기록합니다.
2. Inline override를 포함해 top-level args_path reference를 해석합니다.
3. Resolved Value를 named handler로 route하고 결과를 normalize합니다.

알 수 없는 tool name은 error RawResult를 반환합니다. 이 경우도 normal response envelope를 사용합니다.

## Response Contract

Tool handler는 RawResult를 반환합니다. RawResult는 의도적으로 작습니다.

| Field | 목적 |
| --- | --- |
| content | Normalization 전 MCP content block입니다. |
| structured | Optional tool-specific structured data입니다. |
| is_error | Tool-level error flag입니다. |
| meta | Reserved per-tool metadata map입니다. |

normalize_tool_result는 public contract를 생성합니다.

- content: display-oriented text block.
- structuredContent.data.content: sanitized content blocks.
- structuredContent.data.structuredContent: sanitized structured data 또는 null.
- structuredContent.durationMs: tool duration.
- structuredContent.error: null 또는 message object.
- structuredContent.schemaVersion: 1.
- structuredContent.status: success 또는 error.
- structuredContent.toolName: original tool name.
- _meta.fsMcpResult: compact status metadata.
- isError: error result일 때만 존재합니다.

Text와 JSON string은 서버 밖으로 나가기 전에 sanitize됩니다.

## Batch Contract

Batch tool은 run_batch와 create_batch_response를 사용합니다. Batch layer는 다음 항목을 보존합니다.

- 1-based input index.
- Original input object.
- Per-item ok flag.
- Per-item RawResult content, structuredContent, isError.
- failedCount, succeededCount, totalCount, toolName.

모든 item이 실패한 경우에만 batch response가 tool error로 표시됩니다.

## Filesystem Architecture

fs_tools는 read, write/directory, copy/move/remove/info/edit, shared helpers, HTTP helpers로 나뉩니다.

중요 contract:

- Local path는 ensure_path_allowed, existing_path, target_path 중 하나를 통과합니다.
- Write는 필요한 parent directory를 생성합니다.
- file-edit는 exact string replacement를 수행하며 expected_replacements를 강제할 수 있습니다.
- Binary file은 NUL byte로 감지합니다.
- Image file은 base64 data를 담은 image content block으로 반환합니다.
- Directory traversal은 depth, maxEntries, includeFiles, excludePatterns, allowMissing을 반영합니다.
- URL read는 http://와 redirect handling을 지원하며 TLS 지원 전까지 https://를 거부합니다.
- file-lines는 native Rust streaming으로 line range를 읽습니다.
- dir-list는 native Rust traversal을 사용하고 excludePatterns가 있으면 bundled fd.exe로 fallback합니다.

## Search Architecture

search_tools는 session mode와 backend mode를 분리합니다.

search-start와 search-regex는 같은 search engine을 실행합니다. search-start는 result line을 generated session id에
저장하고, search-regex는 batch result를 바로 반환합니다. search-get은 저장된 line을 offset/length로
pagination합니다. search-stop은 session id를 제거합니다.

Backend selection:

- content search는 project-bundled rg.exe를 실행합니다.
- files search는 project-bundled fd.exe를 실행합니다.
- resolver는 PATH에 의존하지 않고 target/<profile>/tools와 vendor/tools fallback 위치만 확인합니다.
- result structured data에는 backend가 포함됩니다.
- 추가 packaged utility는 future internal wrapper용 jq.exe, sd.exe, hyperfine.exe, tokei.exe입니다.

Search behavior:

- searchType files는 file path만 반환합니다.
- content search는 text-like file을 읽고 RegexBuilder를 적용합니다.
- literalSearch는 compile 전에 pattern을 escape합니다.
- ignoreCase는 case-insensitive matching을 설정합니다.
- contextLines는 grep-like context separator를 출력합니다.
- includeHidden은 dot-path traversal을 제어합니다.
- filePattern은 file name 또는 displayed path에 wildcard matching을 적용합니다.
- maxResults는 traversal을 조기 종료합니다.

## Git Architecture

git_tools는 지원 command surface를 위한 compact git backend를 구현합니다.

Repository discovery:

- path argument가 있으면 우선합니다.
- 없으면 pinned git-cwd를 사용합니다.
- 없으면 current directory를 사용합니다.
- Discovery는 .git을 찾을 때까지 상위 directory로 이동합니다.
- gitdir: 형식의 .git file을 지원합니다.

Object and index handling:

- Blob, tree, commit object는 loose zlib-compressed object로 씁니다.
- Object id는 git object header와 data에 대한 SHA-1입니다.
- Index reader/writer는 git index version 2를 지원합니다.
- Reference는 loose ref 또는 packed-refs에서 읽습니다.
- Object prefix resolution은 loose object를 검색합니다.

Command behavior:

- git-cwd는 요청 시 basic repository initialization을 수행할 수 있습니다.
- git-add는 blob을 쓰고 index를 갱신합니다.
- git-commit은 tree/commit을 쓰고 HEAD를 갱신합니다.
- git-status는 HEAD, index, worktree map을 비교합니다.
- git-diff는 index/worktree, HEAD/index, target/worktree, source/target map을 비교합니다.
- git-show는 commit, tree, blob, revision:path content를 렌더링합니다.

Known git boundaries:

- Packfile object storage는 구현되어 있지 않습니다.
- Submodule handling은 구현되어 있지 않습니다.
- Rename-aware diff는 구현되어 있지 않습니다.
- Diff output은 simple whole-file patch/stat generation이며 full git diff algorithm은 아닙니다.

## Process Architecture

현재 catalog에서 process operation은 public MCP tool로 export하지 않습니다. process_tools는 runtime integration을 위해
internal lifecycle helper를 유지할 수 있지만 process control은 tools/list나 dispatch_tool_call로 노출하지 않습니다.

Internal start flow:

1. command 또는 command_path를 읽습니다.
2. 첫 token을 blockedCommands와 비교합니다.
3. optional cwd를 allowedDirectories로 검증합니다.
4. Shell invocation argument를 구성합니다.
5. stdin, stdout, stderr를 pipe로 둔 child를 spawn합니다.
6. stdout/stderr reader thread를 시작합니다.
7. pid 기준으로 ProcSession을 저장합니다.
8. timeout_ms에 따라 initial output을 짧게 기다립니다.

Session behavior:

- stdout과 stderr는 하나의 shared output buffer에 append됩니다.
- stderr line은 stderr: prefix를 붙입니다.
- Input interaction, lifecycle listing, output read, kill behavior는 future public catalog가 의도적으로 추가하기 전까지 내부 detail입니다.

서버는 unmanaged PID를 거부합니다. 해당 process의 stdin, stdout, stderr, lifecycle을 소유하지 않기 때문입니다.

## Tool Catalog Architecture

protocol::catalog는 exposed tool name, description, schema, annotation의 source of truth입니다. tools::mod의
dispatcher는 catalog와 항상 일치해야 합니다.

Tool matrix test는 다음 방식으로 정합성을 강제합니다.

1. tools/list를 호출하고 catalog name을 수집합니다.
2. dispatch_tool_call을 통해 모든 public tool을 호출합니다.
3. 호출된 name을 sort/deduplicate합니다.
4. 호출된 name과 catalog를 비교합니다.

새 public tool은 catalog, dispatcher, test를 함께 갱신해야 합니다.

## Validation Strategy

Changed surface를 덮는 가장 좁은 command를 사용합니다.

```powershell
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build
```

Documentation-only change에서는 Markdown heading hierarchy, fenced code block language, local link, trailing whitespace도
확인합니다.
