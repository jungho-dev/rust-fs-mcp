# fs-mcp vs rust-fs-mcp 벤치마크 보고서

- 작성일: 2026-05-24
- 범위: 임시 대용량 파일 기반 MCP 파일 작업 성능 비교
- 비교 대상: `fs-mcp`, `rust-fs-mcp`
- 원시 데이터: `C:/Users/jungh/.codex/.docs/fs-mcp-vs-rust-benchmark-2026-05-24-results.json`

## 1. 결론

확인됨: 전체 측정 샘플 60개와 워밍업 12개가 예외 없이 완료됐다. 유효 비교 작업 5개 기준으로
`rust-fs-mcp`는 래퍼 포함 wall time에서 5/5개 작업이 더 낮았고, MCP 내부 duration에서는
3/5개 작업이 더 낮았다.

확인됨: 토큰 소비는 작업별 출력 구조에 크게 좌우됐다. `rust-fs-mcp`는 디렉터리 목록, 메타데이터, 파일 복사에서
반환 토큰이 더 낮았지만, 2,000줄 읽기와 정규식 검색에서는 더 높았다.

확인됨: `file_read.offset/length` 의미가 두 서버에서 달라 직접 비교 항목에서 제외했다. `fs-mcp`는 해당 offset을
줄 번호로 해석해 0줄을 반환했고, `rust-fs-mcp`는 262,144자 slice를 반환했다.

## 2. 고정 작업과 입력

| 항목 | 값 |
| --- | --- |
| 임시 입력 루트 | `C:/Users/jungh/.codex/.tmp/fs-mcp-vs-rust-benchmark-2026-05-24` |
| 대형 텍스트 | `C:/Users/jungh/.codex/.tmp/fs-mcp-vs-rust-benchmark-2026-05-24/data/large_text_128m.txt` |
| 대형 텍스트 크기 | 134,217,796 bytes |
| 대형 바이너리 | `C:/Users/jungh/.codex/.tmp/fs-mcp-vs-rust-benchmark-2026-05-24/data/large_blob_128m.bin` |
| 대형 바이너리 크기 | 134,217,728 bytes |
| 소형 파일 | 512개 |
| 반복 수 | 작업별 도구당 5회 |
| 워밍업 | 작업별 도구당 1회, 집계 제외 |

성공 기준은 두 도구가 같은 작업 정의를 예외 없이 완료하고, 작업별 도구당 5개 측정 샘플을 확보하며, 속도와 반환 토큰을
같은 방식으로 기록하는 것이다.

## 3. 속도 결과

| 작업 | fs duration ms 평균 | rust duration ms 평균 | rust duration 차이 | fs wall ms 평균 | rust wall ms 평균 | rust wall 차이 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 512개 소형 파일 디렉터리 목록 | 7.2 | 12.8 | rust 77.8% 높음 | 44.4 | 32.2 | rust 27.5% 낮음 |
| 대형 2개 + 소형 50개 메타데이터 | 111.2 | 3.4 | rust 96.9% 낮음 | 141.6 | 20.8 | rust 85.3% 낮음 |
| 128MiB 텍스트 정규식 검색 | 145.8 | 152.2 | rust 4.4% 높음 | 173.0 | 170.0 | rust 1.7% 낮음 |
| 128MiB 바이너리 복사 | 79.0 | 74.6 | rust 5.6% 낮음 | 103.8 | 92.0 | rust 11.4% 낮음 |
| 128MiB 텍스트 중 2,000줄 읽기 | 126.8 | 80.0 | rust 36.9% 낮음 | 198.8 | 142.2 | rust 28.5% 낮음 |

해석: 내부 duration은 MCP 서버가 보고한 처리 시간이고, wall time은 Codex 래퍼에서 호출 전후로 측정한 전체 대기 시간이다.
디렉터리 목록은 내부 duration만 보면 `fs-mcp`가 낮지만, 래퍼 포함 wall time은 `rust-fs-mcp`가 낮았다.

## 4. 반환 토큰 결과

| 작업 | fs tokens 평균 | rust tokens 평균 | rust token 차이 |
| --- | ---: | ---: | ---: |
| 512개 소형 파일 디렉터리 목록 | 25,882 | 8,054 | rust 68.9% 낮음 |
| 대형 2개 + 소형 50개 메타데이터 | 21,561 | 13,295 | rust 38.3% 낮음 |
| 128MiB 텍스트 정규식 검색 | 4,043 | 4,642 | rust 14.8% 높음 |
| 128MiB 바이너리 복사 | 524 | 365 | rust 30.3% 낮음 |
| 128MiB 텍스트 중 2,000줄 읽기 | 162,318 | 229,750 | rust 41.5% 높음 |

