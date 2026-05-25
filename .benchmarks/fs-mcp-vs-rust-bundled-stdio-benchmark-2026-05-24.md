# fs-mcp vs rust-fs-mcp bundled stdio 벤치마크 보고서

- 작성일: 2026-05-24
- 범위: project-bundled `rg.exe`, `fd.exe`, `bat.exe` 적용 후 성능 재측정
- 비교 대상: `fs-mcp` stdio 서버, `rust-fs-mcp` release stdio 서버
- 원시 데이터: `C:/JUNGHO/9.Workspace/2.Project/2.Node/rust-fs-mcp/.benchmarks/fs-mcp-vs-rust-bundled-stdio-benchmark-2026-05-24-results.json`

## 1. 결론

확인됨: 워밍업 12개와 측정 샘플 60개가 예외 없이 완료됐다. 직접 비교 가능한 5개 작업 기준으로 `rust-fs-mcp`는 MCP 내부 duration에서 4/5개, stdio wall time에서 4/5개 작업이 더 낮았다.

확인됨: bundled `rg.exe` 적용 후 128MiB 텍스트 정규식 검색은 `rust-fs-mcp`가 내부 duration 61.1%, wall time 61.2% 낮았다. 반대로 `dir-list`는 bundled `fd.exe` child process 비용 때문에 작은 디렉터리 목록에서 TS보다 느렸다.

확인됨: 반환 토큰은 3/5개 직접 비교 작업에서 `rust-fs-mcp`가 더 낮았다. `file-lines`는 더 빠르지만 구조화 반환 크기 때문에 토큰은 더 높다.

## 2. 고정 작업과 입력

| 항목 | 값 |
| --- | --- |
| 입력 루트 | `C:/Users/jungh/.codex/.tmp/fs-mcp-vs-rust-benchmark-2026-05-24` |
| 대형 텍스트 | `C:\Users\jungh\.codex\.tmp\fs-mcp-vs-rust-benchmark-2026-05-24\data\large_text_128m.txt` |
| 대형 텍스트 크기 | 134,217,796 bytes |
| 대형 바이너리 | `C:\Users\jungh\.codex\.tmp\fs-mcp-vs-rust-benchmark-2026-05-24\data\large_blob_128m.bin` |
| 대형 바이너리 크기 | 134,217,728 bytes |
| 소형 파일 | 512개 |
| 반복 수 | 작업별 도구당 5회 |
| 워밍업 | 작업별 도구당 1회, 집계 제외 |
| Rust 실행 파일 | `C:\JUNGHO\9.Workspace\2.Project\2.Node\rust-fs-mcp\target\release\rust-fs-mcp.exe` |

성공 기준은 두 stdio 서버가 같은 JSON-RPC `tools/call` 작업을 예외 없이 완료하고, 작업별 도구당 5개 측정 샘플을 확보하며, 속도와 반환 토큰을 같은 방식으로 기록하는 것이다.

## 3. 속도 결과

| 작업 | fs duration ms 평균 | rust duration ms 평균 | rust duration 차이 | fs wall ms 평균 | rust wall ms 평균 | rust wall 차이 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 512개 소형 파일 디렉터리 목록 | 4.8 | 26.6 | rust 454.2% 높음 | 12.2 | 27.8 | rust 127.9% 높음 |
| 대형 2개 + 소형 50개 메타데이터 | 85.4 | 9.0 | rust 89.5% 낮음 | 94.2 | 11.0 | rust 88.3% 낮음 |
| 256KB 부분 읽기(의미 차이) | 98.6 | 161.4 | rust 63.7% 높음 | 101.2 | 165.6 | rust 63.6% 높음 |
| 128MiB 텍스트 정규식 검색 | 194.8 | 75.8 | rust 61.1% 낮음 | 198.6 | 77.0 | rust 61.2% 낮음 |
| 128MiB 바이너리 복사 | 42.4 | 40.0 | rust 5.7% 낮음 | 44.4 | 41.0 | rust 7.7% 낮음 |
| 128MiB 텍스트 중 2,000줄 읽기 | 127.6 | 81.8 | rust 35.9% 낮음 | 167.2 | 93.0 | rust 44.4% 낮음 |

해석: duration은 MCP 서버가 보고한 처리 시간이고, wall time은 stdio request write부터 response line read까지의 측정값이다. `dir-list`처럼 결과가 작고 작업 자체가 짧은 경우에는 `fd.exe` 프로세스 실행 비용이 우세했다.

## 4. 반환 토큰 결과

