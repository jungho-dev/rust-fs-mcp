# Architecture

## 소스 트리

```markdown
rust-fs-mcp/
├── src/
│   ├── main.rs : 실행 진입점, stdio 서버 또는 --isolated-worker 작업 실행
│   ├── lib.rs : core, protocol, tools 모듈 공개 경로
│   ├── protocol/
│   │   ├── mod.rs : protocol 모듈 묶음, protocol::server 재노출
│   │   ├── jsonrpc/
│   │   │   ├── mod.rs : JSON-RPC 전송 묶음
│   │   │   └── server.rs : JSON-RPC 줄 단위 루프, 버전 협상, 호스트별 instructions, ping, tools/call 동시 처리
│   │   └── catalog/
│   │       ├── mod.rs : 도구 catalog 묶음
│   │       └── tools.rs : 도구 이름, title, 설명, 입력 스키마, annotation, mysql-query는 설정 시에만 노출
│   ├── tools/
│   │   ├── mod.rs : 도구 이름 분기, 기존 모듈 경로 alias
│   │   ├── args/
│   │   │   ├── mod.rs : 인자 도메인 묶음
│   │   │   ├── arg_shape.rs : 분기 전 인자 형태 보정
│   │   │   └── common.rs : 도메인 공통 인자 읽기, 경로 규칙
│   │   ├── fs/
│   │   │   ├── mod.rs : fs 도메인 묶음, 핸들러 교차 테스트
│   │   │   ├── read.rs : file-read, 인코딩, 이미지, isUrl 읽기
│   │   │   ├── lines.rs : file-read-line-range
│   │   │   ├── write.rs : file-write, 임시 파일 원자적 쓰기
│   │   │   ├── dir.rs : dir-create, dir-list, 제외 패턴
│   │   │   ├── path_ops.rs : path-copy, path-move, path-remove, path-stat
│   │   │   ├── edit.rs : file-edit, file-edit-lines, dryRun 미리보기
│   │   │   ├── replace.rs : file-replace, sd 방식 regex·literal·ast 치환, 전체 성공 시에만 쓰기
│   │   │   ├── compare.rs : file-diff, git 없는 unified diff
│   │   │   └── shared.rs : 항목 읽기, 변경 경로 추출
│   │   ├── search/
│   │   │   ├── mod.rs : fs-search 핸들러, 엔진 선택
│   │   │   ├── matcher.rs : regex, fancy-regex 매처, smart-case
│   │   │   ├── walk.rs : 병렬 탐색, grep sink, 예산
│   │   │   ├── find.rs : path-find, fd 방식 이름·타입·크기·시간 필터
│   │   │   ├── ast.rs : ast-grep CLI 기반 file-replace mode "ast" 용 재작성 편집
│   │   │   └── tests.rs : fs-search 종단 테스트
│   │   ├── git/
│   │   │   ├── mod.rs : git 도메인 묶음
│   │   │   ├── repo.rs : git CLI 실행, toplevel 캐시
│   │   │   ├── worktree.rs : git-status, git-add
│   │   │   ├── commit.rs : git-commit, git-amend, 메시지 검사
│   │   │   └── inspect.rs : git-diff, git-show
│   │   ├── web/
│   │   │   ├── mod.rs : web 도메인 묶음, 공통 필드 읽기
│   │   │   ├── fetch.rs : web-fetch
│   │   │   ├── extract.rs : web-extract
│   │   │   └── download.rs : download-to-file
│   │   ├── sql/
│   │   │   ├── mod.rs : sql 도메인 묶음
│   │   │   ├── query.rs : mysql-query 핸들러, 설정값 이내로 제한하는 행 수·시간 상한
│   │   │   ├── gate.rs : 읽기 전용 문장 게이트
│   │   │   ├── session.rs : 캐시된 연결 1개, READ ONLY 트랜잭션, 시간 제한, 시간 초과 시 KILL QUERY
│   │   │   └── render.rs : 결과 행 JSON 객체 렌더링
│   │   └── stats/
│   │       ├── mod.rs : stats 도메인 묶음
│   │       └── counter.rs : 설정으로 켜는 날짜별 도구 호출 집계, 날짜 문서 표 기록
│   └── core/
│       ├── mod.rs : core 모듈 묶음, core::<name> 평면 경로 재노출
│       ├── call/
│       │   ├── mod.rs : 도구 호출 공통 처리 묶음
│       │   ├── args_ref.rs : args_path 인자 파일 처리
│       │   ├── batch.rs : batch 실행기, worker pool, batch 결과 형식
│       │   └── response.rs : RawResult, 응답 envelope, 호스트별 출력 예산
│       ├── config/
│       │   ├── mod.rs : 설정 묶음
│       │   ├── paths.rs : 홈 경로 확장, 경로 정규화
│       │   ├── settings.rs : config.toml 로더([output], [stats], [mysql])
│       │   └── client.rs : MCP 호스트 판별, 호스트별 결과 정책
│       ├── process/
│       │   ├── mod.rs : 자식 프로세스 묶음
│       │   ├── external.rs : 시간 제한을 둔 git, ast-grep CLI 실행, ast-grep 실행 파일 해석
│       │   └── isolate.rs : fancy-regex 작업용 kill 가능 워커 프로세스, 대기 워커 1개
│       ├── sql/
│       │   ├── mod.rs : MySQL wire 클라이언트 묶음
│       │   ├── password.rs : 환경 변수, DPAPI 봉인 파일, 설정값에서 비밀번호 해석
│       │   ├── types.rs : 연결 옵션, 행 상한, 결과 집합, 문장 요약
│       │   ├── error.rs : 클라이언트 오류 타입, 시간 초과 표시
│       │   ├── connection.rs : 평문 TCP 연결, I/O 시간 제한, 종료
│       │   ├── handshake.rs : handshake, mysql_native_password·caching_sha2_password 인증
│       │   ├── statement.rs : COM_QUERY 실행, 행 상한 내 결과 집합 읽기
│       │   ├── packet.rs : 패킷 프레이밍, payload 읽기
│       │   ├── digest.rs : 인증 scramble용 SHA-1, SHA-256
│       │   ├── rsa.rs : 서버 공개 키 RSA-OAEP 암호화
│       │   ├── random.rs : RSA-OAEP seed용 OS 난수
│       │   └── fake_server.rs : 테스트 전용 loopback MySQL 서버
│       ├── text/
│       │   ├── mod.rs : 텍스트 계층 묶음
│       │   ├── diff.rs : 공용 unified diff 엔진(similar), CRLF 정규화
│       │   └── encoding.rs : UTF-16, EUC-KR/CP949 디코딩과 재인코딩
│       └── web/
│           ├── mod.rs : 웹 계층 묶음
│           ├── http.rs : HTTP 클라이언트, SSRF guard, RFC 3986 URL 해석
│           └── html.rs : HTML 트리 빌더, text·markdown·links·readability 변환
├── tests/
│   ├── tool_matrix.rs : catalog의 모든 도구를 dispatch로 호출
│   ├── isolation.rs : 실제 바이너리 기준 fancy-regex 폭주 kill·워커 크래시 검증
│   └── performance.rs : 기본 제외된 로컬 시간 측정
├── bench/
│   ├── README.md : 벤치마크 사용법
│   ├── mcp-bench.ts : 정답 검증을 포함한 stdio 종단 벤치마크
│   ├── ab.ts : 두 바이너리 교차 A/B 비교
│   ├── cli-bench.ts : CLI 연동(fd, sd, git) 전후 벤치마크
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
