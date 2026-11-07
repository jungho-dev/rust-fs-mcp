# rust-fs-mcp

## 개요

1. **소개**
    - AI 에이전트에게 로컬 파일, 검색, git, 웹 작업용 batch-first 도구 모음을 제공하는 native Rust stdio [MCP](https://modelcontextprotocol.io) 서버
    - stdin/stdout 기반 JSON-RPC로 통신하며 단일 실행 파일로 배포
    - 도구 이름과 요청 형태가 공개 fs-mcp 계약을 따름
2. **지원 에이전트**
    - 지원하는 MCP 호스트는 Claude Code, OpenAI Codex, GitHub Copilot(VS Code agent mode, Copilot CLI), Gemini CLI, OpenCode 5종뿐임
    - 호스트마다 `docs/<agent>/FS.md` 라우팅 가이드가 있으며, 그 밖의 MCP 클라이언트는 연결될 수는 있어도 지원 대상이 아님
3. **특징**
    - Node, Python, `rg` 런타임 불필요, git 도구용 PATH의 `git`만 외부 의존
    - 파일, 디렉터리, 경로, 내용 검색, git, 웹에 걸친 21개 도구
    - Batch-first 입력: 같은 종류의 작업을 `items[]` 또는 `paths[]` 배열로 받아 병렬 실행
    - ripgrep 자체 라이브러리(`grep-searcher`, `ignore`)로 프로세스 안에서 내용 검색
    - tokio 없는 HTTPS 클라이언트와 hop마다 검사하는 SSRF guard
    - 클라이언트 출력 토큰 한도 아래로 크기를 제한한 단일 compact 응답 envelope
4. **문서**
    - 영어 원문: [README.md](README.md)
    - 소스 트리: [ARCHITECTURE-KO.md](ARCHITECTURE-KO.md)
    - 에이전트 지침 파일용 라우팅 가이드: [Claude Code](docs/claude/FS.md), [Codex](docs/codex/FS.md), [GitHub Copilot](docs/copilot/FS.md), [Gemini CLI](docs/gemini/FS.md), [OpenCode](docs/opencode/FS.md)
    - 라이선스: [LICENSE.md](LICENSE.md)

---

## 빠른 시작

### 1. 다운로드

1. **릴리스 아카이브**
    - [최신 릴리스](https://github.com/jungho-dev/rust-fs-mcp/releases/latest)에서 플랫폼에 맞는 아카이브를 받음
    - 직접 URL 형식: `https://github.com/jungho-dev/rust-fs-mcp/releases/download/<tag>/rust-fs-mcp-<target>.zip`
2. **Windows 타깃**
    - `x86_64-pc-windows-msvc`
    - `aarch64-pc-windows-msvc`
3. **macOS 타깃**
    - `x86_64-apple-darwin` (Intel)
    - `aarch64-apple-darwin` (Apple Silicon)
4. **Linux 타깃**
    - `x86_64-unknown-linux-gnu`
    - `aarch64-unknown-linux-gnu`

### 2. 검증

1. **체크섬**
    - 아카이브마다 같은 이름의 `<archive>.sha256sum` 파일이 함께 올라감
    - 압축을 풀기 전에 해시를 확인함

```bash
# macOS, Linux
shasum -a 256 -c rust-fs-mcp-x86_64-unknown-linux-gnu.zip.sha256sum
```

```powershell
# Windows PowerShell: 해시가 같으면 True 출력
$zip = ".\rust-fs-mcp-x86_64-pc-windows-msvc.zip"
(Get-FileHash -Algorithm SHA256 $zip).Hash -eq (Get-Content "$zip.sha256sum" -Raw).Split(" ")[0]
```

### 3. 등록

1. **서버 명령**
    - 서버는 인자를 받지 않으므로 클라이언트에는 실행 파일의 절대 경로만 지정함
    - Windows에서는 `rust-fs-mcp.exe`를 가리키고, JSON 안에서는 forward slash를 쓰거나 backslash를 escape함
2. **JSON 설정**
    - Claude Code, Claude Desktop 등 대부분의 MCP 클라이언트가 이 `mcpServers` 형태를 받음

```json
{
  "mcpServers": {
    "rust-fs-mcp": {
      "command": "/absolute/path/to/rust-fs-mcp"
    }
  }
}
```

### 4. Claude Code 등록

1. **CLI 명령**
    - `claude mcp add`로 JSON 편집 없이 등록 가능, `--scope user`면 모든 프로젝트에서 사용

```bash
claude mcp add --scope user rust-fs-mcp -- /absolute/path/to/rust-fs-mcp
```

### 5. 확인

1. **재시작**
    - 클라이언트를 재시작하면 21개 도구가 `tools/list`에 나타남
2. **프로토콜 버전**
    - `2024-11-05`, `2025-03-26`, `2025-06-18` 중에서 자동 협상하므로 클라이언트에 버전 설정 불필요
    - `ping`은 빈 결과를 반환하고, `logging/setLevel`은 MCP 표준 로그 레벨을 받음
3. **에이전트 라우팅**
    - 에이전트별 라우팅 가이드를 지침 파일에서 불러오면, 에이전트가 내장 도구보다 이 도구를 먼저 씀
        1) Claude Code: [docs/claude/FS.md](docs/claude/FS.md), `CLAUDE.md`에서 import
        2) Codex: [docs/codex/FS.md](docs/codex/FS.md), `AGENTS.md`에 붙여 넣음
        3) GitHub Copilot: [docs/copilot/FS.md](docs/copilot/FS.md), VS Code는 `.github/copilot-instructions.md` 또는 `*.instructions.md` 파일에 붙여 넣고, Copilot CLI는 `@FS.md`로 import
        4) Gemini CLI: [docs/gemini/FS.md](docs/gemini/FS.md), `GEMINI.md`에서 import
        5) OpenCode: [docs/opencode/FS.md](docs/opencode/FS.md), `opencode.json`의 `instructions` 배열에 등록

