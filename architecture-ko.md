# Architecture

이 문서는 현재 source tree에 구현된 rust-fs-mcp architecture를 설명합니다.

## Goals

rust-fs-mcp 는 네 가지 제약을 기준으로 설계됩니다.

- 공개 fs-mcp tool 이름과 request shape 를 안정적으로 유지합니다.
- 모든 tool result 를 정규화된 response envelope 안에 둡니다.
- 외부 CLI 도구 (rg, fd, git) 는 번들링 대신 PATH 에서 해결하여 release artifact 를 가벼게 유지하고 사용자 설치 toolchain 을 재사용합니다.
- configuration, search session, git cwd 를 process-local 로 명시적으로 유지합니다.

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
5. initialize는 protocol version, capabilities, server info, server instructions를 반환합니다. clientInfo.name에 claude가 포함된 client는 gate형 라우팅 instructions(built-in 우선, batch/정밀 작업만 rust-fs-mcp)를, 그 외 client는 batch-first instructions를 받습니다.
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
| core::external | PATH 에서 해결된 외부 CLI 도구 (rg, fd, git) 를 spawn 하고 timeout 과 stdout/stderr capture 로 실행합니다. |
| core::config | RuntimeConfig (allowedDirectories), home 확장, lexical path 정규화, 내부 cache 를 이용한 path-allowed 검증입니다. |
| core::response | RawResult type, content sanitization, display text, response timing, public envelope normalization입니다. |
| tools::mod | Tool name dispatcher와 cross-tool argument resolution boundary입니다. |
| tools::fs_tools | File, directory, metadata, 정확 block edit (file-edit), 1-based line edit (file-edit-lines), image, HTTP read behavior 입니다. |
| tools::search_tools | PATH의 ripgrep으로 동작하는 content regex search execution입니다. |
| tools::inspect_tools | 코딩 작업용 compact read-only filesystem inspection collection입니다. |
| tools::git_tools | PATH 에서 해결된 git CLI 를 호출하여 repository discovery 와 status/add/commit/diff/show 를 처리합니다. |
| tests::tool_matrix | Public tool surface의 catalog와 dispatch coverage를 검증합니다. |

## State Model

서버는 runtime state를 process memory에 저장합니다.

| State | Owner | Backing type | Lifetime |
| --- | --- | --- | --- |
| Runtime config | core::config | OnceLock<RwLock<ConfigState>> 와 별도의 Mutex<HashMap<PathBuf, bool>> path-allowed cache | Process lifetime |
| Git cwd | tools::git_tools | OnceLock<Mutex<Option<PathBuf>>> | 변경 또는 process exit까지 |

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
- RUST_FS_MCP_ALWAYS_LOAD(기본 file-read,fs-search,file-edit-lines)는 지정 tool에 _meta {"anthropic/alwaysLoad": true}를 표시해 schema 지연 로드 host가 해당 tool을 즉시 노출하게 합니다.

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
- structuredContent.data.content: sanitized content blocks; 결과 본문(파일 내용, 검색 라인, diff, 목록)이 여기 정확히 1회 담깁니다.
- structuredContent.data.structuredContent: sanitized structured 메타데이터 또는 null; 본문을 중복하지 않습니다.
- structuredContent.durationMs: tool duration.
- structuredContent.error: message object이며 실패 시에만 존재합니다.
- compact envelope를 끄면(RUST_FS_MCP_COMPACT=0) data.text, error: null, schemaVersion: 1, status, toolName이 추가됩니다.
- _meta.fsMcpResult: compact status metadata.
- isError: error result일 때만 존재합니다.

Text와 JSON string은 서버 밖으로 나가기 전에 sanitize됩니다.

## Batch Contract

Batch tool은 run_batch와 create_batch_response를 사용합니다. 기본 compact batch layer는 다음 항목을 보존합니다.

- 1-based input index.
- Per-item ok flag.
- Per-item data: tool별 structured 메타데이터이며 null이면 생략됩니다.
- failedCount, succeededCount, totalCount, toolName.

compact envelope를 끄면 각 entry에 verbatim input object와 content, structuredContent, isError를 담은
per-item result wrapper가 복원됩니다.

모든 item이 실패한 경우에만 batch response가 tool error로 표시됩니다.

## Filesystem Architecture

fs_tools는 read, write/directory, copy/move/remove/info/edit, shared helpers, HTTP helpers로 나뉩니다.

중요 contract:

- Local path는 ensure_path_allowed, existing_path, target_path 중 하나를 통과합니다.
- Write는 필요한 parent directory를 생성합니다.
- file-edit 는 exact string replacement 를 수행하며 expected_replacements 를 강제할 수 있습니다.
- file-edit-lines 는 inclusive 1-based line range 를 교체하며 원본 파일의 line ending 을 보존합니다.
- Binary file 은 NUL byte 로 감지합니다.
- Image file은 base64 data를 담은 image content block으로 반환합니다.
- Directory traversal은 depth, maxEntries, includeFiles, excludePatterns, allowMissing을 반영합니다.
- URL read는 http://와 redirect handling을 지원하며 TLS 지원 전까지 https://를 거부합니다.
- file-read-line-range는 1-based 시작 줄을 기준으로 local text range를 native Rust streaming으로 읽습니다.
- dir-list는 native Rust traversal 을 사용하고 excludePatterns 가 주어지면 PATH 의 fd 로 fallback 합니다.

