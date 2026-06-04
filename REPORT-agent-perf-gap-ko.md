# rust-fs-mcp × Claude Code 에이전트 성능 격차 분석 보고서

작성일: 2026-06-04. 분석 방법: 다중 에이전트 워크플로(37 에이전트) — 기존 벤치 보고서 8건 정독,
원시 stream-json 30 run 재파싱, src/ 전수 코드 분석, 외부 연구(2025–2026), 호스트 통합 메커니즘 검증,
전략 9종 생성 후 3-렌즈 적대적 검증.

질문: 왜 rust-fs-mcp를 붙여도 순정 Claude Code보다 에이전트 성능이 향상되지 않는가,
그리고 어떻게 해야 향상시킬 수 있는가.

---

## 1. 결론 요약

1. **소규모 대화형 작업에서 1:1 도구 대체로 built-in을 wall time으로 이기는 것은 구조적으로 불가능하다.**
   built-in은 in-process 함수 호출(스키마 상시 로드, IPC 없음)이고, MCP는 ToolSearch 스키마 로드 +1턴,
   stdio JSON-RPC 왕복, tool_use 출력토큰을 매번 지불한다. 4차례 개선 시도(compact·lean·composite·token-waste)가
   전부 이 구조를 못 넘었다.
2. **wall의 단일 지배 변수는 출력토큰이다(corr 0.968).** 응답 payload는 wall과 무상관(corr 0.036)이다 —
   tool_result는 input 토큰이고 ~99%가 cache_read로 흡수된다. 따라서 서버 측 응답 토큰 감축(이번
   token-waste 27.4% 감축 포함)은 **wall을 줄이지 못한다.** 가치는 비용·컨텍스트 축에만 있다.
3. **"성능이 향상되지 않는다"의 실제 손실 지점은 서버가 아니라 라우팅 정책이다.** built-in을 차단하지 않으면
   모델은 자발적으로 built-in을 선택한다(스모크: MCP 0회 호출). 현재 손해가 나는 곳은 FS.md의
   무조건 "rust-fs-mcp first" 강제 — 이것이 이길 수 없는 소규모 작업까지 MCP 경로로 보낸다.
4. **이기는 길은 세 가지뿐이다.** (a) built-in이 약한 호스트(Codex: ON이 wall 62.9% 단축, 이미 실증),
   (b) 미측정 영역인 대량 동질 배치(N≥수십 파일 read/edit) — 단 모델이 실제로 배치하는지부터 검증 필요,
   (c) wall이 아닌 축(토큰 비용, 긴 세션 컨텍스트 보존, 계약 일관성) — 가설 단계, 측정 필요.

---

## 2. 측정 사실

### 2.1 ON/OFF 벤치 (2026-05-29, Sonnet 4.6, 조건당 5 run, read-only inspection)

| 지표 | ON (rust-fs-mcp 강제) | OFF (built-in) | 차이 |
|------|----------------------|----------------|------|
| wall time 평균 | 39.144s | 28.141s | OFF 28.1% 빠름 |
| API time 평균 | 38.013s | 24.225s | OFF 36.3% 빠름 |
| 출력토큰 평균 | 1,965 | 1,267 | OFF 35.5% 적음 |
| 총 입력토큰 | 170,094 | 196,427 | ON 13.4% 적음 |
| 턴 / 도구 호출 | 8.2 / 7.2 | 8.6 / 7.6 | 유사 |
| 성공률 | 100% (5/5) | 100% (5/5) | 동률 |

ON 조건은 `--disallowedTools`로 built-in을 인위 차단해 강제한 것이다. 차단하지 않으면 모델은
built-in을 선호해 MCP를 0회 호출한다(스모크 실측).

### 2.2 개선 시도 4종 — 전부 wall 악화

| 시도 | 의도 | 결과 (동일시점 baseline 대비) |
|------|------|------------------------------|
| compact envelope | 응답 중복 제거 | bytes 18%↓ 달성, wall 47.3s→57.8s(22%↑), output 20%↑ |
| lean profile (22→19 도구) | 스키마 축소 | wall 57.8s→73.6s(27% 추가 악화), 호출 9.4→11.2 역증가 |
| composite (fs-inspect+git-status) | 턴 collapse | 턴 9.4→8.4↓ 성공, output 45%↑, wall 70.9s 악화 |
| token-waste (2026-06-04, 미커밋) | structured 27.4%↓ | **미벤치.** 기존 인과 모델상 wall 무효 예측 (3.2절) |

### 2.3 원시 30 run 재파싱 — 인과 모델 (이번 분석에서 신규 검증)

