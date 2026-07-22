# rust-fs-mcp

rust-fs-mcp는 기존 fs-mcp의 공개 tool 계약을 Rust stdio MCP 서버로 이식하는 프로젝트입니다.
stdin/stdout 기반 line JSON-RPC로 filesystem, search, git tool을 제공합니다.

이 프로젝트는 중요한 계약 형태를 유지합니다. 공개 tool 이름, batch-first 입력, args_path 계열 대용량 인자 참조,
정규화된 fs-mcp 응답 envelope를 그대로 유지합니다.

## 현재 상태

- tools/list 가 23개 MCP tool 을 노출하며 tool matrix integration test 가 이를 검증합니다.
- 서버는 initialize, tools/list, tools/call, resources/list, resources/templates/list를 처리합니다.
- filesystem 과 inspection tool 은 native Rust 코드 경로에서 동작합니다.
- content search 는 ripgrep 자체 라이브러리(grep-searcher + ignore)로 in-process 동작하므로 rg 바이너리가 필요 없습니다.
- git tool 은 PATH 에서 해결되는 외부 git CLI 를 wrapping 합니다.
- exclude-aware listing 과 git tool 을 위해 fd, git 이 설치되어 PATH 에서 해결되어야 합니다.
- web-fetch, web-extract, download-to-file 은 tokio 없는 native HTTPS client(ureq)로 동작하며, web-render 는 JS/SPA rendering 을 위해 별도로 설치된 obscura 계열 headless-browser CLI 로 optional 하게 shell-out 합니다.
- resources는 현재 비어 있습니다. 현재 범위는 tool parity 우선입니다.

## 설치

