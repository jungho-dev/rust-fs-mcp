# rust-fs-mcp 전체 tool 응답 낭비·성능 분석 보고서

작성일: 2026-06-04. 대상: 22개 공개 tool 전체의 응답 파이프라인.

핵심 전제: Claude Code MCP client는 `tools/call` 결과에 `structuredContent`가 있으면 `content[]`를 버리고
`structuredContent` 직렬화 본문만 모델에 전달한다. 따라서 `structuredContent` 내부의 모든 중복 byte가
그대로 모델 입력 token 낭비다.

---

## 1. 발견 항목

### A. 본문 2~3중 전송 (대형, 본문 크기에 비례)

per-item text(`data.content`로 합류)와 tool별 `structuredContent`에 같은 본문이 동시 직렬화된다.

| ID | tool | 위치 | 중복 형태 |
|----|------|------|-----------|
| A1 | search-regex | `regex_item` | text(매치 라인 join) + `structured.results[]` 같은 라인 배열 = 2중 |
| A2 | search-get | `get_item` | text + `structured.results[]` + `structured.text` = 3중 |
| A3 | file-lines | `lines_result` | text(번호 라인) + `structured.lines[{number,text}]` = 2중, JSON object 오버헤드 가중 |
| A4 | dir-list | `dir_list_result` | text(entries join) + `structured.entries[]` = 2중 |
| A5 | file-read(디렉터리) | `read_directory` | text + `structured.entries[]` = 2중 |
| A6 | git-status | `handle_git_status` | text + `structured.status` + `structured.entries[]` = 3중 |
| A7 | git-diff | `handle_git_diff` | text + `structured.diff` = 2중 |
| A8 | git-show | `handle_git_show` | text + `structured.output` = 2중 |
| A9 | git-commit | `handle_git_commit` | 호출자가 보낸 message 전문을 `structured.message`로 재전송 |

### B. 요청 인자 에코 (중형, 아이템 수에 비례)

`create_batch_response`가 `results[].input`에 요청 인자를 그대로 반환한다. compact 모드에서도
256자 초과 문자열만 elide하고 나머지 key 전부를 에코한다. 모델은 자기가 보낸 인자를 이미 컨텍스트에
갖고 있고, 아이템 식별은 batch text의 `- [N] OK <path>` 라인으로 충분하다.

### C. batch per-item wrapper 중복 (중형)

`results[].result = {structuredContent, isError}` 구조에서 `isError`는 형제 필드 `ok`와 완전 중복이고,
`result` wrapper 자체가 depth 1단계 + key 1개를 항목마다 추가한다.

### D. 최상위 envelope 고정 중복 (소형, 호출당 약 80B)

| 필드 | 문제 |
|------|------|
| `error: null` | 성공 응답에서 정보가 0 |
| `schemaVersion: 1` | 모델 소비 관점 무가치 |
| `status` | `isError` 부재와 중복 |
| `toolName` | 호출자가 이미 아는 값 + batch text 첫 줄과 중복 |

### E. 연산 성능 하락 요소

| ID | 위치 | 문제 |
|----|------|------|
| E1 | `normalize_tool_result` | `combined_text`가 compact+성공 경로에서 결과를 쓰지 않는데도 전체 본문을 무조건 복사 |
| E2 | `read_item` | cap 판정용 `text.chars().count()`를 모든 read에서 전체 스캔 (byte 길이로 선판정 가능) |
| E3 | `run_batch`/`run_batch_parallel` | 아이템당 `item.clone()` 1회 + handler `items.clone()` 1회 = 대형 write/edit 인자 2회 복사 |

### F. 바이너리 base64 무제한 (안전성)

`binary_result`가 파일 전체 base64를 cap 없이 `structured.base64`로 반환한다. 텍스트 read에는
`RUST_FS_MCP_READ_MAX_CHARS`(기본 100k) cap이 있으나 바이너리 경로에는 적용되지 않아 대형 바이너리를
읽으면 envelope가 무한정 커진다.

### 정상 확인 항목 (수정 불필요)

