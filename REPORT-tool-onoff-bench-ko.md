# rust-fs-mcp 툴별 ON/OFF 벤치마크 보고서 (2026-06-05)

## 1. 결론 요약

- 툴별 단일 목적 태스크 10종 × ON/OFF × 3회(총 60 run, 전부 정상 종료) 비교.
- **전체: ON(rust-fs-mcp 강제)이 wall 평균 10.1s vs OFF(built-in) 14.3s — ON이 29% 빠름.**
  median 기준 10케이스 중 9케이스에서 ON 우위(유일한 예외 c5는 사실상 동률).
- 정확도: ON 30/30 성공, OFF 27/30 — OFF 실패 3건은 전부 c1(built-in `Read`의
  trailing-newline 라인번호 아티팩트로 line_count 241 보고, 정답 240).
- **순수 API 시간은 동률(ON 9.9s vs OFF 9.8s).** wall 격차는 전부 로컬 툴 실행 시간 —
  ① OFF가 Bash를 쓰는 케이스(c8/c9/c10)의 셸 스폰 ~13–15s/콜, ② OFF의 직렬 멀티턴
  (read-before-edit 게이트, 단건 Read 반복)에서 발생.
- 2026-05-29 혼합 태스크 벤치(ON 39–73% 느림)와 **정반대 결과지만 모순 아님**:
  이번 태스크는 전부 "동질 배치로 collapse 가능한" 형태이고, alwaysLoad 적용 후라
  ToolSearch 턴이 대부분 사라짐. "MCP는 다단계를 1콜로 collapse할 때만 이긴다"는
  기존 외부 연구 결론을 툴별로 실측 확증한 것.
- ON이 명확히 지는 축 1개: **git 다중 조회(c10)** — 토큰 4.4×, 비용 ~6×, 턴 3.5×.
  `git-show`가 `stat: true`에도 전체 패치를 반환하는 서버측 결함이 주범.

## 2. 벤치마크 정의

| 항목 | 값 |
|------|-----|
| 비교 변수 | rust-fs-mcp 강제(ON) vs built-in 전용(OFF) — 단일 변수 |
| 모델 | claude-sonnet-4-6 (양쪽 동일) |
| 하니스 | Claude Code CLI 2.1.163, `claude -p --output-format stream-json --verbose` |
| 서버 | rust-fs-mcp 0.1.2 (2026-06-04 빌드, alwaysLoad: file-read/search-regex/file-edit-lines) |
| ON 강제 | `--disallowedTools "Read Edit Write MultiEdit Grep Glob Bash PowerShell ..."`, ToolSearch 허용 |
| OFF | 빈 mcpServers + `--strict-mcp-config`, `--disallowedTools "Task WebFetch WebSearch"` |
| 반복 | 케이스당 조건별 3회, 라운드 내 ON/OFF 인접 실행, 라운드마다 선후 교대 |
| 판정 | 고정 기대값 자동 채점(JSON 정확 일치), 변이 케이스(c6/c7)는 파일시스템 사후 검증 |
| 환경 | Windows 11, 동일 호스트, 2026-06-05 15:17–15:30 단일 배치(801s), 직렬 실행 |
| 재현물 | `.docs/tool-bench-2026-06-05/` (setup.mjs, run.sh, parse.mjs, logs/, runs.json) |

케이스 정의:

| ID | 대상 툴(ON) | 태스크 | OFF 경로 |
|----|------------|--------|----------|
| c1 | file-read 단건 | 1파일 라인수 + 매직라인 | Read 1회 |
| c2 | file-read 배치 | 6파일 매직라인 수집 | Read 6회 |
| c3 | file-lines | 4파일 특정 라인 추출 | Read offset/limit 8회 |
| c4 | search-regex 단건 | 함수 정의 위치 탐색 | Grep 1회 |
| c5 | search-regex 배치 | 4패턴 파일수 집계 | Grep 4회 |
| c6 | file-edit-lines 배치 | 3파일 라인 치환(변이) | Read 3 + Edit 3 |
| c7 | file-write 배치 | 5파일 생성(변이) | Write 5회 |
| c8 | file-read allowMissing | 10경로 존재 프로브(4개 부재) | Bash 루프 1회 |
| c9 | 디렉터리 나열 | 4디렉터리 .rs 카운트 | Bash ls+wc 1회 |
| c10 | git-status/show | 브랜치+커밋3+HEAD 파일 | Bash git 1회 |

## 3. 결과표 (조건별 3-run 평균)