태그된 release마다 사전 빌드된 binary가 제공됩니다.
[최신 release 페이지](https://github.com/jungho-dev/rust-fs-mcp/releases/latest)에서 자신의 platform에 맞는
asset을 받거나, 다음 URL pattern을 직접 사용할 수 있습니다.

```
https://github.com/jungho-dev/rust-fs-mcp/releases/download/<tag>/rust-fs-mcp-<target>.zip
```

| OS | Architecture | Target triple |
| --- | --- | --- |
| Windows | x86_64 | `x86_64-pc-windows-msvc` |
| Windows | aarch64 | `aarch64-pc-windows-msvc` |
| macOS | x86_64 (Intel) | `x86_64-apple-darwin` |
| macOS | aarch64 (Apple Silicon) | `aarch64-apple-darwin` |
| Linux | x86_64 | `x86_64-unknown-linux-gnu` |
| Linux | aarch64 | `aarch64-unknown-linux-gnu` |

각 archive에는 동일 이름의 `<asset>.sha256sum` 파일이 함께 제공됩니다. 소스 tarball
`rust-fs-mcp_src.tar.gz`도 모든 release에 첨부됩니다.

압축 해제 전 무결성 검증:

```bash
# Unix
shasum -a 256 -c rust-fs-mcp-x86_64-unknown-linux-gnu.zip.sha256sum
```

```powershell
# Windows PowerShell
(Get-FileHash -Algorithm SHA256 .\rust-fs-mcp-x86_64-pc-windows-msvc.zip).Hash
# .sha256sum 파일 내용과 비교
```

Release 는 `.github/workflows/release.yml` 이 다음 세 종류의 event 를 단일 `resolve` job 으로 통합해 자동 생성합니다.

- `git push origin main` 시 `Cargo.toml` 의 `version` 을 읽어 origin 에 `v<version>` tag 가 아직 없으면 tag 자동 생성 + release 발행합니다. 동명 tag 가 이미 있으면 아무 작업도 하지 않습니다.
- `git push origin v<X.Y.Z>` 는 해당 tag 를 그대로 release 로 발행합니다.
- `workflow_dispatch` 와 `tag` 입력은 해당 tag 로 수동 release 를 발행합니다.

자동 tag 경로에서는 `github-actions[bot]` 명의로 tag 를 push 하며 기본 `GITHUB_TOKEN` 을 사용하므로 추가 secret 은 필요 없습니다.

## 소스에서 빌드

```powershell
cargo build --release
# binary 위치: target/release/rust-fs-mcp (Windows에서는 rust-fs-mcp.exe)
```

특정 target을 로컬에서 cross-build하려면 target을 설치한 뒤 `--target`을 지정합니다.

```powershell
rustup target add aarch64-apple-darwin
cargo build --release --target aarch64-apple-darwin
```

## Tool Surface

| 영역 | Tools |
| --- | --- |
| Files and directories | file-read, file-read-line-range, file-write, dir-create, dir-list |
| Path mutation and metadata | path-copy, path-move, path-remove, path-stat, file-edit, file-edit-lines |
| Search | fs-search |
| Git | git-status, git-add, git-commit, git-amend, git-diff, git-show |
| Inspect | fs-inspect |
| Web | web-fetch, web-render, web-extract, download-to-file |

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

dispatcher는 tool 실행 전에 흔한 argument shape 실수를 흡수합니다: flat 단일 연산을 `items[]`로 래핑하고, key alias를 정규화하며(`file_path`->`path`, `from`/`to`->`source`/`destination`), `path-remove`와 metadata tool은 `items[]`뿐 아니라 단순 `paths[]` 배열도 받고, JSON 문자열로 마샬된 배열 argument는 다시 배열로 parse합니다.

## 고정 동작

runtime 동작은 프로젝트 환경변수나 process 전역 설정으로 변경되지 않습니다.

- 항상 전체 23-tool catalog를 노출하고, core file/search/edit/git read tool에는 고정 always-load annotation을 유지합니다.
- 응답은 항상 compact envelope를 사용합니다: `{data, durationMs}`와 실패 시의 `error`만 포함합니다.
- 전체 파일 읽기는 80,000자로 고정 제한됩니다. 더 큰 파일은 `offset`, `length`로 나눠 읽습니다.
- 모든 응답의 `{data, durationMs}` payload에는 직렬화 기준 약 88,000 byte의 고정 예산이 적용됩니다: 초과 본문은 `[truncated: ...]` 안내와 함께 잘리고 batch structured tail은 `resultsDropped`로 보고되므로, MCP output-token 한도가 있는 client(예: Claude Code 기본 25,000 token)가 결과를 거부하지 않습니다.
- batch plan은 고정 workload 한도(read 3/8, stat 4/16, search 2/2, fetch 2/32, download 2/16)를 사용합니다.
- `fs-inspect` 내부 deadline은 25초로 고정됩니다.
- local filesystem path는 allowed-root 정책 없이 해석합니다. Git tool은 매 호출에 명시적 `path`가 필요합니다.
- SSRF guard는 loopback(localhost/127.0.0.0/8/::1)을 제외한 non-public address를 차단하며(로컬 개발 서버 접근을 위해 loopback 허용), `web-render`는 항상 `evalScript`를 거부합니다.
- `obscura`를 포함한 외부 CLI는 PATH에서 해결합니다.

## Response Envelope

모든 tool call은 같은 envelope로 정규화됩니다.

- content는 MCP client 표시용 text를 담습니다.
- structuredContent.data.content는 정규화된 content block을 담습니다; 결과 본문(파일 내용, 검색 라인, diff, 목록)이 여기 정확히 1회 담깁니다.
- structuredContent.data.structuredContent는 tool별 structured 메타데이터(count, path, backend)만 담으며 본문을 중복하지 않습니다.
- structuredContent.durationMs는 tool duration입니다.
- structuredContent.error는 실패 시에만 {message}로 제공됩니다.
- compact envelope는 항상 사용하며 성공 응답에서 data.text, error:null, schemaVersion, status, toolName을 생략합니다.
- _meta.fsMcpResult는 status, duration, content type, structured-content 존재 여부를 반복 제공합니다.
- 고정 output budget을 초과하는 결과는 server-side에서 잘리며, `_meta.fsMcpResult.outputTruncated`가 설정되고 display text에 `truncated = true`가 표시됩니다.
- tool 실패 시 isError가 설정됩니다.

Batch tool은 per-item {index, ok, data} entry와 succeededCount, failedCount, totalCount를 반환합니다.
full envelope에서는 per-item {index, input, ok, result} entry와 verbatim request echo가 복원됩니다.

## Module Layout

| Path | 책임 |
| --- | --- |
| src/main.rs | binary entry point입니다. stdio MCP server를 실행하고 fatal startup error에서 non-zero로 종료합니다. |
| src/lib.rs | 안정적인 내부 호출 경로를 유지하는 public module export입니다. |
| src/protocol/server.rs | line JSON-RPC 처리, initialize response, tool call, empty resource 응답입니다. |
| src/protocol/catalog.rs | MCP tool catalog, tool annotation, JSON input schema입니다. |
| src/core/args_ref.rs | args_path, args_offset, args_length 기반 대용량 JSON argument 해석입니다. |
| src/core/batch.rs | 순차·pooled-parallel·mutation-safe batch 실행과 결과 shape, per-item summary입니다. |
| src/core/external.rs | PATH 에서 해결된 외부 CLI 도구 (fd, git) 를 timeout과 stdout/stderr capture 로 실행하는 wrapper 입니다. |
| src/core/config.rs | path normalization, home 확장, lexical normalization, 직접 path 해석입니다. |
| src/core/response.rs | RawResult, display text, timing, envelope normalization, body truncation을 포함한 고정 output budget, 그리고 (현재 passthrough 상태인) sanitizer seam입니다. |
| src/core/web.rs | tokio 없는 blocking HTTPS fetch(ureq), per-hop SSRF guard, body-size cap, HTML extraction(html2text, htmd, scraper, dom_smoothie)입니다. |
| src/tools/fs_tools.rs | file, directory, metadata, 정확 block edit (file-edit), 1-based line edit (file-edit-lines), image, file-read isUrl(core::web로 위임) tool 입니다. |
| src/tools/search_tools.rs | grep-searcher + ignore 로 in-process 동작하는 content regex search 입니다 (backend `native-grep`). |
| src/tools/inspect_tools.rs | 코딩 작업용 compact read-only filesystem inspection request를 처리합니다. |
| src/tools/git_tools.rs | PATH 에서 해결된 git CLI 를 wrapping 하는 git cwd, status, add, commit, amend, diff, show 입니다. |
| src/tools/web_tools.rs | web-fetch, web-render, web-extract, download-to-file handler입니다. |
| tests/tool_matrix.rs | catalog tool 전체가 dispatch를 통해 호출 가능한지 검증하는 integration check입니다. |

자세한 request flow와 module contract는 ARCHITECTURE-ko.md를 참조하세요.

## Filesystem Tools

Filesystem tool은 읽기/쓰기 전에 runtime config boundary로 path를 검증합니다. Relative path는 현재 process
directory 기준으로 해석하고, ~로 시작하는 home path는 확장하며, lexical component를 정규화합니다.

지원 동작:

- Text, binary, image, directory read.
- 1-based start_line과 optional line_count를 지원하는 local text line-range read. file-read-line-range는 native Rust streaming을 사용합니다.
- Rewrite와 append write.
- depth, maxEntries, includeFiles, excludePatterns, noDefaultExcludes, allowMissing을 지원하는 directory creation/listing. dir-list는 native Rust traversal 을 사용하며 대상 경로가 그 내부가 아닌 한 node_modules/, target/, .git/ 을 기본 숨김 처리하고, noDefaultExcludes: true 로 다시 나열합니다.
- Copy, move, recursive remove, metadata read, 정확 block replacement (file-edit, 빈 old_string 은 거부), 1-based line-range replacement (file-edit-lines, 마지막 줄의 개행 부재를 포함해 원본 line ending 을 보존).
- file-read isUrl: true 는 공유 core::web client로 HTTP/HTTPS URL을 읽습니다: per-hop SSRF guard, redirect handling, body-size cap을 포함합니다. 전용 web-fetch/web-render/web-extract/download-to-file tool은 아래 Web Tools를 참조하세요.

## Search Tools

fs-search는 ripgrep 호환 정규식 content search를 실행하고 batch 결과를 바로 반환합니다.

Search 지원 항목:

- ignoreCase, contextLines, includeHidden, filePattern, maxResults.
- literal(고정 문자열 검색, rg -F), wordMatch(단어 경계 매치, rg -w), multiline(패턴이 줄 경계를 넘고 . 이 개행을 매치, rg -U).
- Pattern flavor 는 Rust regex 입니다: linear-time engine 이고 \d \w \b 가 Unicode-aware 라 한글 단어 경계도 동작합니다. look-around 와 backreference 는 재작성 힌트와 함께 거절되고, 단순 문법 오류만 리터럴 검색으로 1회 폴백하며 backend label 에 파스 오류 요지가 남습니다.
- Content search에서 binary file skip.
- 대용량 pattern을 위한 pattern_path indirection과 대상 file을 좁히는 filePattern.
- content search 는 grep-searcher + ignore(ripgrep 자체 라이브러리)로 in-process 동작하므로 rg 설치가 필요 없으며, structured result 에 backend `native-grep` 이 기록됩니다.

## Git Tools

Git tool은 모든 호출에 `path`가 필요하며, 해결된 worktree 안에서 PATH의 git CLI를 호출합니다.

구현된 동작:

- status --porcelain --branch 를 실행하고 porcelain line 을 반환하는 git-status.
- git add 로 path 를 stage 하며 all(--all), update(--update), force(--force) 를 전달하는 git-add. all 과 update 는 명시적 pathspec 없이 변경을 stage 합니다.
- local git config 없이도 commit 되도록 기본 committer identity(user.name=rust-fs-mcp, user.email=rust-fs-mcp@example.invalid)를 주입하고, optional author override 를 받으며, amend, allow-empty, no-verify 를 지원하는 git-commit.
- 마지막 commit 을 다시 쓰는 git-amend. message 가 없으면 --no-edit 로 기존 message 를 유지하고, 새 message 면 Conventional Commit header 를 검사하며, author override 또는 reset-author(상호 배타), 파일 staging, allow-empty, no-verify 를 지원합니다.
- staged, name-only, stat, source/target, contextLines(--unified=<n> 로 매핑), check(--check 로 매핑되어 whitespace 오류와 잔존 conflict marker 를 표시), path filter 를 선택적으로 적용해 git diff 를 실행하는 git-diff.
- object 또는 object:path 를 git show 로 렌더링하는 git-show.
- git-diff(source/target)와 git-show(object/objects)는 - 로 시작하는 revision 값을 거부하므로, revision 이 --output 같은 git 옵션으로 해석될 수 없습니다.

Commit message는 Conventional Commit header(소문자 영문 type, 요약부는 한글 등 언어 무관)로 시작해야 합니다.

## Inspect Tool

fs-inspect는 directory tree에 대한 여러 read-only 질문을 한 번의 batch 호출로 답합니다. root와 request 목록을 받아
request마다 status, confidence, evidence snippet, 집계 metric을 담은 answer를 하나씩 반환합니다. 공유 maxSnippetChars
budget(기본 6000)이 evidence text를 제한해 큰 scan에서도 token 사용을 묶어 둡니다.

지원 request op:

- count-files: glob에 매칭되는 file 수를 세며 optional recursion과 sample path를 제공합니다.
- search: optional field extraction과 per-file pattern filter를 지원하는 regex 또는 literal content search입니다.
- json-pick: JSON file을 읽어 주어진 JSON pointer 위치의 값을 반환합니다.
- snippet: 주어진 pattern 중 하나라도 포함하는 line 주변의 context-bounded snippet을 반환합니다.
- git-status: git-status 조회를 같은 호출에 접어 넣어 read, search, git state가 한 round-trip에 해결되게 합니다.

count-files 와 search 의 directory traversal 은 symlink 나 Windows junction 을 따라가지 않으므로 reparse-point 순환이 무한 재귀를 일으키지 않습니다. 또한 request path 가 그 내부가 아닌 한 node_modules/target(및 .git)을 기본 제외하며, request 에 noDefaultExcludes: true 를 주면 순회에 포함합니다.

## Web Tools

Web tier는 static content를 위한 native tier와 JS-rendered page를 위한 외부 headless-browser tier로 구성된 two-tier 설계입니다.

- web-fetch (TIER-1): async runtime 없는 native ureq blocking HTTPS client입니다. items[] 또는 단일 url을 batch로 받고, html, text, markdown, links, readability(본문 추출) 중 하나로 dump합니다.
- web-render (TIER-2): JavaScript/SPA page를 위해 PATH에서 해결되는 obscura 계열 headless-browser CLI로 shell-out합니다. selector, wait, waitUntil, stealth를 지원합니다. `evalScript`는 비활성화됩니다. web-fetch를 먼저 시도하고 JS 실행이 필요할 때만 web-render로 escalate하세요.
- web-extract: 이미 보유한 HTML(inline 또는 local file)을 text, markdown, links, readability로 변환합니다. 완전히 offline 으로 동작합니다.
- download-to-file: URL을 요청한 해석된 local path로 다운로드합니다.

SSRF guard: web-fetch, download-to-file, file-read isUrl은 host를 resolve하여 private, link-local, unique-local, CGNAT, multicast/reserved, IPv4-embedded IPv6 주소(mapped, compatible, NAT64, 6to4)를 거부하며 redirect의 모든 hop마다 다시 검사합니다. loopback(localhost/127.0.0.0/8/::1)은 로컬 개발 서버 접근을 위해 허용하되, loopback을 내장한 IPv4-embedded 형태는 계속 차단합니다. 검증된 IP는 연결 resolver에 그대로 고정되어 DNS rebinding으로 우회할 수 없습니다. web-render는 guard를 우회할 수 있는 `evalScript`를 거부합니다.

Body size는 request당 제한되며(maxBytes, fetch 기본 5,000,000, download 기본 50,000,000) 요청 값과 무관하게 200,000,000 byte로 hard-clamp됩니다.

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

- Git tool은 PATH의 git binary를 필요로 하며, in-process git object store는 없습니다.
- Git 동작은 설치된 git CLI를 따르며, submodule과 rename detection 기본값도 그대로 따릅니다.
- web-render는 별도로 설치된 obscura 계열 headless-browser binary가 필요합니다; 없으면 JS-rendered page를 가져올 수 없습니다.
- SSRF guard는 요청 시점에 resolve된 address만 검사합니다; resolve와 connect 사이에 DNS 응답이 바뀌는 DNS rebinding은 방어하지 않습니다.
- MCP resources와 resource templates는 현재 empty list를 반환합니다.
