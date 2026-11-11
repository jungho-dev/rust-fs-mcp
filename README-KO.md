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
    - Node, Python, `rg` 런타임 불필요, PATH의 `git`은 git 도구에만, 선택 설치인 `ast-grep` CLI는 `file-replace` `mode: "ast"`에만 필요
    - 파일, 디렉터리, 경로, 편집, 내용 검색, git, 웹에 걸친 24개 도구
    - 25번째 도구인 읽기 전용 `mysql-query`는 config.toml에 `[mysql]` 절이 있을 때만 노출되며, MySQL wire 클라이언트를 내장해 드라이버 crate가 필요 없음
    - `fd` 방식 경로 찾기, `sd` 방식 패턴 치환, `similar` 라이브러리 기반 unified diff를 프로세스 안에서 실행
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
    - 실행 파일은 내부용 `--isolated-worker` 플래그도 받지만, 서버가 자기 fancy-regex worker를 띄울 때만 쓰므로 클라이언트 설정에는 절대 넣지 않음
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
    - 클라이언트를 재시작하면 24개 도구가 `tools/list`에 나타나고, config.toml에 `host`와 `user`를 갖춘 `[mysql]` 절이 있으면 `mysql-query`까지 25개가 나타남
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
    - `path-copy`, `path-move`, `path-remove`, `path-stat`, `file-edit`, `file-edit-lines`, `file-replace`, `file-diff`
3. **검색**
    - `fs-search`, `path-find`
4. **Git**
    - `git-status`, `git-add`, `git-commit`, `git-amend`, `git-diff`, `git-show`
5. **웹**
    - `web-fetch`, `web-extract`, `download-to-file`
6. **데이터베이스**
    - `mysql-query`, config.toml에 `host`와 `user`를 갖춘 `[mysql]` 절이 있을 때만 노출
7. **Annotation**
    - 모든 도구는 사람이 읽는 `title`(예: `Stage Git Files`)을 최상위와 `annotations.title`에 함께 가짐
    - `readOnlyHint: true`: `file-read`, `file-read-line-range`, `dir-list`, `fs-search`, `path-find`, `path-stat`, `file-diff`, `git-status`, `git-diff`, `git-show`, `web-fetch`, `web-extract`
    - `destructiveHint: true`: `file-write`, `file-edit`, `file-edit-lines`, `file-replace`, `path-move`, `path-remove`, `git-add`, `git-commit`, `git-amend`, `download-to-file`
    - `destructiveHint: false`: `dir-create`, `path-copy`
    - `git-add`는 `all: true`가 삭제까지 stage하고 경로 지정 add가 일부만 stage한 hunk를 덮어쓰므로 파괴적 도구로 분류
    - `openWorldHint: true`: `file-read`(`isUrl` 읽기), `web-fetch`, `download-to-file`, 나머지 도구는 모두 `false`
    - `mysql-query`는 노출될 때 `readOnlyHint: true`, `openWorldHint: false`를 선언

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
    - `path-copy`는 디렉터리를 자기 자신이나 자기 하위 경로로 복사하는 요청을 거부
2. **path-stat**
    - 여러 경로의 메타데이터를 한 번에 반환
3. **file-edit**
    - 정확한 블록 치환(`old_string` → `new_string`) 적용
    - `old_string`이 정확히 `expected_replacements`회(기본 1) 일치해야 하며, 아니면 일치한 줄 번호와 함께 실패하고 아무것도 바꾸지 않음
    - 빈 `old_string`은 거부
    - 한 줄짜리 `old_string`을 여러 줄 텍스트로 바꾸면 새 줄들은 파일의 우세한 줄바꿈 형식을 따름
4. **file-edit-lines**
    - 1-based 줄 번호로 치환, 삽입, 삭제
    - 마지막 줄 개행 유무를 포함해 원본 줄바꿈 형식 유지
5. **dryRun**
    - `file-edit`, `file-edit-lines` 항목은 `dryRun`(기본 `false`)을 받으며, 모든 검증과 인코딩 단계를 실행하고 쓰기만 건너뜀
    - 결과는 `Would edit ...`와 unified diff, 내용이 같으면 `No changes`, 줄바꿈만 바뀌면 `eolChanged`와 함께 `Only line endings would change`
    - `dryRun` 항목이 하나라도 있는 batch는 전체 multi-line 출력을 쓰고, `dryRun`이 없으면 응답은 이전과 같음
