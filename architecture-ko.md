# Architecture

이 문서는 현재 source tree에 구현된 rust-fs-mcp architecture를 설명합니다.

## Goals

rust-fs-mcp 는 다섯 가지 제약을 기준으로 설계됩니다.

- 공개 fs-mcp tool 이름과 request shape 를 안정적으로 유지합니다.
- 모든 tool result 를 정규화된 response envelope 안에 둡니다.
- 외부 CLI 도구 (rg, fd, git, obscura) 는 번들링 대신 PATH 또는 지정 경로에서 해결하여 release artifact 를 가벼게 유지하고 사용자 설치 toolchain 을 재사용합니다.
- configuration, search session, git cwd 를 process-local 로 명시적으로 유지합니다.
- web fetch tier 는 tokio 없이 synchronous 하게 유지하며, JavaScript rendering 은 browser engine 을 내장하는 대신 외부 headless-browser CLI 에 위임합니다.

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
4. id가 없는 request(notification)는 response를 반환하지 않으며, id가 있는 request는 항상 response를 받습니다. 잘못된 형식이거나 UTF-8이 아닌 입력 줄은 서버를 종료시키지 않고 JSON-RPC parse error를 반환합니다.
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
| core::batch | Shared batch execution(순차, workload별 plan 기반 pooled-parallel, mutation 충돌 분석)과 structured batch result format입니다. |
| core::external | PATH에서 해결된 외부 CLI 도구 (rg, fd, git, obscura)를 spawn하고 timeout과 stdout/stderr capture로 실행합니다. |
| core::config | home 확장, lexical path 정규화, 직접 path 해석입니다. |
| core::response | RawResult type, display text, response timing, public envelope normalization, 그리고 (현재 passthrough 상태인) sanitizer seam입니다. |
| core::web | tokio 없는 blocking HTTPS fetch(ureq), per-hop SSRF guard, body-size cap, HTML extraction(html2text, htmd, scraper, dom_smoothie)입니다. |
| tools::mod | Tool name dispatcher와 cross-tool argument resolution boundary입니다. |
| tools::fs_tools | File, directory, metadata, 정확 block edit (file-edit), 1-based line edit (file-edit-lines), image, file-read isUrl(core::web로 위임) behavior 입니다. |
| tools::search_tools | PATH의 ripgrep으로 동작하는 content regex search execution입니다. |
| tools::inspect_tools | 코딩 작업용 compact read-only filesystem inspection collection입니다. |
| tools::git_tools | PATH 에서 해결된 git CLI 를 호출하여 repository discovery 와 status/add/commit/diff/show 를 처리합니다. |
| tools::web_tools | web-fetch(native), web-render(obscura shell-out), web-extract(offline HTML conversion), download-to-file(sandboxed download) handler입니다. |
| tests::tool_matrix | Public tool surface의 catalog와 dispatch coverage를 검증합니다. |

## 고정 런타임 모델

서버는 프로젝트 전역 runtime configuration이나 session workdir state를 두지 않습니다. filesystem path는 직접 해석하고, 모든 Git 호출은 각각 `path`가 필요합니다.

## Path Boundary

core::config는 allowed-root 정책 없이 path를 해석합니다.

- ~는 USERPROFILE 또는 HOME 기준으로 확장합니다.
- Relative path는 process current directory에 결합합니다.
- Lexical component를 정규화합니다.
- `target_path`는 parent allow-list 검사 없이 target path를 해석합니다.
- 전체 catalog와 always-load annotation은 고정됩니다.

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
- compact envelope는 고정되며 성공 응답에서 data.text, error:null, schemaVersion, status, toolName을 생략합니다.
- _meta.fsMcpResult: compact status metadata.
- isError: error result일 때만 존재합니다.

core::response의 sanitizer 함수들은 seam으로 유지되지만 현재는 text와 JSON을 변경 없이
통과시킵니다(end-token 재작성을 무력화한 go-fs-mcp와 동일한 계약).

## Batch Contract

Batch tool은 run_batch, run_batch_parallel, run_batch_mutation, create_batch_response를 사용합니다.
Read 계열 tool은 lazy 상주 worker pool에서 atomic work cursor로 실행되며 caller thread가 항상
참여하므로 요청마다 OS thread를 spawn하지 않습니다. workload별 plan이 병렬화를 gate하고
(read 3/8, stat 4/16, search 2/2, fetch 2/32, download 2/16)를 사용합니다. Mutation은 모든 item이 증명 가능하게 독립인 경로를 만질
때만 병렬(2/4)로 실행됩니다 - 동일 경로, 조상/자손 관계, 알 수 없는 형태, 256개 초과는 순차
runner로 폴백합니다. 기본 compact batch layer는 다음 항목을 보존합니다.