* `corr(wall, output_tokens) = 0.968` (모든 조건 분할에서 0.93+). 출력토큰이 단일 최강 예측 변수.
* `corr(wall, 응답 payload 총량) = 0.430`, 최대 payload 기준 0.036 — 사실상 무상관.
  OFF만 보면 −0.472로 오히려 음의 상관.
* 결정적 반례: 최저속 run(comp-5, 120.5s)은 payload가 작고(13.4k자) 출력토큰 최고(7,573).
  최고속 run 3개는 payload 32–34k자로 큰데도 출력토큰 571–878로 빠름.
* 입력의 ~99%가 cache_read(ON 평균 255,074). payload는 캐시에 흡수돼 신규 prefill 비용 ≈ 0.
* 출력토큰당 wall은 조건 무관 19–25ms로 안정 — MCP IPC 왕복 자체는 지배 비용이 아니다.
* **배치 미사용**: ON run의 file-read 계열 26회 호출 중 배치(2개 이상)는 1건. batch-first instruction이
  주입돼 있어도 모델은 `{paths:[단일]}`로 호출. search-regex는 스키마가 items[] 전용인데도 26/26 단건 —
  스키마 강제로도 안 바뀐다는 실측.
* ToolSearch 스키마 로드: ON run당 1–3회(평균 1.6), 각 1턴 소비. 단 출력토큰 환산 시 wall 기여는 수 초 미만.

### 2.4 Codex 대조 (2026-05-28)

ON 성공률 66.7% vs OFF 33.3%, wall ON 29.2s vs OFF 78.6s — **ON 압승**. OFF 실패 원인은 shell 실행
정책 차단. rust-fs-mcp의 실익은 호스트의 built-in fs 능력에 반비례한다는 핵심 명제의 실증.

---

## 3. 원인 분석 — 왜 향상이 없는가

### 3.1 L1: 호스트 구조 비대칭 (불변 제약)

built-in Read/Grep/Edit은 Claude Code 프로세스 내부 함수다: 스키마 상시 로드, IPC 없음, 권한·정규화
오버헤드 없음. MCP 도구는 deferred 스키마(ToolSearch +1턴) + stdio JSON-RPC 왕복 + tool_use 출력토큰을
누적 지불한다. 도구 선택·병렬 호출·structuredContent 처리 전부 호스트 소관이라 서버가 개입할 수 없다.
CLI 바이너리 검사로 확인: 모든 MCP 도구는 기본 deferred(`isMcp===true → deferred`)이며 per-tool
`anthropic/alwaysLoad`(v2.1.121+)만 면제 가능.

### 3.2 L2: 인과 모델 오인 — 서버 측 토큰 최적화는 표적이 아니다

1차 가설("응답 envelope 중복 = 병목")은 반증됐다. wall = f(출력토큰)이고 payload는 cache_read로
흡수된다. compact·lean·composite·token-waste 전부 input/payload를 깎는 작업이라 wall에 무효하거나
역효과였다. **미커밋 token-waste 개선(27.4% 감축)도 같은 이유로 wall 개선은 기대할 수 없다** —
가치 주장을 토큰 비용·컨텍스트 축으로 한정해야 한다. 정작 지배 변수인 출력토큰을 직접 겨냥한
조치(모델이 결과를 재서술하지 않게 하는 프롬프트·응답 형상)는 한 번도 시도되지 않았다(5절 권고 5).

### 3.3 L3: 모델 행동 — 서버의 핵심 레버(배치)가 실제로는 안 쓰인다

rust-fs-mcp의 이론적 우위는 "N건을 1콜로"인데, 모델은 instruction·스키마 강제에도 26/26 단건 호출했다.
검색 세션 도구(search-start/get)도 0회 선택 — 비표준 도구는 훈련 분포 밖이라 모델이 수렴하지 않는다.
단, 중요한 교란: **벤치 태스크 자체가 이질적 단건 순차 작업(파일 2개+검색 1개+git 1개)이라 배치할
동질 아이템이 거의 없었다.** "모델이 배치를 안 한다"가 모델 결함인지 태스크 한계인지 미분리 상태다.

### 3.4 L4: 비교 기준의 함정 — 실사용 손실 지점은 FS.md

ON 39–73s 수치는 `--disallowedTools` 강제 레짐의 합성 경로다. 실사용 기본(미차단)에서는 모델이
built-in으로 자발 수렴하므로 "순정 대비 느려짐"도 "빨라짐"도 없다. 그런데 사용자 환경의 FS.md는
"로컬 파일 작업은 rust-fs-mcp first"를 무조건 강제한다 — 대화형 세션에서 이길 수 없는 소규모 단건
작업까지 MCP 경로로 보내는 유일한 실손실 지점이 바로 이 정책이다. 반대로 Codex에서는 같은 정책이
필수다(built-in 부재). 즉 정책이 호스트 분기 없이 전역 단일이라는 것이 문제의 핵심이다.