---

## 도구

### 1. 목록

1. **파일과 디렉터리**
    - `file-read`, `file-read-line-range`, `file-write`, `dir-create`, `dir-list`
2. **경로와 편집**
    - `path-copy`, `path-move`, `path-remove`, `path-stat`, `file-edit`, `file-edit-lines`
3. **검색**
    - `fs-search`
4. **Git**
    - `git-status`, `git-add`, `git-commit`, `git-amend`, `git-diff`, `git-show`
5. **웹**
    - `web-fetch`, `web-extract`, `download-to-file`
6. **Annotation**
    - 모든 도구는 사람이 읽는 `title`(예: `Stage Git Files`)을 최상위와 `annotations.title`에 함께 가짐
    - `readOnlyHint: true`: `file-read`, `file-read-line-range`, `dir-list`, `fs-search`, `path-stat`, `git-status`, `git-diff`, `git-show`, `web-fetch`, `web-extract`
    - `destructiveHint: true`: `file-write`, `file-edit`, `file-edit-lines`, `path-move`, `path-remove`, `git-add`, `git-commit`, `git-amend`, `download-to-file`
    - `destructiveHint: false`: `dir-create`, `path-copy`
    - `git-add`는 `all: true`가 삭제까지 stage하고 경로 지정 add가 일부만 stage한 hunk를 덮어쓰므로 파괴적 도구로 분류
    - `openWorldHint: true`: `file-read`(`isUrl` 읽기), `web-fetch`, `download-to-file`, 나머지 도구는 모두 `false`

### 2. 파일과 디렉터리

1. **file-read**
    - 텍스트, 바이너리, 이미지, 디렉터리 대상을 병렬로 읽음
    - 항목마다 선택적 `offset`/`length` 구간 지정 가능
    - `isUrl: true`면 공유 웹 클라이언트로 HTTP/HTTPS URL을 가져옴
2. **file-read-line-range**
    - 1-based 줄 범위(`start_line`, 선택 `line_count`)를 줄 번호와 함께 스트리밍으로 반환
3. **file-write**
    - 덮어쓰기 또는 이어쓰기(`mode`), 큰 내용은 `content_path`로 전달 가능
4. **dir-create**
    - 디렉터리 하나 또는 여러 개 생성
