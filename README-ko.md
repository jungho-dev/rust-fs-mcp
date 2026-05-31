# rust-fs-mcp

rust-fs-mcp는 기존 fs-mcp의 공개 tool 계약을 Rust stdio MCP 서버로 이식하는 프로젝트입니다.
stdin/stdout 기반 line JSON-RPC로 filesystem, search, git tool을 제공합니다.

이 프로젝트는 중요한 계약 형태를 유지합니다. 공개 tool 이름, batch-first 입력, args_path 계열 대용량 인자 참조,
정규화된 fs-mcp 응답 envelope를 그대로 유지합니다.

## 현재 상태

- tools/list 가 22개 MCP tool 을 노출하며 tool matrix integration test 가 이를 검증합니다.
- 서버는 initialize, tools/list, tools/call, resources/list, resources/templates/list를 처리합니다.
- filesystem 과 inspection tool 은 native Rust 코드 경로에서 동작합니다.
- search 와 git tool 은 PATH 에서 해결되는 외부 CLI 도구를 wrapping 합니다.
- search, exclude-aware listing, git tool 을 위해 rg, fd, git 이 설치되어 PATH 에서 해결되어야 합니다.
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
| Files and directories | file-read, file-lines, file-write, dir-mk, dir-list |
| File mutation and metadata | file-copy, file-move, file-remove, file-infos, file-edit, file-edit-lines |
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
| RUST_FS_MCP_TOOL_PROFILE | 선택 process env profile입니다. fast-coding을 사용하면 tools/list에 fs-inspect만 노출합니다. |
| RUST_FS_MCP_COMPACT | 기본 on입니다. client token 절약을 위해 content block을 복제하는 data.text를 제거합니다. 0 또는 false면 data.text를 복원합니다. |
| RUST_FS_MCP_READ_MAX_CHARS | 전체 파일 file-read 문자 한도입니다(기본 100000). 초과 시 truncated 플래그와 함께 잘리며 offset/length로 이어 읽습니다. 0이면 비활성화합니다. |
| RUST_FS_MCP_BATCH_WORKERS | per-process batch worker 수를 선택적으로 제한합니다. 양의 정수면 동시성을 제한하고, 미설정 또는 무효 값이면 available parallelism(없으면 4)으로 fallback합니다. |

allowedDirectories 는 RUST_FS_MCP_ALLOWED_DIRECTORIES 환경 변수로 초기화할 수 있습니다. 값은 platform path-list separator 를 사용합니다.

## Response Envelope

모든 tool call은 같은 envelope로 정규화됩니다.

- content는 MCP client 표시용 text를 담습니다.
- structuredContent.data.content는 정규화된 content block을 담습니다; 파일 본문이 여기 있습니다.
- structuredContent.data.structuredContent는 tool별 structured data를 담습니다; read는 메타데이터만이며 파일 본문을 더 이상 중복하지 않습니다.
- structuredContent.data.text는 data.content를 복제하며 RUST_FS_MCP_COMPACT를 끈 경우에만 제공됩니다.
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
| src/core/external.rs | PATH 에서 해결된 외부 CLI 도구 (rg, fd, git) 를 timeout과 stdout/stderr capture 로 실행하는 wrapper 입니다. |
| src/core/config.rs | RuntimeConfig (allowedDirectories), path normalization, home 확장, lexical normalization, path-allowed cache 를 포함하는 allowedDirectories 경계 검증입니다. |
| src/core/response.rs | RawResult, display text, sanitization, timing, envelope normalization입니다. |
| src/tools/fs_tools.rs | file, directory, metadata, 정확 block edit (file-edit), 1-based line edit (file-edit-lines), image, HTTP read tool 입니다. |
| src/tools/search_tools.rs | regex, literal, context, pagination을 지원하는 file/content search session입니다. |
| src/tools/inspect_tools.rs | 코딩 작업용 compact read-only filesystem inspection request를 처리합니다. |
| src/tools/git_tools.rs | PATH 에서 해결된 git CLI 를 wrapping 하는 git cwd, status, add, commit, diff, show 입니다. |
| tests/tool_matrix.rs | catalog tool 전체가 dispatch를 통해 호출 가능한지 검증하는 integration check입니다. |

자세한 request flow와 module contract는 ARCHITECTURE-ko.md를 참조하세요.

## Filesystem Tools

Filesystem tool은 읽기/쓰기 전에 runtime config boundary로 path를 검증합니다. Relative path는 현재 process
directory 기준으로 해석하고, ~로 시작하는 home path는 확장하며, lexical component를 정규화합니다.

지원 동작:

- Text, binary, image, directory read.
- offset과 length를 지원하는 1-based line read. file-lines는 native Rust streaming 을 사용합니다.
- Rewrite와 append write.
- depth, maxEntries, includeFiles, excludePatterns, allowMissing을 지원하는 directory creation/listing. dir-list는 native Rust traversal 을 사용하고 excludePatterns 가 주어지면 PATH 의 fd 로 fallback합니다.
- Copy, move, recursive remove, metadata read, 정확 block replacement (file-edit), 1-based line-range replacement (file-edit-lines).
- redirect handling을 포함한 http:// URL read.

## Search Tools

search-start는 결과를 in-memory session에 저장하고 session id를 반환합니다. search-get은 저장된 결과를
pagination하고, search-stop은 session을 제거합니다.

Search 지원 항목:

- searchType: content 또는 files.
- Regex 또는 literal content search.
- ignoreCase, contextLines, includeHidden, filePattern, maxResults.
- Content search에서 binary file skip.
- content search 는 ripgrep (rg) 을, files search 는 fd 를 실행합니다. 두 도구 모두 PATH 에서 해결되므로 설치되어 있어야 합니다.
- 내부 ExternalTool enum 은 정확히 세 개의 PATH-resolved binary(rg, fd, git)를 wrapping 합니다.

search-regex는 session 저장 없이 같은 search path를 실행합니다.

## Git Tools

Git tool 은 path 또는 pinned git-cwd 에서 repository 를 찾은 뒤, 해결된 worktree 안에서 PATH 의 git CLI 를 호출합니다.

구현된 동작:

- rev-parse 로 worktree 를 해결하고 요청 시 git init 을 먼저 실행할 수 있는 git-cwd.
- status --porcelain --branch 를 실행하고 porcelain line 을 반환하는 git-status.
- git add 로 path 를 stage 하는 git-add.
- local git config 없이도 commit 되도록 기본 committer identity(user.name=rust-fs-mcp, user.email=rust-fs-mcp@example.invalid)를 주입하고, optional author override 를 받으며, amend 와 allow-empty 를 지원하는 git-commit.
- staged, name-only, stat, source/target, path filter 를 선택적으로 적용해 git diff 를 실행하는 git-diff.
- object 또는 object:path 를 git show 로 렌더링하는 git-show.

Commit message는 English Conventional Commit header로 시작해야 합니다.

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

RUST_FS_MCP_TOOL_PROFILE=fast-coding은 tools/list를 fs-inspect로만 제한합니다.

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
- Git tool은 PATH의 git binary를 필요로 하며, in-process git object store는 없습니다.
- Git 동작은 설치된 git CLI를 따르며, submodule과 rename detection 기본값도 그대로 따릅니다.
- MCP resources와 resource templates는 현재 empty list를 반환합니다.
