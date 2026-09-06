# rust-fs-mcp

AI 에이전트에게 로컬 **파일, 검색, git, 웹**을 위한 빠른 batch-first 도구 모음을 제공하는 native Rust stdio
[MCP](https://modelcontextprotocol.io) 서버입니다. stdin/stdout 기반 JSON-RPC로 통신하며, 의존성 없는 단일
바이너리로 배포됩니다.

도구 이름과 요청 형태는 공개 fs-mcp 계약을 따르므로, 해당 표면을 이미 이해하는 MCP client에 그대로 연결됩니다.

## 왜 rust-fs-mcp인가

- **단일 바이너리, 사실상 무의존성.** Node, Python, `rg` runtime이 필요 없습니다. git tool에 쓰는 PATH의
  `git`과, 선택적으로 `web-render`에 쓰는 headless-browser CLI만 외부 요소입니다.
- **23개 도구**: 파일, 디렉터리, path 연산, content 검색, git, filesystem inspection, 웹 fetch.
- **Batch-first.** 같은 종류의 연산은 `items[]`(또는 `paths[]`) 배열로 받아 pooled parallel executor에서
  실행하므로, 에이전트가 여러 대상을 한 번의 호출로 읽거나 편집합니다.
- **In-process 검색.** content 검색은 ripgrep 자체 라이브러리(grep-searcher + ignore)를 쓰므로 `rg` 바이너리가
  필요 없습니다.
- **안전한 웹 접근.** tokio 없는 HTTPS client가 per-hop SSRF guard 뒤에서 URL을 가져옵니다.
- **예측 가능한 출력.** 모든 결과가 하나의 compact envelope를 쓰며, client의 output-token 한도 아래에 머물도록
  크기가 제한됩니다.

## 빠른 시작

1. [설치](#설치)(또는 [소스에서 빌드](#소스에서-빌드))에서 바이너리를 준비합니다.
2. MCP client에 등록합니다. 서버는 **인자를 받지 않고** stdin/stdout으로 통신합니다. 대부분의 client(Claude Code,
   Claude Desktop 등)는 다음 형태를 씁니다.

   ```json
   {
     "mcpServers": {
       "rust-fs-mcp": {
         "command": "/absolute/path/to/rust-fs-mcp"
       }
     }
   }
   ```

   Windows에서는 `command`를 `rust-fs-mcp.exe`의 전체 경로로 지정합니다(JSON에서 backslash를 escape하거나
   forward slash 사용).

   ```json
   {
     "mcpServers": {
       "rust-fs-mcp": {
         "command": "C:/tools/rust-fs-mcp/rust-fs-mcp.exe"
       }
     }
   }
   ```

3. client를 재시작합니다. 23개 도구가 `tools/list`에 나타납니다.

서버는 MCP protocol version을 자동 협상하므로(`2024-11-05`, `2025-03-26`, `2025-06-18` 지원) client에서 version을
설정할 필요가 없습니다.

## 도구

| 영역 | 도구 |
| --- | --- |
| 파일과 디렉터리 | `file-read`, `file-read-line-range`, `file-write`, `dir-create`, `dir-list` |
| Path 연산과 메타데이터 | `path-copy`, `path-move`, `path-remove`, `path-stat`, `file-edit`, `file-edit-lines` |
| 검색 | `fs-search` |
| Git | `git-status`, `git-add`, `git-commit`, `git-amend`, `git-diff`, `git-show` |
| Inspect | `fs-inspect` |
| Web | `web-fetch`, `web-render`, `web-extract`, `download-to-file` |

읽기 전용 도구(`file-read`, `file-read-line-range`, `dir-list`, `fs-search`, `path-stat`, `fs-inspect`,
`git-status`, `git-diff`, `git-show`, `web-fetch`, `web-render`, `web-extract`)에는 MCP `readOnlyHint`가
붙습니다. 변경 도구(`file-write`, `file-edit`, `file-edit-lines`, `path-move`, `path-remove`, `git-commit`,
`git-amend`, `download-to-file`)에는 `destructiveHint`가 붙습니다.

## 사용 핵심

**Batch-first 입력.** 같은 종류의 대상은 한 번의 호출에 모두 담습니다.

```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
  "name":"file-read",
  "arguments":{"paths":["src/main.rs","src/lib.rs","Cargo.toml"]}
}}
```

읽기 batch는 3-8개 파일 정도로 유지하고 더 큰 집합은 나눕니다. 초과 항목은 항목 단위로 잘릴 뿐 hard-reject되지
않습니다.

**관대한 인자.** dispatcher는 도구 실행 전에 흔한 shape 실수를 흡수합니다: flat 단일 연산을 `items[]`로 래핑하고,
key alias를 정규화하며(`file_path`->`path`, `from`/`to`->`source`/`destination`), `path-remove`와 metadata
도구는 `items[]`뿐 아니라 단순 `paths[]` 배열도 받고, JSON 문자열로 인코딩된 배열은 다시 배열로 parse합니다.

**경로.** 절대 경로를 권장합니다. 상대 경로는 process 작업 디렉터리 기준으로 해석하고, 선행 `~`는 home 디렉터리로
확장합니다. allowed-root 제한은 없으며, git 도구는 매 호출에 명시적 `path`가 필요합니다.

**대용량 인자.** 모든 도구는 `{"args_path":"/abs/path/to/args.json"}`(optional `args_offset`/`args_length`)로
파일에서 인자를 읽을 수 있어, 큰 payload를 JSON-RPC 한 줄 밖으로 뺄 수 있습니다.

## 응답 Envelope

모든 호출은 하나의 compact envelope로 정규화됩니다: `{data, durationMs}`, 실패 시에만 `error` 추가.

- `data.content`는 결과 본문(파일 내용, 검색 라인, diff, 목록)을 정확히 1회 담습니다.
- `data.structuredContent`는 도구별 메타데이터(count, path, backend)만 담으며 본문을 중복하지 않습니다.
- `durationMs`는 도구의 wall-clock 소요 시간입니다.
- `_meta.fsMcpResult`는 status, duration, content type, structured content 존재 여부를 반복 제공합니다.
- `isError`는 도구 실패 시 설정됩니다.

Batch 도구는 per-item `{index, ok, data}` entry와 `succeededCount`, `failedCount`, `totalCount`를 추가합니다.

**출력 크기.** 결과는 client의 MCP output-token 한도(예: Claude Code 기본 25,000 token) 아래에 머물도록
server-side에서 `MAX_STANDARD_BYTES`(30,000 byte)로 제한됩니다. 초과 본문은 client로 넘쳐 흐르지 않고 안내와
`outputTruncated`와 함께 그 자리에서 잘립니다. 전체 fidelity가 필요하면 더 작은 slice를 요청하세요:
`offset`/`length`, `maxResults`, 또는 더 적은 batch 항목.

## 설치

사전 빌드된 바이너리가 모든 [release](https://github.com/jungho-dev/rust-fs-mcp/releases/latest)에 첨부됩니다.
자신의 platform에 맞는 archive를 받거나, 다음 URL pattern을 직접 사용합니다.

```text
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

각 archive에는 동일 이름의 `<asset>.sha256sum`이 함께 제공됩니다. 압축 해제 전 검증하세요.

```bash
# Unix
shasum -a 256 -c rust-fs-mcp-x86_64-unknown-linux-gnu.zip.sha256sum
```

```powershell
# Windows PowerShell
(Get-FileHash -Algorithm SHA256 .\rust-fs-mcp-x86_64-pc-windows-msvc.zip).Hash
# .sha256sum 파일 내용과 비교
```

## 소스에서 빌드

Rust 1.85+ (edition 2024)가 필요합니다.

```powershell
cargo build --release
# binary 위치: target/release/rust-fs-mcp (Windows에서는 rust-fs-mcp.exe)
```

특정 target을 cross-build하려면 target을 설치하고 `--target`을 지정합니다.

```powershell
rustup target add aarch64-apple-darwin
cargo build --release --target aarch64-apple-darwin
```

## 도구 레퍼런스

### 파일과 디렉터리

- `file-read`는 text, binary, image, directory 대상을 병렬로 읽습니다. 각 항목은 optional `offset`/`length`
  slice를 받고, `isUrl: true`는 공유 web client로 HTTP/HTTPS URL을 가져옵니다([Web](#web) 참조).
- `file-read-line-range`는 1-based line range(`start_line`, optional `line_count`)를 line 번호와 함께 native
  streaming으로 반환합니다.
- `file-write`는 rewrite 또는 append(`mode`)합니다. 큰 content는 `content_path`로 받을 수 있습니다.
- `dir-create`는 하나 또는 여러 디렉터리를 생성합니다.
- `dir-list`는 `depth`, `maxEntries`, `includeFiles`, `excludePatterns`, `allowMissing`로 디렉터리를
  나열합니다. 대상 경로가 그 내부가 아닌 한 `node_modules/`, `target/`, `.git/`을 기본 숨김 처리하며,
  `noDefaultExcludes: true`로 함께 나열합니다.

### Path 연산과 메타데이터

- `path-copy`, `path-move`, `path-remove`는 `recursive`/`force` 플래그로 복사, 이동/이름변경, 삭제합니다.
  독립적인 경로는 병렬로 실행하고, 겹치는 경로는 순차 runner로 fallback합니다.
- `path-stat`은 여러 경로의 메타데이터를 한 번에 반환합니다.
- `file-edit`는 정확한 block 치환(`old_string` -> `new_string`)을 적용하고, `expected_replacements`를 강제할 수
  있으며, 빈 `old_string`을 거부합니다.
- `file-edit-lines`는 1-based line 번호로 치환, 삽입, 삭제하며 파일의 원본 line ending(마지막 줄 개행 부재
  포함)을 보존합니다.

### 검색

`fs-search`는 ripgrep 호환 regex content 검색을 in-process로 실행하고 batch 결과를 바로 반환합니다.

- 플래그: `ignoreCase`, `literal`(고정 문자열, `rg -F`), `wordMatch`(`rg -w`), `multiline`(`rg -U`),
  `contextLines`, `includeHidden`, `filePattern`, `maxResults`.
- Pattern flavor는 Rust regex입니다: linear-time engine이고 `\d \w \b`가 Unicode-aware라 한글 단어 경계도
  동작합니다. look-around와 backreference는 backtracking engine(fancy-regex, backtrack limit과 검색 timeout이
  상한)으로 자동 전환되며, 단순 문법 오류는 리터럴 검색으로 1회 fallback합니다.
- binary 파일은 skip합니다. 큰 pattern은 `pattern_path`로 받을 수 있습니다. backend label은 structured
  result에 기록됩니다(예: `native-grep`).

### Git

모든 git 도구는 `path`가 필요하며, 그 worktree 안에서 PATH의 git CLI를 실행합니다.

- `git-status`는 `status --porcelain --branch`를 실행합니다.
- `git-add`는 `all`/`update`/`force`로 path를 stage합니다.
- `git-commit`은 local git config 없이도 commit되도록 기본 committer identity를 주입하고, optional author
  override를 받으며, `amend`/`allow-empty`/`no-verify`를 지원합니다. message는 Conventional Commit
  header(소문자 영문 type, 요약부는 언어 무관)로 시작해야 합니다.
- `git-amend`는 마지막 commit을 다시 씁니다: message가 없으면 `--no-edit`, 있으면 검증된 새 message를 쓰며,
  author override 또는 reset-author, staging, `allow-empty`, `no-verify`를 지원합니다.
- `git-diff`는 `staged`, `nameOnly`, `stat`, `source`/`target`, `contextLines`, `check`, path filter로
  `git diff`를 실행합니다.
- `git-show`는 object(또는 `objects[]`)와 optional `object:path`를 `git show`로 렌더링합니다.

`-`로 시작하는 revision 입력은 거부되므로, revision이 git 옵션으로 해석될 수 없습니다.

### Inspect

`fs-inspect`는 directory tree에 대한 여러 read-only 질문을 한 번의 batch 호출로 답합니다. `root`와 request
목록을 받아 request마다 status, confidence, evidence snippet, 집계 metric을 담은 answer를 하나씩 반환합니다.
공유 `maxSnippetChars` budget(기본 6000)이 evidence를 token-bounded로 유지합니다.

Request op: `count-files`, `search`, `json-pick`(JSON pointer 위치 값), `snippet`, `git-status`(filesystem과
git state가 한 round-trip에 해결되도록 접어 넣음). traversal은 symlink나 Windows junction을 따라가지 않습니다.

### Web

static content를 위한 native tier와 JavaScript-rendered page를 위한 외부 headless-browser tier로 구성된
two-tier 설계입니다.

- `web-fetch`(native): browser 없이 하나 또는 여러 URL을 HTTP/HTTPS로 가져와 `markdown`(기본), `text`,
  `links`, `readability`(본문), raw `html` 중 하나로 dump합니다. 먼저 이것을 시도하세요.
- `web-render`(외부): JS/SPA page를 위해 PATH에서 해결되는 obscura 계열 headless-browser CLI로 하나의 URL을
  렌더링합니다. `selector`, `wait`, `waitUntil`, `stealth`를 지원합니다. page에 JS 실행이 필요할 때만 여기로
  escalate하세요.
- `web-extract`: 이미 보유한 HTML(inline 또는 local file)을 markdown, text, links, readability로 완전히
  offline 변환합니다.
- `download-to-file`: 하나 또는 여러 URL을 temp file로 stream한 뒤 rename하여 local file로 다운로드합니다.

**SSRF guard.** `web-fetch`, `download-to-file`, `file-read isUrl`은 host를 resolve하여 non-public
address(private, link-local, unique-local, CGNAT, multicast/reserved, IPv4-embedded IPv6 형태)를 거부하며
redirect의 모든 hop마다 다시 검사합니다. loopback(`localhost`/`127.0.0.0/8`/`::1`)은 로컬 개발을 위해
허용됩니다. 검증된 address는 연결 resolver에 고정됩니다. `web-render`는 in-browser 요청이 guard를 우회할 수
있으므로 `evalScript`를 거부합니다.

Body size는 fetch와 download 모두 200,000,000-byte hard ceiling이 기본이며, 명시적 `maxBytes`로 낮출 수만
있습니다.

## 요구 사항과 한계

- git 도구에는 PATH의 `git`이 필요합니다. in-process git object store는 없으며, 동작은 설치된 git
  version(submodule, rename-detection 기본값 포함)을 따릅니다.
- `web-render`에는 별도로 설치된 obscura 계열 headless-browser 바이너리가 필요합니다. 없으면 JS-rendered page를
  가져올 수 없습니다.
- SSRF guard는 요청 시점의 resolve된 address만 검사합니다. resolve와 TCP connect 사이의 DNS rebinding은
  방어하지 않습니다.
- MCP resources와 resource templates는 현재 empty list를 반환합니다. 이 프로젝트는 도구에 집중합니다.

## 개발

```powershell
cargo test
cargo clippy --all-targets
cargo build
```

test suite는 core behavior unit test와, 모든 catalog 도구가 dispatch를 통해 호출 가능한지 검증하는 integration
test(`tests/tool_matrix.rs`)를 포함합니다.

request flow와 내부 계약의 module별 상세는 [architecture-ko.md](architecture-ko.md)를 참조하세요. 이 문서의 영어
원문은 [readme.md](readme.md)에 있습니다.

## License

Apache-2.0. [LICENSE.md](LICENSE.md) 참조.