5. **dir-list**
    - `depth`, `maxEntries`, `includeFiles`, `excludePatterns`, `allowMissing`로 디렉터리 나열
    - 대상 경로가 그 안이 아니면 `node_modules/`, `target/`, `.git/`을 기본으로 숨기고, `noDefaultExcludes: true`면 함께 나열
6. **레거시 인코딩**
    - 읽기와 편집은 UTF-16(BOM), EUC-KR/CP949를 자동 디코딩하고 편집 결과를 원래 인코딩으로 다시 씀
    - 감지한 인코딩은 `encoding`으로 보고

### 3. 경로와 편집

1. **path-copy, path-move, path-remove**
    - `recursive`/`force` 플래그로 복사, 이동·이름 변경, 삭제
    - 서로 독립된 경로는 병렬로, 겹치는 경로는 순차로 실행
2. **path-stat**
    - 여러 경로의 메타데이터를 한 번에 반환
3. **file-edit**
    - 정확한 블록 치환(`old_string` → `new_string`) 적용
    - `old_string`이 정확히 `expected_replacements`회(기본 1) 일치해야 하며, 아니면 일치한 줄 번호와 함께 실패하고 아무것도 바꾸지 않음
    - 빈 `old_string`은 거부
4. **file-edit-lines**
    - 1-based 줄 번호로 치환, 삽입, 삭제
    - 마지막 줄 개행 유무를 포함해 원본 줄바꿈 형식 유지

### 4. 검색

1. **fs-search**
    - ripgrep 호환 정규식 내용 검색을 프로세스 안에서 실행하고 batch 결과를 바로 반환
2. **플래그**
    - `ignoreCase`, `literal`(`rg -F`), `wordMatch`(`rg -w`), `multiline`(`rg -U`), `contextLines`, `includeHidden`, `filePattern`, `maxResults`, `maxColumns`, `respectGitignore`, `encoding`
3. **기본값**
    - `ignoreCase`를 생략하면 smart-case
    - 저장소 `.gitignore`를 따르며, `respectGitignore: false`면 무시 대상 파일도 검색
    - `maxColumns`(500)보다 긴 줄은 일치 위치 주변만 남김
4. **결과**
    - 경로순 정렬
    - 응답 예산에 닿으면 수집을 멈추고 `limitReached: true` 표시
    - 시간 초과 시 그때까지 찾은 결과를 `timedOut: true`와 함께 반환
    - 없는 검색 경로는 0건이 아니라 오류
5. **정규식 문법**
    - Rust regex: 선형 시간 엔진이며 `\d \w \b`가 Unicode 기준이라 한글 단어 경계도 동작
    - look-around와 backreference는 fancy-regex로 자동 전환되며, backtrack 한도와 검색 시간 초과로 제한
    - 단순 문법 오류는 한 번만 리터럴 검색으로 fallback
6. **기타 동작**
    - 바이너리 파일은 건너뜀
    - 큰 패턴은 `pattern_path`로 전달 가능

### 5. Git

1. **실행 방식**
    - 모든 git 도구는 `path`가 필수이며, 그 worktree 안에서 PATH의 git CLI를 실행
2. **git-status**
    - `status --porcelain --branch` 실행
3. **git-add**
    - 경로를 stage하며 `all`/`update`/`force` 지원
4. **git-commit**
    - 로컬 git 설정 없이도 커밋되도록 기본 committer identity 주입
    - 선택적 author 지정과 `amend`/`allow-empty`/`no-verify` 지원
    - 메시지는 Conventional Commit 헤더로 시작해야 함: type은 소문자 영어, 요약은 언어 무관
5. **git-amend**
    - 마지막 커밋을 다시 씀: 메시지가 없으면 `--no-edit`, 있으면 검증한 새 메시지 사용
    - author 지정 또는 reset-author, staging, `allow-empty`, `no-verify` 지원
6. **git-diff**
    - `staged`, `nameOnly`, `stat`, `source`/`target`, `contextLines`, `check`, 경로 필터로 `git diff` 실행
7. **git-show**
    - object(또는 `objects[]`)와 선택적 `object:path`를 `git show`로 출력
8. **옵션 주입 차단**
    - `-`로 시작하는 revision 입력은 거부하므로 revision으로 git 옵션을 끼워 넣을 수 없음