6. **file-replace**
    - `sd` 방식으로 파일 안의 정규식 또는 리터럴 찾아 바꾸기 실행, 항목 하나가 `file_path` 하나이며 `replacement`는 필수이고 `""`면 일치 부분 삭제
    - `mode: "regex"`(기본)는 항상 multi-line이고 `ignoreCase: true`가 아니면 대소문자를 구분하며, `fs-search`와 달리 smart-case가 아님, `dotAll`, `wordMatch`는 선택
    - look-around와 backreference는 fancy-regex로 전환되며, 잘못된 정규식은 리터럴 fallback 없이 오류
    - 항목마다 `timeout_ms`(기본 25000)를 받고 결과 상한은 256 MiB
    - fancy-regex 항목은 kill 가능한 worker 프로세스에서 실행되며 찾는 즉시 일치 구간을 스트리밍하므로 결과 상한과 `maxReplacements`만 적용됨, worker는 항목의 `timeout_ms`에 kill되고, kill이나 worker crash는 그 항목을 실패시키고 아무것도 쓰지 않으며 서버는 계속 동작
    - 치환 문자열은 `$1`, `${1}`, `${name}`, `$$`를 확장하고, 모호한 `$1a`/`$1_x`와 없는 group 참조는 거부
    - `mode: "literal"`은 치환 문자열을 그대로 넣고, `mode: "ast"`는 ast-grep 패턴과 template으로 다시 쓰며 중첩 일치는 `ast-grep -U`처럼 가장 바깥 것만 남김
    - `maxReplacements`는 파일 순서로 치환 수를 제한하고(`0`이면 전부, `sd -n`과 같음), `expectedReplacements`가 맞지 않으면 아무것도 쓰지 않음
    - 모든 항목을 쓰기 전에 메모리에서 먼저 계획하므로 항목 하나가 실패하면 아무것도 쓰지 않으며, 같은 파일 항목은 순서대로 적용
    - 어느 항목이든 `dryRun: true`면 호출 전체를 미리보기로 처리하고, 실제 적용한 편집도 항목별 unified diff를 반환
    - UTF-8(BOM 유지), UTF-16 LE/BE BOM, EUC-KR/CP949 파일은 원래 인코딩으로 다시 씀
    - `sd`와 다른 점:
        1) `wordMatch`가 패턴 전체를 `\b(?:...)\b`로 감싸므로 alternation도 단어 경계를 유지;
        2) CRLF 파일에서도 `^`, `$`가 동작;
        3) look-around와 backreference가 오류 없이 동작;
        4) 없는 group 참조는 빈 문자열을 넣지 않고 오류;
        5) 치환 문자열의 `\n` 같은 backslash escape는 해석하지 않고 그대로 유지.
7. **file-diff**
    - `path`를 `otherPath`, 또는 inline `content`나 `content_path`와 비교해 git 없이 git 형식 unified diff를 반환
    - 옵션: `contextLines`(3), `ignoreWhitespace`(`git diff -b`), `algorithm` `myers`(기본), `patience`, `histogram`
    - CRLF와 LF는 비교 전에 맞추고 `eolChanged`로 보고하며, 바이너리 파일은 거부하고 한쪽당 64 MiB로 제한
    - 양쪽 모두 UTF-8, UTF-16, EUC-KR 디코딩
    - 긴 diff는 출력 예산 근처에서 잘리고 `truncated: true`가 붙지만 추가·삭제 줄 수는 정확히 유지