| ID | wall ON | wall OFF | ON/OFF | turns ON/OFF | out_tok ON/OFF | 콜수 ON/OFF | 성공 ON/OFF | 승자 |
|----|--------:|---------:|-------:|--------------|----------------|-------------|-------------|------|
| c1 | 5.1s | 5.5s | 0.93 | 2 / 2 | 167 / 154 | 1 / 1 | 3/3 / **0/3** | ON(정확도) |
| c2 | 9.0s | 11.5s | 0.78 | 2 / 7 | 518 / 672 | 1 / 6 | 3/3 / 3/3 | ON |
| c3 | 11.5s | 23.1s | 0.50 | 2.7 / 9 | 593 / 1501 | 1.7 / 8 | 3/3 / 3/3 | **ON** |
| c4 | 6.2s | 6.9s | 0.89 | 2 / 2 | 167 / 187 | 1 / 1 | 3/3 / 3/3 | 동률 |
| c5 | 8.9s | 8.8s | 1.01 | 2 / 5 | 477 / 570 | 1 / 4 | 3/3 / 3/3 | 동률 |
| c6 | 7.9s | 12.7s | 0.63 | 2 / 7 | 400 / 754 | 1 / 6 | 3/3 / 3/3 | **ON** |
| c7 | 12.6s | 12.5s | 1.00 | 3 / 6 | 578 / 648 | 2 / 5 | 3/3 / 3/3 | 동률 |
| c8 | 11.1s | 19.5s | 0.57 | 2 / 2 | 732 / 250 | 1 / 1 | 3/3 / 3/3 | ON(환경) |
| c9 | 7.1s | 22.1s | 0.32 | 2 / 2 | 274 / 242 | 1 / 1 | 3/3 / 3/3 | ON(환경) |
| c10 | 21.7s | 20.0s | 1.09 | 7 / 2 | 1071 / 246 | 6 / 1 | 3/3 / 3/3 | **OFF** |

전체 합계: ON wall 303s · out 평균 498tok · 비용 $1.93 / OFF wall 428s · out 평균 522tok · 비용 $1.73.
비용은 cache_creation 편차(콜드/웜)가 커서 참고치만 — 토큰·턴 수가 신뢰 지표.

## 4. 케이스별 메커니즘 (로그 감사 결과)

전 60 run의 stream-json 로그를 케이스·조건별로 전수 감사(20 agent)하고
집계 재계산·기대값 재검증·시점 드리프트 검사(3 agent)를 별도 수행했다.

### 승부가 갈린 메커니즘 5가지

1. **배치 collapse가 턴을 줄인다 (c2, c3, c6 — ON 압승).**
   ON은 전 케이스에서 1개의 items[]/paths[] 배치 콜로 수렴(c2: 6경로 1콜, c5: 4패턴
   1콜, c6: 3편집 1콜). OFF는 같은 작업을 직렬 멀티턴으로 수행(어시스턴트 메시지당
   tool_use 1개씩). in-process 툴 실행은 양쪽 다 ms 단위라 wall ≈ 턴수 × 턴당 추론
   지연. c6은 built-in Edit의 read-before-edit 게이트 때문에 OFF가 Read 3회를
   강제로 추가 지불(6콜/7턴 vs 1콜/2턴).

2. **Windows Bash 스폰 비용 ~13–15s/콜 (c8, c9, c10 OFF).**
   OFF가 Bash를 쓴 9개 run 전부에서 wall − api 격차 13–15s가 일관 관측
   (예: c9-off-1 wall 21.7s, api 6.8s). built-in fs 툴(Read/Grep/Edit/Write)을 쓴
   OFF run들은 격차 ~0.3s. 즉 c8/c9의 ON 승리는 MCP가 빨라서가 아니라 **OFF의 유일한
   경로(셸)가 이 호스트에서 비싸기 때문**. 셸이 빠른 환경이면 c8은 OFF가 이겼을 것
   (api 기준 OFF 6.2s < ON 10.8s). c9는 api 기준으로도 동률, c10은 api 기준 OFF 압승.

3. **structuredContent 메타데이터의 정확도 우위 (c1 — OFF 전패).**
   built-in `Read`는 trailing newline 뒤 빈 영역을 `241→`로 번호 매겨 렌더링하고,
   모델은 보이는 최대 라인번호를 그대로 보고(3/3 동일 실패). ON은
   `structuredContent.lineCount: 240`을 그대로 인용해 정답. 모델 산수 실수가 아니라
   **표현 형식이 유도한 체계적 오답**이며, 같은 형식을 쓰는 한 재발한다.

4. **ToolSearch 1턴이 배치 이득을 상쇄 (c7 동률, c10 악화).**
   file-write는 alwaysLoad 미지정이라 ON이 매 run ToolSearch 1턴을 선지불 →
   5파일 배치 이득이 정확히 상쇄돼 OFF와 동률(12.6 vs 12.5s). alwaysLoad된
   file-read/search-regex/file-edit-lines를 쓴 케이스는 ToolSearch 0회로 직행.
   c10은 ToolSearch 1–2회 + git 툴 다중 왕복으로 7–9턴까지 증가.