## Search Architecture

fs-search는 item마다 ripgrep 호환 content search를 한 번 실행하고 batch result를 바로 반환합니다.

Backend:

- content search 는 PATH 에서 해결된 ripgrep (rg) 을 실행합니다.
- resolver 는 std::process::Command 로 명령 이름만 전달하므로 rg 가 설치되어 PATH 에 있어야 합니다.
- result structured data 에는 backend label 이 기록됩니다 (예: `path-rg`).

Search behavior:

- text-like file을 읽어 ripgrep 정규식으로 매칭합니다.
- ignoreCase는 case-insensitive matching을 설정합니다.
- contextLines는 grep-like context separator를 출력합니다.
- includeHidden은 dot-path traversal을 제어합니다.
- filePattern은 glob matching으로 대상 file을 좁힙니다.
- maxResults는 반환되는 match line 수를 제한합니다.
- pattern_path로 대용량 pattern을 참조로 전달할 수 있습니다.

## Git Architecture

git_tools는 core::external을 통해 PATH에서 해결된 git CLI를 wrapping합니다. 모든 handler가 argument vector를 만들어
해결된 worktree 안에서 run_git으로 실행합니다.

Repository discovery:

- path argument가 있으면 우선하며, file path면 그 parent directory를 사용합니다.
- 없으면 session git-set-workdir 값을 사용합니다.
- worktree root는 rev-parse --show-toplevel로 확인합니다.

Command behavior:

- git-set-workdir는 worktree를 해결하고 요청 시 git init을 먼저 실행할 수 있으며 이후 호출을 위해 Git working dir를 저장합니다.
- git-add는 주어진 path에 git add를 실행하며, all/update/force 플래그가 설정되면 --all/--update/--force를 추가합니다(all/update는 명시적 pathspec 없이 staging 가능).
- git-commit은 local git config 없이도 commit이 되도록 -c user.name=rust-fs-mcp 와 -c user.email=rust-fs-mcp@example.invalid 를 항상 주입하고, author object가 주어지면 --author를 추가하며, amend, allow-empty, no-verify를 전달합니다.
- git-amend는 HEAD를 다시 씁니다: 기존 commit이 있어야 하며, message가 없으면 --no-edit로 기존 message를 유지하고, 새 message면 Conventional Commit header를 검사하며, author와 reset-author 조합을 거부하고, staged file·allow-empty·no-verify를 전달합니다.
- git-status는 git status --porcelain --branch를 실행합니다.
- git-diff는 staged, name-only, stat, source/target, contextLines(--unified=<n>로 매핑), check(--check로 매핑되어 whitespace 오류와 잔존 conflict marker를 표시), path argument를 선택적으로 적용해 git diff를 실행합니다.
- git-show는 object 또는 objects[] 리비전 집합에 git show를 실행하며, filePath 결합과 stat(diffstat)·format=raw를 지원합니다. 여러 리비전은 한 번의 호출로 처리됩니다.

Validation:

- Commit message는 git 실행 전에 English Conventional Commit header 검사를 통과해야 합니다.
- run_git은 exit 0이면 stdout을, 그렇지 않으면 stderr 기반 error를 반환합니다.

Known git boundaries:

- git binary가 설치되어 PATH에서 해결되어야 하며, in-process git object store는 없습니다.
- 동작과 edge case는 설치된 git version을 따르며 submodule과 rename detection 기본값도 포함합니다.

## Inspect Architecture

inspect_tools는 read-only 코딩 조회를 위한 단일 composite tool인 fs-inspect를 노출합니다. 한 번의 호출이 root와
request 목록을 담고, 각 request는 op에 따라 dispatch됩니다.

Request op:

- count-files는 directory 아래 glob 매칭 수를 세며 optional recursion과 sample path를 제공합니다.
- search는 optional capture-group field extraction과 file pattern filter를 지원하는 regex 또는 literal scan을 실행합니다.
- json-pick은 JSON file을 parse해 요청된 JSON pointer 위치의 값을 반환합니다.
- snippet은 substring 매칭 주변의 context-bounded line range를 반환합니다.
- git-status는 git_tools::handle_git_status에 위임해 filesystem과 git state가 한 round-trip에 해결되게 합니다.

Shared behavior:

- per-call maxSnippetChars budget(기본 6000)이 전체 evidence text를 제한하고 초과 시 truncated 플래그를 설정합니다.
- 각 answer는 id, op, status, value, confidence, evidence, warnings를 담으며, 호출은 scannedFiles, bytesRead, snippetChars, truncated metric도 반환합니다.
- 컴파일된 wildcard pattern은 search cache와 동일하게 process-wide map에 캐싱됩니다.
- RUST_FS_MCP_TOOL_PROFILE=fast-coding은 tools/list를 fs-inspect로만 좁힙니다.

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