8. **ast-grep 탐색**
    - 서버는 `file-replace` `mode: "ast"`에 쓸 `ast-grep` CLI를 다음 순서로 찾음:
        1) `RUST_FS_MCP_AST_GREP`, 절대 경로 또는 서버 작업 디렉터리 기준 상대 경로이며, `.cmd`, `.bat`, `.ps1` wrapper는 npm native 실행 파일로 해석하거나 오류;
        2) Windows에서는 서버 실행 파일 폴더, System32, Windows 폴더(Rust 표준 `Command` 순서);
        3) PATH의 절대 경로 항목, 빈 항목과 상대 경로 항목은 건너뜀;
        4) `ast-grep.cmd` shim 옆에 있는 npm `@ast-grep/cli` 설치의 native 실행 파일.
    - Windows에서는 자식 프로세스를 Job Object 안에서 실행하므로 시간 초과 시 하위 프로세스까지 종료되고, Unix에서는 자체 process group을 받아 group 전체가 종료됨
    - Claude Code의 숨은 conhost처럼 서버에 console이 있으면 자식이 그것을 물려받고, console이 없으면 `CREATE_NO_WINDOW`를 유지해 약 7 ms 더 걸림

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
    - look-around와 backreference는 fancy-regex로 자동 전환되며, backtrack 한도로 제한하고 `timeout_ms` + 1초에 멈추는 kill 가능한 worker 프로세스에서 실행, kill이나 worker crash는 그 항목만 실패시키고 서버는 계속 동작
    - fancy-regex 문법 오류를 포함한 잘못된 정규식은 한 번만 리터럴 검색으로 fallback하며, backend label이 입력 패턴 기준 위치와 함께 오류를 알려 줌
    - 지원하지 않는 Unicode 모드 전환, 재귀 깊이, 컴파일 크기 같은 엔진 한도는 그대로 오류
    - `wordMatch`는 감싸기 전에 패턴만 따로 검증하므로 짝이 맞지 않는 `)`는 문법 오류이고 `(?x)` 주석은 동작
6. **기타 동작**
    - 바이너리 파일은 건너뜀
    - 큰 패턴은 `pattern_path`로 전달 가능
    - `^`와 `$`는 CRLF 파일에서도 LF 파일처럼 줄 경계에 일치하므로 `off$`, `^$`, `^\s*$`가 두 형식에서 같은 줄을 반환
    - 줄 단위 검색은 일치 전에 각 줄 끝의 CR 하나를 떼므로 `\r`, `\r$`는 CRLF 줄 끝에 일치하지 않으며, CR 자체를 찾으려면 `off\r\n`처럼 `multiline: true`를 지정
7. **path-find**
    - `fd` 방식으로 이름 정규식 또는 glob으로 파일과 디렉터리를 찾으며, 내용은 읽지 않고 `fs-search`와 같은 `ignore` 탐색을 씀
    - `pattern`은 smart-case 정규식, `glob`은 `|`로 대안을 잇는 glob이며, 둘 다 디렉터리를 포함한 항목 이름과 비교
    - `fullPath: true`면 root 기준 `/` 경로와 비교하며, 이때 `*`는 `/`에서 멈추므로 깊이와 무관하게 찾으려면 `**/*.rs` 사용
    - 숨김 항목은 `includeHidden: true`이거나 pattern 또는 glob이 `.`으로 시작할 때만 포함
    - 저장소 `.gitignore`는 `fs-search`처럼 따르며, `node_modules/`, `target/`, `.git/`은 `noDefaultExcludes: true`가 아니고 root도 그 안이 아니면 건너뜀
8. **path-find 필터**
    - `type`(`any`, `file`, `directory`, `symlink`), `extension`(`rs|toml`), `empty`, `minDepth`/`maxDepth`, `excludePatterns`(`fd --exclude`), `followLinks`
    - `size`는 `fd -S`를 따름: `+10k` 이상, `-1M` 이하, `500b` 정확히, 단위 필수이며 `k`는 1000, `ki`는 1024, 파일에만 적용
    - `changedWithin`, `changedBefore`는 기간(`30m`, `2h`, `7d`, `2w`) 또는 날짜(`YYYY-MM-DD`, `YYYY-MM-DD HH:MM:SS`, RFC 3339, `@epoch`)를 받음
    - offset 없는 날짜는 `fd`처럼 로컬 시간 기준이며, DST로 겹치는 시각은 나중 시점, 건너뛴 시각은 전환 전 offset을 씀
    - `TZ`는 TZif 경로, IANA 이름, POSIX 규칙을 받고, `TZ`가 없으면 시스템 시간대를 쓰며, Windows의 IANA 이름은 ICU(Windows 10 1703 이상)가 필요하고 없으면 날짜가 오류