5. **git-show 페이로드 폭주 (c10 — ON 유일한 명확 패배).**
   `git-show HEAD stat:true`가 stat만이 아닌 **전체 패치 본문(18,415자)을 반환**,
   HEAD~2는 88,710자로 토큰 한도 초과 → 스필 파일 + 오류 통지 턴 낭비.
   ON 토큰 1071 vs OFF 246(4.4×), 비용 ~6×. OFF는 복합 git 원라이너 1콜(결과 203자)로
   동일 정답. 다중 리비전 조회는 git-show가 배치 불가라 리비전당 1왕복인 것도 불리.

### 동률 케이스의 의미

- c4(단건 검색)·c5(4패턴): OFF Grep이 이미 충분히 효율적(94자 결과, files_with_matches).
  배치 이득이 턴 4→1 감소로도 wall에 안 나타남 — 검색 결과가 작으면 턴당 비용 차가 미미.
- c7: 위 4번. **alwaysLoad 지정이 곧 승부 변수**임을 보여주는 대조군.

## 5. 기존 결론과의 정합성

| 항목 | 2026-05-29 혼합 벤치 | 본 벤치 (2026-06-05) |
|------|---------------------|---------------------|
| 태스크 형태 | 이질 다단계 1개(탐색 포함) | 동질 단일 목적 10개(경로 명시) |
| ToolSearch | ON 매 run 1+회 | alwaysLoad로 대부분 0회 |
| 결과 | ON wall +39–73% | ON wall −29% (9/10 케이스 우위) |

두 결과를 잇는 단일 인과 모델: **wall ≈ (턴수 × 턴당 지연) + 로컬 툴 실행시간**.
혼합 태스크에서는 ON이 턴을 늘렸고(스키마 로드 + 이질 단건 순차 호출), 본 벤치에서는
ON이 턴을 줄였다(배치 collapse + alwaysLoad). MCP 자체가 빠르거나 느린 게 아니라
**호출 패턴이 턴수를 어느 쪽으로 움직이는가**가 결정한다. 당시 FS.md의 게이트 라우팅
("단건은 built-in, 2+ 동종 작업은 배치 MCP")은 양쪽 실측과 모두 부합했다.
(후속: 2026-06-05 본 벤치 결과를 근거로 ~/.claude/FS.md를 MCP-first 강제 + 오류 시에만
built-in 폴백으로 전환 — §8 권고 1은 이 결정으로 대체됨. 서버 instructions는 게이트형 유지.)

## 6. 서버 개선 후보 (실측 근거)

1. **git-show `stat: true` 결함 수정** — stat 요청에 전체 패치 반환(c10에서 18K/88K자
   페이로드 유발). diff-tree --name-only 급 출력으로 줄이면 c10 열위 대부분 해소. [최우선]
2. **file-write·git-status·git-show alwaysLoad 후보 검토** — c7 실측상 ToolSearch 1턴
   = 배치 이득 전액. 단 alwaysLoad 남발은 컨텍스트 비용이므로 사용 빈도 기준 선별.
3. **file-lines 미지원 파라미터 무시 문제** — c3-on-1이 미지원 `ranges` 키를 보냈는데
   서버가 조용히 무시하고 전체 파일 반환(240–320라인 덤프). unknown-key 거부나
   경고를 반환하면 모델이 즉시 교정 가능.

## 7. 유효성·한계

- **n=3/조건**: 방향성 판정용. 기존 분석 기준 wall 10% 차 검출엔 ~152 run 필요 —
  5s 이상 벌어진 c3/c6/c8/c9만 견고, c4/c5/c7/c10은 동률~경합 해석이 안전.
- **시점 드리프트 검증 완료**: 라운드 평균 12.5→13.5→13.8s(+10%)로 양 조건에 균등,
  outlier(>2× median) 0건, 승자 플립은 경합 4케이스에서만 발생. 비교 오염 없음.
- **합성 레짐**: ON은 `--disallowedTools` 강제 상태. 실사용(비차단)에서는 모델이
  built-in을 자발 선택하므로 이 수치는 "MCP를 쓰게 됐을 때"의 성능이지 채택률이 아님.
- **환경 특이성**: c8/c9 ON 승리와 c10 wall 동률은 이 호스트의 Bash 스폰 비용(13–15s)
  의존. 셸이 빠른 환경에선 c8·c10이 OFF 우위로 뒤집힐 수 있음(미검증 추론).
- **비용 수치**: cache_creation 콜드/웜 편차가 케이스 간 비교를 오염 — 합계만 참고.
- 집계 산식은 독립 재계산으로 일치 확인, 기대값 5종은 디스크/저장소 재검증 전부 PASS.

## 8. 권고

1. FS.md 게이트 라우팅 유지 — "2+ 동종 작업 = 배치 MCP" 실측 정당화 완료.
2. git 다중 조회는 Claude Code에서 Bash git 우선(MCP git은 단건 조회·정형 결과용).
   git-show stat 결함 수정 전까지 다중 리비전 조회에 MCP git 비권장.
3. §6의 서버 개선 1번(git-show)을 우선 적용 후 c10만 재벤치하면 효과 격리 측정 가능.