---

## 4. 전략 검증 결과 (9종 생성 → 3-렌즈 적대 검증)

각 전략을 벤치 정합·호스트 실현성·순이익 3개 렌즈로 독립 검증했다. 다수결 기각(killed)이어도
기각 사유 자체가 원인 분석의 일부다.

### 4.1 생존 (조건부 유효)

| 전략 | 판정 | 조건 |
|------|------|------|
| 워크로드-클래스 게이트 라우팅 (FS.md 재설계) | conditional | 호스트 분기 필수. Claude 스코프에서만 게이트 적용, Codex 스코프는 MCP-first 유지. 전역 재작성은 Codex를 깨므로 금지 |
| 벤치 재설계 (축소형) | conditional | 전면 게이트는 통계적으로 무리(10% delta 검출에 그룹당 ~152 run 필요). 유일한 미측정 칸인 "대량 동질 edit/write 1-arm"만 측정 가치 있음 |
| 비-wall 가치축 (컨텍스트·정확도) | conditional | 가설 단계. 긴 세션 auto-compact 발생률·후반 정답률 직접 측정 전까지 주장 불가 |
| 못 이기는 클래스 영구 양보 | conditional | 양보 부분은 타당(손실 회피). 단 도구 표면 통합(consolidate)은 lean 실패 전례가 있어 라우팅 변경과 동시 적용일 때만 |

### 4.2 기각 (사유가 곧 교훈)

| 전략 | 기각 사유 (핵심) |
|------|------------------|
| 배치-전용 스키마 강제 | search-regex가 이미 items[] 전용인데 26/26 단건 — 스키마 레버는 이미 실패 실증. "description 2KB 절단" 전제도 실측 결과 거짓 |
| 고-fan-out 배치 클래스 | composite 함정 재현 위험: 턴↓여도 N-항목 인자·합본 응답으로 output이 N에 비례 증가. 단 critic 지적대로 **공정한 검정 자체가 미실시**(태스크에 동질 다중 아이템 부재) — "기각"이 아니라 "미검증"에 가까움 |
| edit 응답에 ±K 컨텍스트 동봉 | 제거되는 verify-read 턴의 output 비용은 ~1s 수준으로 과대평가. edit-heavy 벤치 자체가 없어 효과 미관측. 모델이 inline 컨텍스트를 신뢰해 재읽기를 생략하는지 미입증 |
| 검색 세션 collapse | 모델이 세션 도구를 0회 선택(on-2는 search-regex 7회 반복하면서도 세션 미사용). 25k 초과분 디스크 격리는 호스트 기능이라 서버 주장 무효 |
| alwaysLoad로 ToolSearch 제거 | 메커니즘은 실재(CLI 바이너리로 확인)하나 절감 상한(세션당 1–3초)이 측정 노이즈(per-run 14–121s)에 묻힘. 단 1회 측정으로 확인 가능한 저비용 실험이라 재검 여지 있음 |

---

## 5. 권고 — 실행 우선순위

1. **FS.md 호스트 분기 재설계** (즉시, 저비용, 유일한 확실 이득)
   * Claude Code 스코프: 단건 read/grep/단발 git → built-in 허용(현행 무조건 MCP-first 폐기).
     MCP는 동질 N건 배치, allowMissing, 라인 정밀 편집, args_path 대형 인자에서만 우선.
   * Codex 등 built-in 취약 호스트 스코프: 현행 MCP-first 유지.
   * built-in 차단(`--disallowedTools`)은 어떤 경우에도 사용하지 않음.
2. **token-waste 개선(미커밋)의 가치 재명명 후 커밋** — "wall 개선"이 아니라 "토큰 비용 27.4%↓,
   컨텍스트 점유↓"로 문서화. 벤치 증거상 wall 효과 주장은 하지 말 것.
3. **유일한 빈 칸 1-arm 측정**: 동질 대량 작업(예: 30–50파일 일괄 read 또는 치환)을
   동일시점 인터리브, output_tokens 1차 KPI(ms/out_tok CV 7%로 유일하게 신뢰 가능),
   wall은 보조 지표로 측정. 모델이 실제 배치하는지(`items[]` 다건 수렴)가 1차 관찰 대상.
   여기서 지면 "Claude에서 wall 승리"는 영구 포기가 합리적.
4. **저비용 보조 실험 2건**: (a) per-tool `anthropic/alwaysLoad`로 ToolSearch 턴 소멸 여부 1회 확인,
   (b) "등록만 ON"(차단 없음) 3rd arm으로 자율 수렴 대비 명시 라우팅의 순이득 정량화.