9. **path-find 출력**
    - 첫 줄은 `<root>:`, 이어서 `/` 구분자의 root 기준 항목이 나오고 디렉터리는 `/`로 끝남
    - `details: true`면 탭, byte 단위 크기, 탭, UTC RFC 3339 수정 시각을 덧붙임
    - `sortBy`는 `path`(기본, 경로 구성 요소 단위), `modified`(최신순), `size`(큰 순)이며, `maxResults`는 그 순서의 정확한 앞부분을 반환
    - 부분 결과는 `limitReached: true` 또는 `timedOut: true`로 표시

### 5. Git

1. **실행 방식**
    - 모든 git 도구는 `path`가 필수이며, 그 worktree 안에서 git CLI를 실행
    - `git`은 PATH의 절대 경로 항목에서만 찾음
    - git 자식 프로세스는 `GIT_TERMINAL_PROMPT=0`으로 실행되므로 console에서 자격 증명을 묻지 않음
    - Windows에서 git 자식은 서버에 console이 붙어 있으면 그것을 물려받고, 없으면 `CREATE_NO_WINDOW`로 실행됨
    - 없는 경로는 `Path does not exist`, git이 저장소로 인식하지 못한 경로는 `Not a git repository`, 그 밖의 실패는 `Cannot open the git repository at <path>`와 git 원문 메시지로 보고
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
    - 상대 링크와 redirect 대상은 RFC 3986에 따라 해석: query만 있는 참조, fragment만 있는 참조, dot segment, `//host` 형태를 처리하고, 자체 scheme을 가진 참조는 대소문자와 관계없이, `mailto:` 같은 비HTTP scheme까지 원문 그대로 유지
2. **web-extract**
    - 이미 가진 HTML(inline 또는 로컬 파일)을 네트워크 없이 markdown, text, links, readability로 변환
    - 256단계보다 깊게 중첩된 HTML은 그 깊이에서 평탄화하며, `web-fetch`도 같은 파서를 씀
3. **download-to-file**
    - URL 하나 또는 여러 개를 임시 파일로 스트리밍한 뒤 제자리로 rename
    - `overwrite: true`는 기존 파일을 먼저 지우지 않고 그 rename으로 교체하므로, 다운로드나 rename이 실패하면 원본이 그대로 남음
    - 한 batch에서 목적지가 같은 항목은 병렬이 아니라 순서대로 실행되며, `overwrite`가 없으면 뒤 항목이 `File already exists`로 실패
4. **SSRF guard**
    - `web-fetch`, `download-to-file`, `isUrl`을 쓴 `file-read`는 호스트를 resolve해서 공개 주소가 아니면 거부: private, link-local, unique-local, CGNAT, multicast/reserved, IPv4를 품은 IPv6 형태
    - redirect hop마다 다시 검사
    - 로컬 개발용으로 loopback(`localhost`, `127.0.0.0/8`, `::1`)은 허용
    - 검증한 주소를 연결 resolver에 고정하므로, 검사와 연결 사이에 DNS 응답이 바뀌어도 요청 대상이 바뀌지 않음
5. **재시도**
    - `web-fetch`, `download-to-file`, `isUrl`을 쓴 `file-read`는 멱등 GET 요청이므로, 응답 byte가 오기 전에 연결이 reset되면 1회 재시도하고, 두 번째 reset은 오류로 보고함
6. **본문 크기**
    - `web-fetch` 기본값 20,000,000 byte
    - `download-to-file` 기본값은 hard ceiling인 200,000,000 byte
    - 명시적 `maxBytes`로 두 값 모두 그 ceiling까지 조정 가능

### 7. 데이터베이스

1. **mysql-query**
    - config.toml의 `[mysql]` 절에 설정한 MySQL 서버에 호출당 읽기 전용 SQL 문장 하나를 실행
    - 그 절에 `host`와 `user`가 있을 때만 `tools/list`에 포함되며, 절 없이 호출하면 켜는 방법을 안내하는 오류를 반환
    - 허용 문장: `SELECT`, `SHOW`, `DESCRIBE`/`DESC`, `EXPLAIN`, `WITH ... SELECT`
    - 스키마를 먼저 읽고(`SHOW`, `DESCRIBE`, `TABLE_SCHEMA`와 `TABLE_NAME`으로 거른 `information_schema`) 모든 `SELECT`는 `LIMIT`으로 작게 유지