토큰 값은 각 MCP 도구 요약에 표시된 `tokens = ... token` 값이다. 이는 모델 API의 input/output/cache token이 아니라
도구 반환 페이로드의 요약 토큰이다.

## 5. 직접 비교 제외 항목

| 작업 | fs duration ms 평균 | rust duration ms 평균 | fs tokens 평균 | rust tokens 평균 | 제외 사유 |
| --- | ---: | ---: | ---: | ---: | --- |
| 256KB 부분 읽기(의미 차이) | 106.4 | 166.6 | 366 | 66,903 | fs-mcp interpreted offset as line number and returned 0 lines at offset 67108864, while rust-fs-mcp interpreted offset/length as a 262144-char slice. |

이 항목은 실패가 아니라 API 의미 차이 발견이다. 같은 줄 범위 읽기 비교는 `file_lines_2000_large_text`로 별도 측정했다.

## 6. 작업별 판단

* 확인됨: `file_infos_52_paths`는 `rust-fs-mcp`가 내부 duration 평균 96.9% 수준으로 크게 낮았다.
* 확인됨: `file_copy_128m_blob`는 `rust-fs-mcp`가 wall time 기준 11.4% 낮았다.
* 확인됨: `search_regex_large_text`는 두 도구가 거의 비슷했다. 내부 duration은 rust가 +4.4%였고, wall time은 -1.7%였다.
* 확인됨: `file_lines_2000_large_text`는 `rust-fs-mcp`가 더 빨랐지만 반환 토큰은 +41.5%로 더 많았다.
* 확인됨: `dir_list_512_small`는 `rust-fs-mcp`가 반환 토큰을 68.9% 줄였지만, 내부 duration은 더 높게 보고됐다.

## 7. 검증 결과

* 확인됨: 입력 생성 스크립트가 `large_text_128m.txt`, `large_blob_128m.bin`, 소형 파일 512개를 생성했다.
* 확인됨: 측정 샘플 60개 중 예외 또는 실패 샘플은 0개다.
* 확인됨: 파일 복사 측정 중 생성된 복사본은 측정 후 삭제했다.
* 미검증: OS 파일시스템 캐시는 제거하지 않았다. 따라서 결과는 워밍업 이후 warm-cache 성향이 있다.
* 미검증: 모델 API input/output/cache token, TTFT, 비용은 MCP 호출 결과에서 노출되지 않아 산정하지 않았다.

## 8. 변경 파일

* `.docs/fs-mcp-vs-rust-benchmark-2026-05-24.md`
* `.docs/fs-mcp-vs-rust-benchmark-2026-05-24-results.json`
* `.tmp/fs-mcp-vs-rust-benchmark-2026-05-24/input-manifest.txt`
* `.tmp/fs-mcp-vs-rust-benchmark-2026-05-24/data/large_text_128m.txt`
* `.tmp/fs-mcp-vs-rust-benchmark-2026-05-24/data/large_blob_128m.bin`
* `.tmp/fs-mcp-vs-rust-benchmark-2026-05-24/data/small/*.txt`

## 9. 이슈 및 리스크

* 미검증: 서버 바이너리 버전 정보는 도구 결과에 노출되지 않았다.
* 미검증: 단일 로컬 Windows 환경 결과이므로 다른 디스크, OS, MCP 서버 설정에서는 수치가 달라질 수 있다.
* 추론: 대량 반환 작업에서는 서버 처리 속도보다 반환 구조와 직렬화 크기가 토큰 소비와 wall time에 더 큰 영향을 줄 수 있다.

## 10. 후속 작업

* cold-cache 비교가 필요하면 OS 재부팅 또는 캐시 정리 절차를 별도 고정 조건으로 둔다.
* 서버 버전 확인 명령이 제공되면 보고서에 버전과 빌드 정보를 추가한다.
* 실제 사용 패턴이 정해지면 해당 작업 조합으로 반복 수를 늘려 재측정한다.

## 11. 근거

* 확인됨: `C:/Users/jungh/.codex/.docs/fs-mcp-vs-rust-benchmark-2026-05-24-results.json`의 원시 측정 JSON.
* 확인됨: MCP 도구 결과의 `durationMs`, `tokens`, `contents`, `structuredText` 요약값.
* 확인됨: 입력 manifest `C:/Users/jungh/.codex/.tmp/fs-mcp-vs-rust-benchmark-2026-05-24/input-manifest.txt`.
