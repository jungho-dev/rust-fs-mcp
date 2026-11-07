# Architecture

## 소스 트리

```markdown
rust-fs-mcp/
├── src/
│   ├── main.rs : 실행 진입점, stdio 서버 시작
│   ├── lib.rs : core, protocol, tools 모듈 공개 경로
│   ├── protocol/
│   │   ├── mod.rs : protocol 모듈 묶음
│   │   ├── server.rs : JSON-RPC 줄 단위 루프, 버전 협상, 호스트별 instructions, ping, tools/call 동시 처리
│   │   └── catalog.rs : 도구 이름, title, 설명, 입력 스키마, annotation
│   ├── tools/
│   │   ├── mod.rs : 도구 이름 분기, 기존 모듈 경로 alias
│   │   ├── arg_shape.rs : 분기 전 인자 형태 보정
│   │   ├── common.rs : 도메인 공통 인자 읽기, 경로 규칙
│   │   ├── fs/
│   │   │   ├── mod.rs : fs 도메인 묶음, 핸들러 교차 테스트
│   │   │   ├── read.rs : file-read, 인코딩, 이미지, isUrl 읽기
│   │   │   ├── lines.rs : file-read-line-range
│   │   │   ├── write.rs : file-write, 임시 파일 원자적 쓰기
│   │   │   ├── dir.rs : dir-create, dir-list, 제외 패턴
│   │   │   ├── path_ops.rs : path-copy, path-move, path-remove, path-stat
│   │   │   ├── edit.rs : file-edit, file-edit-lines
│   │   │   └── shared.rs : 항목 읽기, 변경 경로 추출
│   │   ├── search/
│   │   │   ├── mod.rs : fs-search 핸들러, 엔진 선택
│   │   │   ├── matcher.rs : regex, fancy-regex 매처, smart-case
│   │   │   ├── walk.rs : 병렬 탐색, grep sink, 예산
│   │   │   └── tests.rs : fs-search 종단 테스트
│   │   ├── git/
│   │   │   ├── mod.rs : git 도메인 묶음
│   │   │   ├── repo.rs : git CLI 실행, toplevel 캐시
│   │   │   ├── worktree.rs : git-status, git-add
│   │   │   ├── commit.rs : git-commit, git-amend, 메시지 검사
│   │   │   └── inspect.rs : git-diff, git-show
│   │   └── web/
│   │       ├── mod.rs : web 도메인 묶음, 공통 필드 읽기
│   │       ├── fetch.rs : web-fetch
│   │       ├── extract.rs : web-extract
│   │       └── download.rs : download-to-file
│   └── core/
│       ├── mod.rs : core 모듈 묶음
│       ├── args_ref.rs : args_path 인자 파일 처리
│       ├── batch.rs : batch 실행기, worker pool, batch 결과 형식
│       ├── client.rs : MCP 호스트 판별, 호스트별 결과 정책
│       ├── config.rs : 홈 경로 확장, 경로 정규화
│       ├── encoding.rs : UTF-16, EUC-KR/CP949 디코딩과 재인코딩
│       ├── external.rs : 시간 제한을 둔 git CLI 실행
│       ├── response.rs : RawResult, 응답 envelope, 호스트별 출력 예산
│       ├── settings.rs : config.toml 로더
│       └── web.rs : HTTP 클라이언트, SSRF guard, HTML 추출
├── tests/
│   ├── tool_matrix.rs : catalog의 모든 도구를 dispatch로 호출
│   └── performance.rs : 기본 제외된 로컬 시간 측정
├── bench/
│   ├── README.md : 벤치마크 사용법
│   ├── mcp-bench.ts : 정답 검증을 포함한 stdio 종단 벤치마크
│   ├── ab.ts : 두 바이너리 교차 A/B 비교
│   ├── probe.ts : 원본 결과 형태 덤프
│   └── probe-search.ts : fs-search 메타데이터 확인
├── docs/
│   ├── claude/FS.md : Claude Code 라우팅 가이드
│   ├── codex/FS.md : Codex 라우팅 가이드
│   ├── copilot/FS.md : GitHub Copilot 라우팅 가이드
│   ├── gemini/FS.md : Gemini CLI 라우팅 가이드
│   └── opencode/FS.md : OpenCode 라우팅 가이드
├── .github/workflows/
│   ├── ci.yml : 타깃별 fmt, clippy, build, test, MSRV 검사
│   └── release.yml : 타깃별 릴리스 아카이브와 체크섬
├── .cargo/config.toml : 로컬 링커, 테스트 환경 격리
├── .cmd/
│   ├── build.cmd : Windows 릴리스 배포
│   └── publish.cmd : crates.io 배포
├── build.rs : gnullvm 타깃용 libunwind.dll 복사
├── Cargo.toml : 패키지 메타데이터, 의존성
├── README-KO.md : 사용 안내
└── LICENSE.md : Apache-2.0
```
