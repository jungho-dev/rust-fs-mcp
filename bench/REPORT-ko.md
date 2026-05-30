# rust-fs-mcp vs 네이티브(pwsh·내장툴) 벤치마크 보고서

## 0. 결론 요약

- **단일 변수 비교**: 동일한 6개 고정 작업을 `rust-fs-mcp`(stdio MCP, 세션과 동일 바이너리) 경로와 네이티브(pwsh cmdlet / `rg`·`git` CLI) 경로로 각각 측정.
- **지연(latency)**: 승패가 작업별로 갈림. 다중 항목·라인범위·메타데이터 배치(T2/T3/T5)는 MCP가 **4~13배 빠름**. 단순 전체 읽기(T1)와 git 상태(T6)는 네이티브가 **1.7~1.8배 빠름**. 검색(T4)은 사실상 동률(둘 다 `rg`).
- **LLM 토큰(컨텍스트 소비)**: **모든 작업에서 MCP가 네이티브보다 큼**. compact ON 기준 합계 **2.2배**, OFF 기준 **4.3배**. 원인은 응답 구조 자체(본문 다중 복제 + `structuredContent` 직렬화 전달).
- **현재 세션의 실제 동작**: MCP 서버가 **compact OFF**로 돌고 있어(실측 근거: 응답에 `data.text` 존재) 실사용 토큰은 최악 구간(c0). `.mcp.json`에 `RUST_FS_MCP_COMPACT=1`만 추가하면 즉시 절반으로 감소.
- **종합 트레이드오프**: rust-fs-mcp는 **속도·배치·결정적 구조화 출력**을 얻는 대신 **LLM 토큰을 2~4배 더 쓴다**. 토큰이 병목이면 네이티브, 라운드트립·다중항목·구조화가 병목이면 MCP가 유리.

---

## 1. 벤치마크 설계

### 1.1 환경

| 항목 | 값 |
| --- | --- |
| CPU | 11th Gen Intel Core i5-11400H (6C/12T) |
| OS | Windows 11 (10.0.26200) |
| Shell | PowerShell 7.7.0-preview.1 |
| 측정 바이너리 | `C:\JUNGHO\1.Language\6.Rust\target\release\rust-fs-mcp.exe` (2026-05-30 11:13 빌드) |
| 세션 서버 동일성 | 실행 중인 MCP 프로세스 3개가 **동일 exe** 사용 → 측정값이 에이전트 실사용을 반영 |
| 외부 CLI | `rg` 14.x, `fd`, `git` (모두 PATH) |
| 반복 | 작업당 **워밍업 4회 + 측정 25회**, 독립 2회 실행에서 p50 재현 확인 |

### 1.2 측정 방법

- **MCP 경로**: `.NET Process`로 stdio 서버를 **상주(persistent)** 실행 → `initialize` 후 각 작업의 JSON-RPC 요청을 보내고 `Stopwatch`로 write→ReadLine 왕복 시간 측정. 프로세스 기동 비용은 측정에서 제외(상주 모델 = 실사용과 동일).
- **네이티브 경로**: 동일 pwsh 세션에서 `Stopwatch`로 감싸 실행 후 stdout을 `Out-String`으로 캡처. cmdlet(`Get-Content` 등)은 인터프리터 내 실행이라 프로세스 기동 비용 없음, 외부 CLI(`rg`/`git`)는 매 호출 프로세스 기동 포함(실사용 반영).
- **compact 변수**: MCP는 `RUST_FS_MCP_COMPACT=1`(ON)과 `=0`(OFF) 두 조건을 명시적으로 격리 측정.
- 하니스: `bench/run-bench.ps1`. 원자료: `bench/results.csv`, `bench/results.json`, `bench/samples.json`.

### 1.3 고정 작업(Fixed Task) 매트릭스

입력은 모두 이 저장소의 실제 파일/디렉토리로 고정.