2. **읽기 전용 게이트**
    - 게이트는 문장이 서버에 닿기 전에 다음을 거부함:
        1) 한 호출에 문장 두 개 이상;
        2) 실행 주석(`/*! ... */`, `/*M! ... */`);
        3) 세션 변수나 시간 제한을 바꾸는 optimizer hint(`SET_VAR`, `MAX_EXECUTION_TIME`);
        4) `WITH` 뒤의 `INSERT`, `UPDATE`, `DELETE`, `REPLACE`;
        5) 잠금 읽기(`FOR UPDATE`, `FOR SHARE`, `LOCK IN SHARE MODE`);
        6) `SLEEP`, `BENCHMARK`, `GET_LOCK` 같은 잠금 함수, 복제 대기 함수, `LOAD_FILE`;
        7) `EXPLAIN ANALYZE`;
        8) `INTO OUTFILE`, `INTO DUMPFILE`.
    - 따옴표 문자열과 backtick 식별자 안의 키워드는 세지 않으므로 `SELECT 'FOR UPDATE'`는 통과함
3. **실행**
    - 모든 문장은 multi-statement를 협상하지 않는 연결에서 `START TRANSACTION READ ONLY` ... `ROLLBACK` 안에서 실행됨
    - 세션에 서버 측 시간 제한을 둠: MySQL은 `MAX_EXECUTION_TIME`, MariaDB는 fallback으로 `max_statement_time`
    - 그 제한보다 2초 늦은 클라이언트 측 timeout에 걸리면 연결을 버리고 두 번째 연결로 `KILL QUERY`를 보냄
    - 호출은 서버 프로세스당 연결 하나에서 직렬로 실행되며, 유휴 연결은 60초 동안 재사용한 뒤 닫음
    - 시간 초과, 끊김, 잘린 결과가 있었던 연결은 재사용하지 않고 닫음
4. **인자**
    - `sql`: 실행할 문장, `sql_path`는 UTF-8 파일에서 문장을 읽음
    - `maxRows`: 이번 호출의 행 상한, `mysql.max_rows`를 넘지 못함
    - `timeout_ms`: 이번 호출의 시간 제한, `mysql.query_timeout_ms`를 넘지 못함
5. **결과**
    - 행은 한 줄에 객체 하나씩 담은 JSON 배열로 반환되고 key는 `SELECT` 순서를 따르며, 중복 열 이름에는 `_2`, `_3` 접미사가 붙음
    - 정수와 실수는 JSON number, `DECIMAL`과 날짜·시간 값은 자릿수를 잃지 않도록 문자열로 유지
    - JSON 열은 JSON 그대로 넣고, `NULL`은 `null`, 출력 가능한 UTF-8이 아닌 binary 값은 `0x` hex로 바꿔 256 byte까지 표시
    - `maxRows` 또는 출력 예산의 절반에서 잘린 결과는 `truncated: true`를 보고하며, 메타데이터에는 `rowCount`, `columnCount`, `queryMs`도 담김
6. **Wire 클라이언트**
    - 드라이버 crate 없이 내장한 클라이언트이며, TLS 없는 평문 TCP와 text protocol(`COM_QUERY`)을 씀
    - 인증은 `mysql_native_password`와 `caching_sha2_password`(fast path, RSA full authentication)를 지원
7. **보안 한계**
    - 연결에 TLS가 없어 문장과 행이 암호화 없이 네트워크를 지나므로 신뢰할 수 있는 네트워크에서만 사용
    - 게이트는 경계가 아니라 다중 방어의 한 겹이므로, 도구에는 그 자체로 읽기 전용인 데이터베이스 계정을 줌

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
    - `[mysql]`이 설정되어 있으면 `instructions` 끝에 데이터베이스 읽기를 `mysql-query`로 안내하는 한 줄이 더 붙음
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
3. **도구 사용 집계**
    - `[stats] enabled = true`면 도구 호출 수를 날짜별 `<dir>/YYYY-MM-DD.md` 표로 기록하고, 키가 없거나 `false`면 설정값 확인 한 번 외에 비용이 없음
    - `dir` 기본값은 `.docs`이며, 상대 경로는 서버 작업 폴더 기준으로 해석함
    - 호출 경로에서는 원자적 카운터만 올리고, 백그라운드 스레드가 2초마다, 그리고 stdin 종료 시 한 번 더 파일에 병합함
    - 여러 서버 프로세스가 같은 파일을 `.tool-stats.lock` 잠금 파일로 공유하며, 강제 종료된 프로세스는 마지막 병합 이후 카운트를 잃음