- 1-based input index.
- Per-item ok flag.
- Per-item data: tool별 structured 메타데이터이며 null이면 생략됩니다.
- failedCount, succeededCount, totalCount, toolName.

compact batch entry는 verbatim input object나 per-item result wrapper를 보관하지 않습니다.

모든 item이 실패한 경우에만 batch response가 tool error로 표시됩니다.

## Filesystem Architecture

fs_tools는 read, write/directory, copy/move/remove/info/edit, shared helpers, HTTP helpers로 나뉩니다.

중요 contract:

- Local path는 ensure_path_allowed, existing_path, target_path 중 하나를 통과합니다.
- Write는 필요한 parent directory를 생성합니다.
- file-edit 는 exact string replacement 를 수행하고 expected_replacements 를 강제할 수 있으며 빈 old_string 은 거부합니다.
- file-edit-lines 는 inclusive 1-based line range 를 교체하며 마지막 줄의 후행 개행 부재를 포함해 원본 파일의 line ending 을 보존합니다.
- Binary file 은 NUL byte 로 감지합니다.
- Image file은 base64 data를 담은 image content block으로 반환합니다.
- Directory traversal은 depth, maxEntries, includeFiles, excludePatterns, allowMissing을 반영합니다.
- URL read(isUrl: true)는 core::web::http_fetch로 위임됩니다: HTTP와 HTTPS, per-hop SSRF guard, redirect handling, body-size cap을 포함합니다. 자세한 내용은 아래 Web Architecture를 참조하세요.
- file-read-line-range는 1-based 시작 줄을 기준으로 local text range를 native Rust streaming으로 읽습니다.
- dir-list는 native Rust traversal만 사용하며 excludePatterns는 내장 compiled wildcard set으로 매칭합니다.

## Search Architecture

fs-search는 item마다 ripgrep 호환 content search를 한 번 실행하고 batch result를 바로 반환합니다.
rg stdout은 줄 단위 스트림으로 디코드됩니다: maxResults 도달 시 남은 트리 스캔 대신 rg process를
즉시 종료하고, reader가 블록된 동안의 timeout_ms는 watchdog thread가 kill로 강제합니다.

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
- 모든 Git tool 호출에 path가 필요합니다.
- worktree root는 rev-parse --show-toplevel로 확인합니다.

Command behavior:

- git-add는 주어진 path에 git add를 실행하며, all/update/force 플래그가 설정되면 --all/--update/--force를 추가합니다(all/update는 명시적 pathspec 없이 staging 가능).
- git-commit은 local git config 없이도 commit이 되도록 -c user.name=rust-fs-mcp 와 -c user.email=rust-fs-mcp@example.invalid 를 항상 주입하고, author object가 주어지면 --author를 추가하며, amend, allow-empty, no-verify를 전달합니다.
- git-amend는 HEAD를 다시 씁니다: 기존 commit이 있어야 하며, message가 없으면 --no-edit로 기존 message를 유지하고, 새 message면 Conventional Commit header를 검사하며, author와 reset-author 조합을 거부하고, staged file·allow-empty·no-verify를 전달합니다.
- git-status는 git status --porcelain --branch를 실행합니다.
- git-diff는 staged, name-only, stat, source/target, contextLines(--unified=<n>로 매핑), check(--check로 매핑되어 whitespace 오류와 잔존 conflict marker를 표시), path argument를 선택적으로 적용해 git diff를 실행합니다.
- git-show는 object 또는 objects[] 리비전 집합에 git show를 실행하며, filePath 결합과 stat(diffstat)·format=raw를 지원합니다. 여러 리비전은 한 번의 호출로 처리됩니다.

Validation:

- Commit message는 git 실행 전에 Conventional Commit header 검사(소문자 영문 type, 요약부 언어 무관)를 통과해야 합니다.
- git-diff(source/target)와 git-show(object/objects)는 git 실행 전에 - 로 시작하는 revision 값을 거부하므로, revision 이 --output 같은 git 옵션으로 해석될 수 없습니다.
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
- count-files 와 search 의 directory traversal 은 symlink 와 Windows junction 을 건너뛰어 reparse-point 순환이 무한 재귀를 일으키지 않습니다.
## Web Architecture