### 6. 웹

1. **web-fetch**
    - 브라우저 없이 URL 하나 또는 여러 개를 HTTP/HTTPS로 가져옴
    - 출력 모드: `markdown`(기본), `text`, `links`, `readability`(본문), 원본 `html`
    - 긴 렌더링 결과는 `offset`/`maxChars`로 나눠 읽음
2. **web-extract**
    - 이미 가진 HTML(inline 또는 로컬 파일)을 네트워크 없이 markdown, text, links, readability로 변환
3. **download-to-file**
    - URL 하나 또는 여러 개를 임시 파일로 스트리밍한 뒤 제자리로 rename
4. **SSRF guard**
    - `web-fetch`, `download-to-file`, `isUrl`을 쓴 `file-read`는 호스트를 resolve해서 공개 주소가 아니면 거부: private, link-local, unique-local, CGNAT, multicast/reserved, IPv4를 품은 IPv6 형태
    - redirect hop마다 다시 검사
    - 로컬 개발용으로 loopback(`localhost`, `127.0.0.0/8`, `::1`)은 허용
    - 검증한 주소를 연결 resolver에 고정하므로, 검사와 연결 사이에 DNS 응답이 바뀌어도 요청 대상이 바뀌지 않음
5. **본문 크기**
    - `web-fetch` 기본값 20,000,000 byte
    - `download-to-file` 기본값은 hard ceiling인 200,000,000 byte
    - 명시적 `maxBytes`로 두 값 모두 그 ceiling까지 조정 가능

---

## 사용법

### 1. Batch 입력

1. **종류별 한 번 호출**
    - 같은 종류의 대상은 한 번의 호출에 모두 담음
    - 읽기 batch는 3-8개 파일 정도로 유지하고 더 큰 집합은 나눔
    - 큰 항목은 그 항목만 잘리고 호출 자체는 거부되지 않음

```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"file-read","arguments":{"paths":["src/main.rs","src/lib.rs","Cargo.toml"]}}}
```

### 2. 인자 형태 보정

1. **흡수하는 실수**
    - dispatcher는 도구 실행 전에 흔한 인자 형태 실수를 흡수함:
        1) 단일 flat 작업은 `items[]`로 감쌈;
        2) key alias 정규화(`file_path` → `path`, `from`/`to` → `source`/`destination`);
        3) `path-remove`와 메타데이터 도구는 `items[]`뿐 아니라 단순 `paths[]` 배열도 받음;
        4) JSON 문자열로 인코딩된 배열은 다시 배열로 파싱함.

### 3. 경로

1. **해석 방식**
    - 절대 경로 권장
    - 상대 경로는 서버 작업 디렉터리 기준으로 해석
    - 맨 앞의 `~`는 홈 디렉터리로 확장
    - `"C:/work/a.txt"`처럼 같은 따옴표로 감싼 경로는 안쪽 값이 절대 경로면 따옴표를 벗겨서 해석함, 일부 모델이 따옴표를 남긴 JSON 문자열을 보내기 때문
2. **접근 범위**
    - 허용 루트 제한이 없어 프로세스 사용자가 접근 가능한 모든 경로에 닿음
    - git 도구는 호출마다 명시적 `path` 필요

### 4. 큰 인자

1. **인자 파일**
    - 모든 도구는 JSON 파일에서 인자를 읽을 수 있음: `{"args_path":"/abs/path/to/args.json"}`
    - 선택적 `args_offset`/`args_length`로 그 파일의 구간 지정
2. **필드별 파일**
    - 큰 값은 `content_path`, `old_string_path`, `new_string_path`, `replacement_path`, `pattern_path`로 전달 가능

---

## 응답

### 1. Envelope

1. **형태**
    - 기본 `off` 모드에서 모든 호출은 compact envelope 하나를 반환: `{data, durationMs}`, 실패 시에만 `error` 추가
    - lossless 모드와 lossless 호스트 프로필은 대신 `content` 평문을 반환하며, 호스트 프로필과 설정 절에서 설명