| ID | 작업 | 입력 | MCP 도구 | 네이티브 등가 |
| --- | --- | --- | --- | --- |
| T1 | 파일 전체 읽기 | `src/tools/fs_tools.rs` (63KB / 1942줄) | `file-read` | `Get-Content -Raw` |
| T2 | 라인 범위 읽기 | 같은 파일 500–699행 | `file-lines` (offset/length) | `Get-Content \| Select -Skip -First` |
| T3 | 재귀 디렉토리 목록 | `src/` (depth 6) | `dir-list` | `Get-ChildItem -Recurse -Name` |
| T4 | 정규식 내용 검색 | `src/`, `pub\s+fn\s+\w+`, ctx 2 | `search-regex` | `rg -n -C 2 -g *.rs` |
| T5 | 메타데이터 배치 | `src/`의 .rs 16개 | `file-infos` (1콜) | `Get-Item` 파이프 |
| T6 | git 상태 | 저장소 루트 | `git-status` | `git status` |

> 동일 변수 원칙: 작업·입력·반복·성공 기준 고정, **경로(도구 스택)만** 변경.

---

## 2. 응답 구조 분석 — "transport 크기"와 "LLM 실소비"는 다르다

raw JSON-RPC 라인 길이를 그대로 비교하면 MCP에 과도하게 불리하거나 유리하게 왜곡된다. 실제 LLM이 소비하는 양을 분리해야 한다.

직접 호출로 확인한 Claude Code 동작:

- MCP 응답의 최상위 `result.content`는 **사람용 요약 302자(고정)** 뿐 — 파일 본문이 아니다.
- 실제 본문은 `result.structuredContent.data.content`에 담긴다.
- **Claude Code는 `result.structuredContent` 객체를 직렬화해 tool_result로 LLM에 전달**한다(`result.content`·`_meta`는 제외). 따라서 **LLM 실소비 = `result.structuredContent` 직렬화 크기**.

그 안에서 파일 본문이 다음과 같이 **중복 적재**된다(`file-read` 기준):

| 위치 | compact ON | compact OFF |
| --- | --- | --- |
| `data.content[].text` (요약+본문) | ✓ | ✓ |
| `data.text` (본문 사본) | — (제거) | ✓ |
| `data.structuredContent.results[].result.content[].text` | ✓ | ✓ |
| `data.structuredContent.results[].result.structuredContent.content` | ✓ | ✓ |
| **본문 사본 수** | **3** | **4** |

- 네이티브 pwsh는 본문을 **1회**만 싣는다 → MCP의 구조적 토큰 오버헤드의 근본 원인.
- compact 토글은 `data.text` 1개를 제거(4→3) → 정확히 **토큰 절반** 감소로 측정됨.
- 코드 근거: `src/core/response.rs:64-72`(기본 ON, `unwrap_or(true)`), `:97-99`(`if !compact_enabled() { data["text"]=... }`).

---

## 3. 측정 결과

### 3.1 지연(p50 / mean / p90, 단위 ms)

| Task | MCP c1 p50 | c1 mean | c1 p90 | 네이티브 p50 | nat mean | nat p90 | 승자(p50) |
| --- | --- | --- | --- | --- | --- | --- | --- |
| T1 read file | 1.00 | 1.31 | 1.31 | 0.60 | 1.63 | 2.43 | native 1.7× |
| T2 line range | 0.92 | 0.95 | 1.05 | 4.42 | 4.47 | 5.07 | **MCP 4.8×** |
| T3 dir list | 0.24 | 0.26 | 0.31 | 3.04 | 3.19 | 3.70 | **MCP 12.7×** |
| T4 search | 13.59 | 13.81 | 15.08 | 15.75 | 15.96 | 17.27 | MCP 1.16× |
| T5 meta×16 | 0.75 | 0.75 | 0.77 | 4.52 | 4.78 | 5.69 | **MCP 6.0×** |
| T6 git status | 46.75 | 46.66 | 48.43 | 26.58 | 26.65 | 27.97 | native 1.8× |

(compact OFF의 지연은 ON과 사실상 동일 — 응답 크기만 바뀌고 연산 경로는 같음.)

### 3.2 LLM 실소비 토큰(추정 = `structuredContent` bytes / 4)