* fs-inspect: snippet 예산(`maxSnippetChars` 기본 6000) + 단일 본문 구조라 양호.
* search-start: preview 20줄이 structured에만 존재, text는 1줄 요약 — 중복 없음.
* file-read(텍스트): 본문은 text 1회, structured는 메타만 — 이전 개선(ae5e26f 계열)으로 이미 해소.
* external.rs / args_ref.rs: channel 기반 대기, ASCII fast-path 등 양호.

---

## 2. 툴별 분석

전 22개 tool. 공통 D(envelope 고정 중복 제거)는 전 tool에, 공통 B·C(input 에코·wrapper 제거)는
batch 계열 15개 tool에 일괄 적용된다. 아래 표는 tool 고유 항목만 적는다. 실측치는 4장 표 참조.

### 파일·디렉터리 (11개, batch)

| tool | 고유 발견 | 고유 수정 |
|------|-----------|-----------|
| file-read (텍스트) | 본문 text 1회 — 정상(이전 개선). E2: cap 판정용 chars 전수 스캔 | byte 길이 선판정으로 스캔 생략 |
| file-read (디렉터리) | A5: entries가 text+structured 2중 | structured는 entryCount만 |
| file-read (바이너리) | F: base64 cap 미적용 — 대형 파일이면 envelope 폭탄 | read cap 기준 절단 + truncated·returnedBytes 표기 |
| file-read (URL) | 본문 1회 + headers 메타 — 정상 | — |
| file-lines | A3: 전 라인을 `{number,text}` object로 재전송 (object wrapper로 text보다 더 큼) | structured는 returned 수만 |
| file-write | 본문 중복 없음. B: content가 256자 이하면 통째 에코됨 | 공통만 |
| dir-mk | 고유 낭비 없음 | 공통만 |
| dir-list | A4: entries가 text+structured 2중 (maxEntries 500이면 목록 2회) | structured는 entryCount·truncated만 |
| file-copy · file-move · file-remove | 고유 낭비 없음 (1줄 요약 + 메타) | 공통만 |
| file-infos | 고유 낭비 없음 (메타 8필드) | 공통만 |
| file-edit · file-edit-lines | B: old/new_string·replacement가 256자 이하면 통째 에코됨. E3: 대형 인자 2회 clone | 공통 + clone 제거(E3) |

### 검색 (4개, batch)

| tool | 고유 발견 | 고유 수정 |
|------|-----------|-----------|
| search-start | preview 20줄이 structured 단독 — 중복 없음 | 공통만 |
| search-regex | A1: 매치 라인이 text+`results[]` 2중 | structured는 backend·totalCount만 |
| search-get | A2: 같은 페이지가 text+`results[]`+`text` 필드 3중 | structured는 sessionId·backend·offset·length·totalCount만 |
| search-stop | 고유 낭비 없음 | 공통만 |

### git (6개, 단일 호출 — B·C 비해당)

| tool | 고유 발견 | 고유 수정 |
|------|-----------|-----------|
| git-cwd | structured.status는 text에 없는 부가 정보 — 중복 아님 | 유지 (D만) |
| git-status | A6: porcelain 출력이 text+`status`+`entries[]` 3중 | structured는 path만 |
| git-add | 고유 낭비 없음 | D만 |
| git-commit | A9: 호출자가 보낸 message 전문을 에코 | structured는 path·oid만 |
| git-diff | A7: diff 전문이 text+`diff` 2중 | structured는 path만 |
| git-show | A8: 출력 전문이 text+`output` 2중 | structured는 path·object만 |

### inspect (1개, 단일 호출)

| tool | 고유 발견 | 고유 수정 |
|------|-----------|-----------|
| fs-inspect | snippet 예산(기본 6,000자)·본문 1회 — 양호 | 연계: git-status op이 슬림화된 structured 대신 content text에서 상태 취득 |

---

## 3. 수정 내용