2. **필드**
    - `data.content`: 결과 본문(파일 내용, 검색 줄, diff, 목록)을 정확히 한 번 담음
    - `data.structuredContent`: 도구별 메타데이터(개수, 경로, backend)만 담고 본문은 중복하지 않음
    - `durationMs`: 도구 실행 시간
    - `_meta.fsMcpResult`: 상태, 실행 시간, content type, structured content 존재 여부
    - `isError`: 도구 실패 시 설정
3. **Batch 결과**
    - batch 도구는 항목별 `{index, ok, data}`와 `succeededCount`, `failedCount`, `totalCount`를 추가
    - 모든 항목이 실패할 때만 batch 전체를 오류로 표시

### 2. 호스트 프로필

1. **판별**
    - `initialize` 때 `clientInfo.name`을 정확히 일치로 비교해 이후 호출에 그 호스트 프로필을 적용하고, 새 `initialize`가 오면 교체
    - `initialize`마다 stderr에 한 줄 기록: `rust-fs-mcp: initialize client="<name>" version="<ver>" protocol="<requested>" host=<host>`
2. **프로필**
    - 형식: 클라이언트 이름, 호스트, 결과 형태, 출력 예산
        1) `codex-mcp-client`: Codex, lossless, 30,000 byte
        2) `Code - OSS`, `Visual Studio Code`, `Visual Studio Code - Insiders`: VS Code의 GitHub Copilot, lossless, 30,000 byte
        3) `copilot-cli`: GitHub Copilot CLI, lossless, 19,000 byte
        4) `gemini-cli-mcp-client`: Gemini CLI, lossless, 30,000 byte
        5) `opencode`: OpenCode, lossless, 30,000 byte
        6) `claude-code`와 그 밖의 이름: 기본 envelope, 30,000 byte
    - 서버 `instructions`의 둘째 줄은 그 호스트의 내장 도구 중 대체할 것과 유지할 것을 안내함(예: Codex는 `exec_command`, `apply_patch`)
3. **이유**
    - Claude Code는 직렬화한 `structuredContent`를 모델에 넘기므로 기본 envelope에서도 본문이 그대로 전달됨
    - Gemini CLI는 `content`만 읽고, OpenCode는 비어 있지 않은 `content`를 쓰고, VS Code는 텍스트 대신 직렬화한 `structuredContent`를 보여 주고, Codex는 `structuredContent`를 우선해 이미지를 버리므로, 이 호스트들에는 본문을 `content`로 보냄
    - Copilot CLI는 20 KiB를 넘는 도구 출력을 파일로 저장하고 모델에는 경로와 미리보기만 주므로 예산을 그보다 낮게 둠
4. **확인한 이름**
    - 2026-11-07 실제 호스트 실행에서 `claude-code`(Claude Code 2.1.318), `codex-mcp-client`(Codex 0.166.0), `copilot-cli`(Copilot CLI 1.0.107), `gemini-cli-mcp-client`(Gemini CLI 0.67.0), `opencode`(OpenCode 2.2.5)를 기록함
    - VS Code는 실행하지 않았고, `Code - OSS`는 VS Code 소스, 공식 이름 두 개는 문서화된 제품 이름 기준임
    - `gemini mcp list`는 `mcp-test-client`라는 테스트 클라이언트로 연결만 확인해 기본 envelope를 받고, 실제 Gemini CLI 세션은 `gemini-cli-mcp-client`를 보냄
    - 일치하지 않는 이름은 기본 envelope로 동작하며, stderr 줄에서 호스트가 실제로 보내는 이름을 확인할 수 있음

### 3. 출력 예산

1. **기본값**
    - 결과를 서버에서 30,000 byte(Copilot CLI는 19,000 byte)로 제한해 Claude Code 기본 25,000 token 같은 호스트 출력 한도 아래에 머묾
2. **적용 순서**
    - 먼저 찾은 값을 사용:
        1) config.toml의 `output.max_bytes`;
        2) 레거시 환경 변수 `RUST_FS_MCP_OUTPUT_BYTES`;
        3) `MAX_MCP_OUTPUT_TOKENS`에 token당 1.2 byte를 곱한 값;
        4) 호스트 프로필 기본값(Copilot CLI 19,000 byte).
    - 아무 값도 없으면 30,000 byte
    - `tools/list`의 `anthropic/maxResultSizeChars` `_meta`는 연결된 호스트와 관계없이 항상 Claude Code 예산을 씀