| Task | MCP c1 | MCP c0 | 네이티브 pwsh | c1/native | c0/native |
| --- | --- | --- | --- | --- | --- |
| T1 read file | 33,154 | 66,205 | 15,757 | 2.10× | 4.20× |
| T2 line range | 4,511 | 8,012 | 1,445 | 3.12× | 5.54× |
| T3 dir list | 326 | 542 | 74 | 4.41× | 7.32× |
| T4 search | 16,000 | 31,818 | 7,446 | 2.15× | 4.27× |
| T5 meta×16 | 2,332 | 3,580 | 535 | 4.36× | 6.69× |
| T6 git status | 290 | 368 | 205 | 1.41× | 1.80× |
| **합계** | **56,613** | **110,525** | **25,462** | **2.22×** | **4.34×** |

### 3.3 T1 파일 읽기 — 모든 변형 비교(정보 등가, 가장 공정한 비교)

| 변형 | chars | est tok | 기준(내장 Read) 대비 |
| --- | --- | --- | --- |
| 네이티브 pwsh `Get-Content -Raw` | 63,029 | 15,757 | 0.82× |
| 내장 `Read`(`cat -n`, 라인번호 prefix) | 76,623 | 19,156 | 1.00× |
| MCP `file-read` compact ON | 132,618 | 33,154 | **1.73×** |
| MCP `file-read` compact OFF | 264,819 | 66,205 | **3.46×** |

> 내장 `Read`는 줄번호 접두사로 raw 대비 +21%. MCP는 본문 3~4중 복제로 내장 Read 대비 1.7~3.5배.

---

## 4. 작업별 해석

- **T1(읽기)**: 정보가 완전 등가(둘 다 파일 본문). MCP의 2.1~4.2배 토큰은 **순수 중복 오버헤드**. 지연은 둘 다 1ms대로 무의미한 차이.
- **T2(라인범위)**: MCP가 지연 4.8배 우위. 네이티브 `Get-Content | Select`는 전체 파일을 객체화 후 슬라이스라 느림. MCP는 네이티브 스트리밍. 단 토큰은 MCP가 3배.
- **T3(디렉토리)**: 지연 MCP 12.7배 우위. 단 토큰은 MCP 4.4배 — 다만 `dir-list`는 타입·크기 등 **구조 메타를 더 담으므로** 토큰차의 일부는 정보량 차이(네이티브 `-Name`은 경로만).
- **T4(검색)**: 둘 다 `rg`로 shell-out하므로 지연 동률. 토큰은 네이티브 `rg`가 절반(MCP는 매칭 결과를 구조화·중복 적재).
- **T5(메타×16)**: 지연 MCP 6배 우위 + **1콜로 16개 배치**. 토큰은 MCP 4.4배지만 네이티브보다 풍부한 메타 포함.
- **T6(git status)**: 네이티브 `git` CLI가 1.8배 빠름. MCP는 git 객체를 직접 파싱하는 자체 구현이라 성숙한 git 최적화(인덱스 캐시 등)에 못 미침. 토큰은 근소차(1.4배).

---

## 5. 도구 호출 횟수(배치) 차원 — 측정 외 정성 분석

지연·토큰 외에 **라운드트립 수**가 실사용 비용을 좌우한다.

- **MCP**: batch-first. T5처럼 16개 메타/읽기를 **1콜**로 처리 → LLM 턴 1회.
- **내장 `Read`**: 1콜 = 1파일이 일반적. 16파일이면 **16콜 = 16 라운드트립**(각 라운드트립은 모델 추론 + 도구 왕복). 토큰은 적어도 턴 수에서 크게 불리.
- **pwsh**: `ForEach`로 1콜에 N개 처리 가능 → 배치 면에서 MCP와 대등, 토큰은 더 적음.

> 즉 "내장툴 vs MCP"에서는 MCP의 배치 이점이 크고, "pwsh vs MCP"에서는 pwsh가 토큰·배치 모두에서 경쟁력 있다.

---

## 6. compact 설정 이슈(즉시 조치 가능)