| 작업 | fs tokens 평균 | rust tokens 평균 | rust token 차이 |
| --- | ---: | ---: | ---: |
| 512개 소형 파일 디렉터리 목록 | 25,885 | 8,064 | rust 68.8% 낮음 |
| 대형 2개 + 소형 50개 메타데이터 | 21,717 | 13,399 | rust 38.3% 낮음 |
| 256KB 부분 읽기(의미 차이) | 369 | 66,904 | rust 18,031.2% 높음 |
| 128MiB 텍스트 정규식 검색 | 4,618 | 4,715 | rust 2.1% 높음 |
| 128MiB 바이너리 복사 | 542 | 369 | rust 31.9% 낮음 |
| 128MiB 텍스트 중 2,000줄 읽기 | 162,321 | 229,760 | rust 41.5% 높음 |

토큰 값은 MCP 도구 요약에 표시된 `tokens = ... token` 값이다. 모델 API input/output/cache token, TTFT, 비용은 로컬 stdio MCP 호출에서 노출되지 않아 측정하지 않았다.

## 5. Bundled Backend 확인

| 작업 | rust-fs-mcp backend |
| --- | --- |
| 512개 소형 파일 디렉터리 목록 | bundled-fd |
| 128MiB 텍스트 중 2,000줄 읽기 | bundled-bat |
| 128MiB 텍스트 정규식 검색 | bundled-rg |

확인됨: search는 `bundled-rg`, files/dir listing은 `bundled-fd`, line range read는 `bundled-bat` backend로 응답했다.

## 6. 직접 비교 제외 항목

| 작업 | 제외 사유 |
| --- | --- |
| 256KB 부분 읽기 | `fs-mcp`와 `rust-fs-mcp`의 `file-read offset/length` 의미가 달라 직접 성능 비교에서 제외한다. line range 비교는 `file_lines_2000_large_text`로 판단한다. |

## 7. 검증 결과

- 확인됨: `cargo +1.93.0-x86_64-pc-windows-gnu fmt --check`, `cargo +1.93.0-x86_64-pc-windows-gnu clippy --all-targets --all-features -- -D warnings`, `cargo +1.93.0-x86_64-pc-windows-gnu test`, `cargo +1.93.0-x86_64-pc-windows-gnu build --release`가 통과했다.
- 확인됨: `target/release/tools/win32-x64/rg.exe`, `fd.exe`, `bat.exe`가 release build output에 존재한다.
- 확인됨: release stdio smoke test에서 `search-regex`, `searchType=files`, `dir-list`, `file-lines`가 각각 bundled backend를 반환했다.
- 확인됨: `dir-list depth=1`은 immediate entry만, `depth=2`는 nested file까지 반환했다.
- 확인됨: 재벤치마크 invalidRuns는 0개다.
- 확인됨: 기본 `stable-x86_64-pc-windows-gnu` rustup alias는 manifest가 없어 명시적 `1.93.0-x86_64-pc-windows-gnu` toolchain으로 검증했다.
- 미검증: OS 파일시스템 캐시는 제거하지 않았다. 결과는 warm-cache 성향이 있다.
- 미검증: 모델 API token, TTFT, 비용은 로컬 MCP response에 없어 산정하지 않았다.

## 8. 변경 파일

- `.benchmarks/run-stdio-benchmark.ps1`
- `.benchmarks/fs-mcp-vs-rust-bundled-stdio-benchmark-2026-05-24-results.json`
- `.benchmarks/fs-mcp-vs-rust-bundled-stdio-benchmark-2026-05-24.md`
- `.gitignore`
- `README.md`
- `README-ko.md`
- `ARCHITECTURE.md`
- `ARCHITECTURE-ko.md`
- `build.rs`
- `src/core/bundled.rs`
- `src/core/mod.rs`
- `src/lib.rs`
- `src/tools/fs_tools.rs`
- `src/tools/search_tools.rs`
- `tests/tool_matrix.rs`
- `vendor/tools/win32-x64/rg.exe`
- `vendor/tools/win32-x64/fd.exe`
- `vendor/tools/win32-x64/bat.exe`

## 9. 이슈 및 리스크

- 확인됨: `dir-list`는 bundled `fd.exe` 방식에서 작은 입력 기준 TS보다 느리다. 이 경로는 child process startup 비용이 병목이다.
- 추론: 대량 검색처럼 기존 Rust 도구 자체가 최적화된 작업은 bundled `rg.exe`가 명확히 유리하다.
- 미검증: 다른 디스크, cold-cache, 다른 MCP client wrapper에서는 wall time이 달라질 수 있다.

## 10. 근거

- 확인됨: `C:/JUNGHO/9.Workspace/2.Project/2.Node/rust-fs-mcp/.benchmarks/fs-mcp-vs-rust-bundled-stdio-benchmark-2026-05-24-results.json`
- 확인됨: `.benchmarks/run-stdio-benchmark.ps1` 실행 결과
- 확인됨: release build output의 bundled exe file metadata