| 항목 | 수정 |
|------|------|
| A1–A9 | tool별 structured에서 본문 필드 제거, 본문은 text 1회만. 메타(backend, totalCount, path, oid 등)는 유지 |
| B | compact 모드에서 `results[].input` 제거 (full 모드 `RUST_FS_MCP_COMPACT=0`은 verbatim 유지) |
| C | compact 모드에서 `results[]`를 `{index, ok, data}`로 평탄화 (`result` wrapper·`isError` 제거) |
| D | compact 모드에서 `error`(성공 시)·`schemaVersion`·`status`·`toolName` 제거, `durationMs` 유지 |
| E1 | `combined_text`를 에러 또는 full 모드에서만 계산 |
| E2 | `text.len() <= max_chars`면 chars 스캔 생략 |
| E3 | batch runner를 `&[Value]` 기반으로 전환, full 모드에서만 input clone |
| F | base64를 read cap(4의 배수 내림)으로 절단, `truncated`/`totalBytes` 표기 |

연계 수정: `fs-inspect git-status` op이 git-status structured 대신 text에서 상태를 취득,
`tests/tool_matrix.rs`·`tests/verify_diff_a5e26fad.rs`의 구조 assert, README/ARCHITECTURE 4개 문서의
envelope 명세, `src/core/config.rs`의 batch runner 호출부.

참고: search 계열의 totalCount는 heading 라인을 포함한 저장 라인 수다. search-get의 offset/length
페이지네이션이 같은 배열 인덱스를 쓰므로 의도된 정합이다.

---

## 4. 측정 (동일 입력, debug build, `result.structuredContent` 직렬화 byte)

측정 입력은 수정 영향을 받지 않는 고정 대상만 사용: Cargo.lock 검색, LICENSE.md 30줄/전문,
src 트리(파일 구성 불변), 고정 커밋(`96541d6..22ea483`) diff/show.

| tool | before | after | 절감 |
|------|--------|-------|------|
| search-regex | 1,882 | 970 | -48.5% |
| file-lines | 4,924 | 2,328 | -52.7% |
| dir-list | 1,322 | 786 | -40.5% |
| file-read | 11,486 | 11,286 | -1.7% |
| git-show | 2,321 | 1,202 | -48.2% |
| git-diff | 3,850 | 1,958 | -49.1% |
| file-infos | 795 | 594 | -25.3% |
| git-status | 510 | 550 | 비교 무효¹ |
| TOTAL | 27,090 | 19,674 | -27.4% |

¹ git-status는 측정 사이 이번 수정으로 작업트리 변경 파일이 8개→14개로 늘어 입력 자체가 커졌다
(비결정적 입력). 동일 입력이면 3중→1중 전송이므로 약 60% 절감이다.

file-read는 본문 중복이 원래 없어(이전 개선에서 해소) envelope 고정비만 줄었다. 본문이 작고
아이템 수가 많을수록(검색·목록·git 계열) 절감폭이 커진다. 표에 없는 tool(write·copy·move·remove·edit·mk·add·commit·cwd·start·get·stop·inspect)은
쓰기·세션 계열이라 결정적 입력 구성이 어렵거나 응답이 원래 작아 측정 대상에서 제외했다 —
공통 B·C·D 절감(아이템당 input 에코 + wrapper + 호출당 ~80B)은 동일하게 적용된다.
측정 하네스: `target/measure_envelope.ts` (`bun target/measure_envelope.ts <exe> <label>`).

### 스모크 검증 (debug build, 실제 JSON-RPC 호출)

- compact 성공: envelope `{data, durationMs}`, per-item `{index, ok, data}` — input echo·results 배열 없음 확인.
- compact 실패(존재하지 않는 파일 read): `error.message` + `isError` + per-item `{index, ok:false}` 확인.
- full(`RUST_FS_MCP_COMPACT=0`): envelope key `data,durationMs,error,schemaVersion,status,toolName`,
  per-item key `index,input,ok,result` 복원 확인.
- `cargo test` 24 unit + 2 integration 전부 통과, `cargo clippy --all-targets` 경고 0.

---

## 5. 리스크

* compact 응답에서 `input` 에코가 사라지므로 응답만 보고 요청을 복원할 수 없다.
  디버깅 시 `RUST_FS_MCP_COMPACT=0`으로 전체 envelope를 복원한다.
* batch `results[]` 구조가 `{index, ok, data}`로 바뀌므로 구버전 구조를 파싱하는 외부 스크립트가 있다면 영향.
  확인 범위(tests, 문서) 내 소비자는 모두 갱신했다.