web_tools와 core::web는 two-tier fetch 설계를 구현합니다: static/API content를 위한 native TIER-1 경로와 JavaScript-rendered content를 위한 외부 CLI TIER-2 경로입니다.

TIER-1 (web-fetch, download-to-file, file-read isUrl):

- ureq는 async runtime이 없는 blocking HTTP/1.1 client입니다; TLS는 rustls입니다.
- http_fetch는 단일 redirect loop를 소유합니다: ureq agent의 max_redirects(0)과 http_status_as_error(false)가 모든 3xx를 caller에게 돌려주므로, ureq 자체의 redirect 처리를 신뢰하는 대신 각 hop을 따라가기 전에 SSRF guard로 다시 검증합니다.
- Agent는 (scheme | host:port | 검증된 IP set) key로 32개 FIFO 상한 cache에 보관되어 같은 host 반복 요청이 TCP/TLS 세션을 재사용합니다; timeout은 요청 단위 config로 주입되고, DNS 응답이 바뀌면 key가 바뀝니다.
- 하나의 wall-clock deadline이 전체 redirect chain에 걸쳐 유지됩니다(hop마다 timeout을 새로 주지 않습니다).
- Response body는 ureq의 .limit()으로 읽으며, 실제 cap은 min(caller maxBytes, MAX_ALLOWED_BYTES)(200,000,000 byte)로, caller가 요청한 값과 무관하게 적용됩니다.
- download-to-file은 body를 대상 directory의 temp file로 스트림한 뒤 rename으로 옮기므로, 부분 다운로드가 대상 경로에 남지 않고 대용량 body가 메모리에 버퍼링되지 않습니다.

SSRF guard (core::web의 ensure_url_allowed / is_public_ip):

- http:// 와 https:// scheme만 허용합니다.
- host를 실제 IP address로 resolve하며, resolve된 모든 address가 public이어야 합니다.
- IPv4: loopback, private, link-local, broadcast, documentation, unspecified, CGNAT(100.64.0.0/10), "this network"(0.0.0.0/8), multicast/reserved(>= 224.0.0.0/4)를 거부합니다.
- IPv6: loopback, unspecified, multicast, unique-local(fc00::/7), link-local(fe80::/10)을 거부합니다. IPv4 대상을 내장하는 주소 형태 - IPv4-mapped(::ffff:a.b.c.d), deprecated IPv4-compatible(::a.b.c.d), NAT64(64:ff9b::/96), 6to4(2002::/16) - 는 내장된 IPv4 address로 정규화된 뒤 다시 검사되므로, loopback/private 대상을 guard 뒤로 밀반입할 수 없습니다.
- Guard는 redirect의 매 hop마다 실행되며, 최초 URL에만 적용되지 않습니다.
- guard는 항상 활성화됩니다.
- 수용된 잔여 위험: guard는 요청 시점에 resolve된 address만 검사합니다; 검사와 TCP connect 사이에 DNS 응답이 바뀌는 DNS rebinding은 방어하지 않습니다.

TIER-2 (web-render):

- Navigation과 DNS를 core::external::ExternalTool::Obscura를 통해 PATH에서 해결되는 외부 obscura 계열 headless-browser CLI에 위임합니다.
- url argument는 process spawn 전에 여전히 ensure_url_allowed를 통과합니다.
- evalScript는 in-browser request로 URL-level guard를 우회할 수 있어 거부됩니다.

HTML extraction (web-extract, 그리고 web-fetch/web-render의 html이 아닌 dump mode):

- html2text는 plain wrapped text를 렌더링합니다.
- htmd는 markdown을 렌더링합니다.
- scraper는 link를 추출하고 중복을 제거하며, 상대 href 값을 page URL 기준으로 resolve합니다.
- dom_smoothie는 Readability 방식의 본문(title, byline, text, content HTML)을 추출합니다.

download-to-file은 fetch한 body를 해석된 target_path에 씁니다. web-fetch의 기본 body cap(5,000,000 byte)과는 별도로 자체 기본 cap(50,000,000 byte)을 가지며, 둘 다 동일한 200,000,000 byte hard ceiling으로 clamp됩니다.

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