4. **MySQL 연결**
    - `host`와 `user`를 갖춘 `[mysql]` 절이 `mysql-query`를 켜며, 두 키 중 하나라도 없으면 도구는 꺼진 채로 stderr에 경고를 출력함
    - `port` 기본값은 3306이고, `database`는 선택 항목인 기본 스키마임
    - 비밀번호는 설정된 출처 중 다음 순서로 먼저 오는 것을 씀:
        1) `password_env`, 비밀번호를 담은 환경 변수의 이름;
        2) `password_dpapi`, 현재 사용자가 Windows DPAPI로 봉인한 파일;
        3) `password`, 설정 파일의 평문이며 권장하지 않음.
    - `connect_timeout_ms`(기본 10000)는 TCP 연결 시간을 제한함
    - `query_timeout_ms`(기본 10000)와 `max_rows`(기본 1000)는 `timeout_ms`, `maxRows` 인자의 기본값이자 상한임
    - `password_dpapi`는 Windows에서만 동작하며, 다른 플랫폼은 `password_env`를 안내하는 오류를 냄

```toml
[output]
compress = "lossless" # "off" | "lossless", 비우면 호스트 프로필을 따름
max_bytes = 30000 # RUST_FS_MCP_OUTPUT_BYTES 환경 변수 대체

[stats]
enabled = true # 기본 false
dir = "C:/project/.docs" # 기본 ".docs", 상대 경로는 서버 작업 폴더 기준

[mysql]
host = "db.example.invalid" # 필수
port = 3306 # 기본 3306
user = "reader" # 필수
database = "sample_db" # 선택, 기본 스키마
password_env = "SAMPLE_DB_PASSWORD" # 1순위: 환경 변수 이름
# password_dpapi = "~/.config/rust-fs-mcp/mysql-password.dpapi" # 2순위: Windows DPAPI 파일
# password = "..." # 3순위: 평문, 권장하지 않음
connect_timeout_ms = 10000 # 기본 10000
query_timeout_ms = 10000 # 기본 10000, timeout_ms 상한
max_rows = 1000 # 기본 1000, maxRows 상한
```

5. **DPAPI 비밀번호 파일**
    - 서버를 실행하는 Windows 사용자로 비밀번호를 한 번 봉인하며, DPAPI가 파일을 그 사용자에 묶으므로 다른 계정은 풀 수 없음
    - 스크립트는 비밀번호를 화면에 표시하지 않고 읽어 봉인한 byte만 기록하며, 대상 디렉터리는 미리 있어야 함
    - 스크립트가 쓴 파일 경로를 `password_dpapi`에 지정함

```powershell
Add-Type -AssemblyName System.Security
$secure = Read-Host -AsSecureString "MySQL password"
$bstr = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
$plain = [Text.Encoding]::UTF8.GetBytes([Runtime.InteropServices.Marshal]::PtrToStringBSTR($bstr))
[Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr)
$sealed = [Security.Cryptography.ProtectedData]::Protect($plain, $null, 'CurrentUser')
[IO.File]::WriteAllBytes("$HOME\.config\rust-fs-mcp\mysql-password.dpapi", $sealed)
```

---

## 소스에서 빌드

### 1. 요구 사항

1. **툴체인**
    - Rust 1.88 이상(edition 2024)
2. **런타임**
    - git 도구를 쓸 때만 PATH의 `git` 필요
    - `file-replace` `mode: "ast"`를 쓸 때만 선택 설치인 `ast-grep` CLI가 PATH에 필요

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
3. **ast-grep**
    - `file-replace` `mode: "ast"`는 ast-grep 탐색 순서로 찾은 `ast-grep` CLI가 필요하며(0.51.1로 검증), 없으면 이 mode는 설치 안내 오류를 반환하고 다른 도구는 영향 없음
    - Linux의 `sg`는 무관한 명령이므로 `sg`로 fallback하지 않음
    - `ast-grep`은 UTF-8만 파싱하므로 `mode: "ast"`는 BOM 없는 UTF-8만 받음
    - `assert!`, `format!` 같은 Rust 매크로 인자는 token tree라 그 안에서는 패턴이 일치하지 않음