5. **출력토큰 직접 겨냥 조치** (서버 아닌 프롬프트/정책 측): MCP 결과 재서술 금지, 도구 결과 인용 시
   요약 강제 등 — 지배 변수(corr 0.968)를 직접 깎는 유일한 미시도 레버.
6. **포지셔닝 확정**: rust-fs-mcp의 주 전장은 built-in이 약한 호스트(Codex 류, 이미 압승 실증)와
   멀티 호스트 계약 일관성. Claude Code에서는 "성능 도구"가 아니라 "능력 보완 도구"(배치·세션·정밀편집)로
   포지셔닝.

---

## 6. 측정 신뢰도 / 면책

* 모든 wall 결론은 **Sonnet 4.6 단일 모델, 단일 소규모 read-only 태스크, headless(claude -p), n=5**에서
  나왔다. per-run 변동 14–121s, wall CV 25–36% — 10% delta 검출에는 그룹당 ~152 run이 필요하므로
  소수점 % 차이는 정밀도 과대표현이다. 방향성(OFF 우위, output 지배)만 신뢰 가능.
* 성공률 100% 동률은 "품질 동등"이 아니라 "태스크 천장효과로 변별 불가"다.
* SessionStart 훅(CLAUDE.md 재주입)이 nested run의 입력토큰을 부풀렸다 — 상대 비교만 유효.
* ON 실행이 항상 OFF보다 선행하는 비무작위 순서 — 캐시 워밍 편향 가능성.
* 대화형 세션, Opus/Haiku, edit-heavy·대량 배치 워크로드는 전부 미측정. 본 보고서의 "구조적 불가"
  결론은 측정된 레짐(소규모 read-only)에 한정되며, 5절 권고 3의 빈 칸이 측정되면 일부 수정될 수 있다.
* programmatic tool calling은 Claude API/Platform 전용으로 Claude Code CLI 미지원(inferred) —
  지원 시 턴 비용 구조가 바뀌므로 재평가 필요.

---

## 7. 근거 산출물

* 기존 벤치: `.docs/claude-onoff-benchmark-2026-05-29.md`, `.docs/claude-onoff-improvement-2026-05-29.md`,
  `.docs/claude-composite-bench-2026-05-29.md`, `.docs/codex-onoff-benchmark-2026-05-28.md`
* 원시 데이터: `.docs/claude-onoff-bench-2026-05-29*/` (30 run stream-json, 이번 분석에서 재파싱 검증)
* 외부 연구: `.docs/claude-mcp-perf-research-2026-05-29.md` + Anthropic tool-search/PTC/writing-tools 문서
* 토큰 감축 작업: `REPORT-token-waste-ko.md` (미커밋, wall 무효 예측 본 보고서 3.2절)

---

## 8. 적용 결과 (2026-06-04 권고 실제 반영)

권고 1·4 을 코드·설정으로 적용하고 실호스트로 검증했다.

| 항목 | 내용 | 검증 |
|------|------|------|
| 호스트별 instructions 분기 | `src/protocol/server.rs`: initialize의 `clientInfo.name`에 claude 포함 시 gate형 instructions(built-in 우선, batch/정밀만 MCP), 그 외(Codex 등)는 기존 batch-first 유지 | unit test + JSON-RPC 스모크: claude-code/Claude Desktop→gate, codex→batch-first |
| ToolSearch 세금 제거 | `src/protocol/catalog.rs`: `RUST_FS_MCP_ALWAYS_LOAD`(기본 file-read,search-regex,file-edit-lines)에 `_meta {"anthropic/alwaysLoad": true}` | headless 실측: file-read 직행 호출 성공, ToolSearch 0회, 2턴, 정답. 대조군 file-infos(deferred)는 직접 호출 불가 → 모델이 alwaysLoad 도구로 우회 |
| FS.md 게이트 라우팅 | `~/.claude/FS.md`: 무조건 MCP-first 폐기 → 워크로드 게이트(단건→built-in, 배치·allowMissing·라인편집·세션·args_path→MCP). `~/.codex/FS.md`는 별도 파일로 Codex MCP-first 무수정 | 파일 분리 확인(Codex 영향 없음) |
| 회귀 검증 | cargo test 26 unit(신규 2 포함)+2 integration 전부 통과, clippy 경고 0, release 빌드 배포(실행중 exe rename 우회) | 완료 |

미측정 잔여: wall 개선폭 정량화는 미수행(6절 신뢰도 한계상 n≥60 필요). 메커니즘 수준 검증만 완료 —
alwaysLoad는 세션당 ToolSearch 1–3턴 제거를 직접 확인했고, gate 라우팅 효과는 향후 일상 사용에서 관찰.
적용 후 각 Claude 세션에서 `/mcp` 재연결해야 신규 바이너리·instructions가 반영된다.