3. **잘림 처리**
    - 초과 본문은 JSON escape 기준 byte로 재서 그 자리에서 자르고, 안내 문구와 `outputTruncated`를 붙임
    - batch 항목은 water-filling 방식으로 예산을 나눔
    - 잘린 `file-read`, `web-fetch` 항목은 `next: offset=N`, 잘린 `file-read-line-range` 항목은 `next: start_line=N`으로 끝남
    - 실패한 호출은 일반 `content` 텍스트에도 오류 사유를 담음

### 4. 설정

1. **파일 위치**
    - `~/.config/rust-fs-mcp/config.toml`, 프로세스당 한 번 읽으므로 수정 후 서버 재시작 필요
    - `RUST_FS_MCP_CONFIG`로 경로 변경, 빈 값이나 `none`이면 파일을 읽지 않음
    - 파일이 없으면 기본값 유지, 잘못된 키는 기본값을 유지하고 stderr에 경고 출력
2. **Lossless 모드**
    - `compress = "lossless"`면 `structuredContent` 없이 `content` 평문 블록 하나로 반환, Claude Code는 둘 다 있으면 `content` 대신 `structuredContent`를 전달하기 때문
    - 본문은 JSON escape와 반복 메타데이터 없이 전달되고, 텍스트에 없는 structured 필드만 `[meta] {...}` 한 줄로 붙음
    - 실행 시간과 상태는 `_meta.fsMcpResult`로 이동
    - 결과 본문은 바꾸지 않으므로 정확한 내용 보장은 그대로 유지
    - `compress = "off"`면 모든 호스트에 기본 envelope를 강제하고, `compress`를 비워 두면 호스트 프로필을 따름

```toml
[output]
compress = "lossless" # "off" | "lossless", 비우면 호스트 프로필을 따름
max_bytes = 30000 # RUST_FS_MCP_OUTPUT_BYTES 환경 변수 대체
```

---

## 소스에서 빌드

### 1. 요구 사항

1. **툴체인**
    - Rust 1.88 이상(edition 2024)
2. **런타임**
    - git 도구를 쓸 때만 PATH의 `git` 필요

### 2. 호스트 빌드

1. **결과물**
    - 결과물 위치: `target/release/rust-fs-mcp`(Windows는 `rust-fs-mcp.exe`)

```bash
cargo build --release
```

### 3. 타깃 지정 빌드

1. **크로스 타깃**
    - 링크 가능한 호스트에서 타깃을 추가하고 `--target`을 지정, 릴리스 워크플로는 각 타깃을 해당 OS runner에서 빌드함

```bash
rustup target add aarch64-apple-darwin
cargo build --release --target aarch64-apple-darwin
```

---

## 한계

1. **Git**
    - git 도구는 PATH의 `git`이 필요하며 프로세스 내장 git object store는 없음
    - 동작은 설치된 git 버전을 따르며 submodule, rename 감지 기본값도 포함
2. **웹**
    - 브라우저 tier가 없어 JavaScript로 렌더링하는 클라이언트 측 페이지는 가져올 수 없음
3. **MCP 범위**
    - MCP resources와 resource templates는 빈 목록을 반환하며, 이 프로젝트는 도구에 집중함

---

## 개발

### 1. 검사

1. **CI 검사**
    - CI가 push마다 실행하는 검사

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
```

### 2. 테스트

1. **테스트 범위**
    - 각 모듈 안의 unit test가 핵심 동작을 검증
    - `tests/tool_matrix.rs`가 catalog의 모든 도구를 dispatch로 호출해 catalog와 dispatcher의 일치를 확인
2. **테스트 격리**
    - `.cargo/config.toml`이 `RUST_FS_MCP_CONFIG=none`을 설정하고 출력 예산 환경 변수를 비우므로, `cargo test`는 개인 설정을 읽지 않음
3. **벤치마크**
    - stdio 벤치마크와 A/B 비교 스크립트는 `bench/README.md` 참조

---

## 라이선스

1. **Apache-2.0**
    - [LICENSE.md](LICENSE.md) 참조