4. **fancy-regex**
    - `fs-search`, `file-replace`의 fancy-regex 항목은 worker 프로세스에서 실행되어 대기 worker 기준 항목당 약 3 ms가 더 들고(2026-11-11 실측: look-around 없는 검색 5.6 ms 대비 8.6 ms), 그 밖의 호출은 worker를 띄우지 않음
    - 첫 fancy-regex 작업 뒤에는 유휴 대기 worker 하나를 유지함: 다음 작업이 그것을 쓰고 대체 worker가 새로 뜨므로 유휴 worker는 최대 하나임
    - Windows에서는 worker를 `DETACHED_PROCESS`로 띄우므로 worker마다 console host가 뜨지 않음
5. **시간대**
    - Windows에서 `TZ`의 IANA 이름은 ICU(Windows 10 1703 이상)가 필요하며, 없으면 offset 없는 `path-find` 날짜는 UTC로 대체하지 않고 오류
6. **MCP 범위**
    - MCP resources와 resource templates는 빈 목록을 반환하며, 이 프로젝트는 도구에 집중함
7. **MySQL**
    - `mysql-query`는 TLS 없는 평문 TCP로 연결하므로 신뢰할 수 있는 네트워크에서만 사용
    - 읽기 전용 게이트는 다중 방어의 한 겹이며, 데이터베이스 계정 자체가 읽기 전용이어야 함
    - 인증은 `mysql_native_password`, `caching_sha2_password`만 지원하고, `password_dpapi`는 Windows에서만 동작

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

2. **타깃**
    - CI는 `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`, `aarch64-apple-darwin`마다 해당 OS runner에서 clippy, 빌드, 테스트를 실행
    - 별도 MSRV job이 Rust 1.88.0으로 `cargo check --locked --all-targets` 실행
    - MSRV 검사와 Linux, macOS, Windows MSVC의 x86_64, aarch64 대상 `cargo clippy --all-targets -- -D warnings`를 검증했으며, 호스트가 아닌 타깃은 링크 없이 검사만 함

### 2. 테스트

1. **테스트 범위**
    - 각 모듈 안의 unit test가 핵심 동작을 검증
    - `tests/tool_matrix.rs`가 항상 노출되는 catalog 도구 24개 전부를 dispatch로 호출해 catalog와 dispatcher의 일치를 확인하고, `[mysql]` 절 없는 `mysql-query`가 켜는 방법 안내 오류를 반환하는지 확인
    - MySQL 클라이언트 unit test는 테스트 전용 loopback 서버를 상대로 실행되므로 데이터베이스가 필요 없음
    - PATH에 `ast-grep` CLI가 있으면 `file-replace` `mode: "ast"` 치환 테스트를 실행하고, 없으면 치환 테스트를 건너뜀
    - `tests/isolation.rs`는 실제 stdio 서버로 확인: 폭주하는 fancy-regex 검색·치환이 제때 kill되고 같은 서버가 계속 응답하며, crash한 worker는 그 항목만 실패시키고, worker 결과가 프로세스 내 엔진과 같고, fancy 항목만 worker를 띄우며, worker는 부모가 사라지거나 예산이 지나면 종료됨
    - 유휴 대기 worker가 다음 작업을 처리하고, 작업 timeout에 kill되며, 서버와 함께 종료되는지도 확인
2. **테스트 격리**
    - `.cargo/config.toml`이 `RUST_FS_MCP_CONFIG=none`을 설정하고 출력 예산 환경 변수를 비우므로, `cargo test`는 개인 설정을 읽지 않음
3. **벤치마크**
    - stdio 벤치마크와 A/B 비교 스크립트는 `bench/README.md` 참조
    - `bench/cli-bench.ts`는 CLI 통합 전후를 작업 단위로 비교: 기존 도구나 호출마다 띄운 `fd`, `sd`, `git` 프로세스 대 `path-find`, `file-replace`, `file-diff`, `dryRun`

---

## 라이선스

1. **Apache-2.0**
    - [LICENSE.md](LICENSE.md) 참조