- `~/.claude/.mcp.json`의 `rust-fs-mcp` 정의에 **`env` 없음** → `RUST_FS_MCP_COMPACT` 미지정.
- 코드 기본값은 ON(`response.rs:71`)이나, **현재 세션 바이너리(2026-05-30 빌드)는 OFF로 동작**(실측: 응답에 `data.text` 존재). git 이력상 compact가 "opt-in(기본 off)"으로 도입된 뒤 워킹트리에서 기본 on으로 바뀌는 중이라, 빌드 시점 버전이 off.
- **결과**: 에이전트 실사용 토큰 = 표 3.2의 **c0 열(최악, 네이티브의 4.3배)**.
- **권고(설정 한 줄)**:

```json
"rust-fs-mcp": {
  "type": "stdio",
  "command": "C:\\JUNGHO\\1.Language\\6.Rust\\target\\release\\rust-fs-mcp.exe",
  "args": [],
  "env": { "RUST_FS_MCP_COMPACT": "1" }
}
```

→ 즉시 c1로 전환, MCP 토큰 **절반** 절감. (근본 절감은 §2의 본문 3중 복제 제거가 필요 — 코드 개선 영역.)

---

## 7. 권고 — 언제 무엇을 쓸까

| 상황 | 권고 |
| --- | --- |
| 토큰/컨텍스트가 병목, 대용량 파일 읽기 | **네이티브 pwsh raw** 또는 내장 `Read` |
| 여러 파일 메타/라인범위/디렉토리 한 번에 | **MCP**(배치·지연 우위) |
| 정규식 검색 | 지연 동률 → **토큰 우선이면 `rg` 직접**, 구조화 결과 필요하면 MCP |
| git 상태/diff | **`git` CLI**(빠르고 토큰 적음) |
| 권한·경로 제약(allowedDirectories) 강제 필요 | **MCP**(보안 경계 일관) |
| 즉시 가능한 토큰 절감 | `.mcp.json`에 `RUST_FS_MCP_COMPACT=1` 추가 |

---

## 8. 검증 노트(validity)

- ✅ 측정 바이너리 = 세션 MCP 서버와 동일 exe(프로세스 경로로 확인) → 결과가 실사용 반영.
- ✅ compact 변수는 env로 명시 격리, T1 c0(265K)≠c1(133K)로 토글 작동 확인.
- ✅ 25회×2 독립 실행에서 p50 안정(재현).
- ✅ LLM 실소비 정의를 직접 호출로 검증(Claude Code가 `structuredContent`를 전달).
- ⚠️ 비대칭: MCP는 상주(기동비용 0), 네이티브 외부 CLI(rg/git)는 매회 기동 포함 — 단 이는 **실사용과 동일**한 조건.
- ⚠️ 정보 등가는 T1만 완전. T3/T5는 MCP가 더 많은 구조 메타 포함 → 토큰차 일부는 정보량.

---

## 9. 잔여 불확실성

- 토큰은 **bytes/4 추정**(실제 토크나이저 아님). 상대 비교엔 충분, 절대값은 ±수% 오차.
- Claude Code의 정확한 tool_result 직렬화(공백/키 정렬)는 클라이언트 버전 의존. 여기선 `structuredContent` 객체 크기로 근사(`ConvertFrom/To-Json` 라운드트립).
- **프롬프트 캐싱 미반영**: 반복 호출 시 도구 결과가 cache-read 단가로 떨어지면 토큰 비용 체감이 달라질 수 있음(컨텍스트 점유 자체는 동일).
- 단일 머신·단일 저장소. 더 큰 트리·네트워크 FS·다른 OS에서 특히 T3/T6 결과가 달라질 수 있음.
- `git-status` 지연은 이 저장소의 `target/`(대량 빌드 산출물) 스캔 비용 영향 가능 — repo 형태 의존.

---

## 10. 재현 방법

```powershell
pwsh -NoProfile -File bench\run-bench.ps1 -Iterations 25 -Warmup 4
# 산출물: bench\results.csv, bench\results.json, bench\samples.json
```
