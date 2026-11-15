# fp-report.ps1

# - 백신 오탐 신고에 붙일 증거 묶음 생성 (해시, 메타데이터, 서명 상태, 빌드 환경)
# - 기본 대상은 target/release/rust-fs-mcp.exe, -ExePath 로 교체 가능
# - 산출물: target/fp-report/ 아래 보고서 md 와 제출용 exe 사본
# - target 폴더는 CARGO_TARGET_DIR 우선 (build.cmd 와 동일)
# - 탐지명, 엔진 버전, 탐지 시각은 사용자가 보고서에서 직접 채움

param(
    [string] $ExePath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

# 1. 대상 exe 확인 -------------------------------------------------------------
$repoRoot = Split-Path -Parent $PSScriptRoot
$targetDir = Join-Path $repoRoot "target"
if ($env:CARGO_TARGET_DIR) {
    $targetDir = [System.IO.Path]::Combine($repoRoot, $env:CARGO_TARGET_DIR)
}
$defaultExe = Join-Path $targetDir "release\rust-fs-mcp.exe"
$interactive = [Environment]::UserInteractive -and -not [Console]::IsInputRedirected -and
    -not ([Environment]::GetCommandLineArgs() -match '^(--?|/)noni')
if (-not $ExePath -and $interactive) {
    $typed = ""
    try {
        $typed = Read-Host "Detected file path [$defaultExe]"
    }
    catch {
        Write-Host "Prompt unavailable; using the default path."
    }
    if ($typed) {
        $ExePath = $typed
    }
}
if (-not $ExePath) {
    $ExePath = $defaultExe
}
if (-not (Test-Path -LiteralPath $ExePath -PathType Leaf)) {
    [Console]::Error.WriteLine("Report target not found: $ExePath")
    exit 1
}
$exe = Get-Item -LiteralPath $ExePath
Write-Host "Target: $($exe.FullName) ($($exe.Length) bytes)"

# 2. 해시 계산 -----------------------------------------------------------------
$digests = [ordered]@{}
foreach ($algorithm in @("SHA256", "SHA1", "MD5")) {
    $digests[$algorithm] = (Get-FileHash -LiteralPath $exe.FullName -Algorithm $algorithm).Hash
}

# 3. 메타데이터와 서명 상태 ----------------------------------------------------
$version = $exe.VersionInfo
$versionTag = "unversioned"
if ($version.FileVersion) {
    $versionTag = $version.FileVersion -replace '[^0-9A-Za-z._-]', '_'
}
$signature = Get-AuthenticodeSignature -LiteralPath $exe.FullName
$signer = "none"
if ($signature.SignerCertificate) {
    $signer = "$($signature.SignerCertificate.Subject) [$($signature.SignerCertificate.Thumbprint)]"
}

# 4. 빌드 환경 수집 ------------------------------------------------------------
$rustc = "unknown"
$rustcPath = Get-Command rustc -ErrorAction SilentlyContinue
if ($rustcPath) {
    # stderr 비리디렉션 (5.1 NativeCommandError 회피)
    $printed = & $rustcPath.Source -vV
    if ($LASTEXITCODE -eq 0) {
        $rustc = ($printed -join "; ")
    }
}

# 5. 보고서 작성 ---------------------------------------------------------------
$outDir = Join-Path $targetDir "fp-report"
if (-not (Test-Path -LiteralPath $outDir -PathType Container)) {
    New-Item -ItemType Directory -Path $outDir -Force | Out-Null
}
$stamp = (Get-Date).ToString("yyyy-MM-dd HH:mm:ss zzz")
$lines = @(
    "# rust-fs-mcp 오탐 신고 자료",
    "",
    "## 1. 탐지 정보 (신고자 작성)",
    "",
    "- 탐지명:",
    "- 알약 엔진/패턴 버전:",
    "- 탐지 일시:",
    "- 탐지 경로: 실시간 감시 / 수동 검사 / 행위 기반",
    "- 발생 PC 수:",
    "",
    "## 2. 탐지 대상 파일",
    "",
    "- 파일 경로: $($exe.FullName)",
    "- 파일 크기: $($exe.Length) bytes",
    "- 최종 수정: $($exe.LastWriteTime.ToString('yyyy-MM-dd HH:mm:ss'))",
    "- SHA256: $($digests['SHA256'])",
    "- SHA1: $($digests['SHA1'])",
    "- MD5: $($digests['MD5'])",
    "",
    "## 3. 파일 메타데이터",
    "",
    "- ProductName: $($version.ProductName)",
    "- FileDescription: $($version.FileDescription)",
    "- FileVersion: $($version.FileVersion)",
    "- CompanyName: $($version.CompanyName)",
    "- OriginalFilename: $($version.OriginalFilename)",
    "- LegalCopyright: $($version.LegalCopyright)",
    "- Authenticode 상태: $($signature.Status)",
    "- 서명자: $signer",
    "",
    "## 4. 빌드 환경",
    "",
    "- 보고서 생성: $stamp",
    "- rustc: $rustc",
    "- 빌드 프로필: release (opt-level 3, LTO, codegen-units 1, strip, panic abort)",
    "- 패커/프로텍터: 사용하지 않음 (UPX 등 없음)",
    "- 소스 공개 위치: https://github.com/jungho-dev/rust-fs-mcp",
    "",
    "## 5. 프로그램 설명",
    "",
    "- 용도: 개발용 코딩 에이전트에 로컬 파일 접근을 제공하는 stdio MCP 서버",
    "- 기능: 파일 읽기·쓰기·검색, 경로 복사·이동·삭제, 디렉터리 조회·생성, git 조회와 로컬 커밋(add, commit, amend)",
    "- 기본 빌드 추가 기능: 호출자가 지정한 URL 조회와 다운로드, 읽기 전용 MySQL 조회 (--no-default-features 빌드에는 없음)",
    "- 실행 형태: 상위 프로세스가 표준입출력으로 실행하고 종료시킴, 서비스·작업 스케줄러 등록 없음",
    "- 자동 실행 등록, 레지스트리 수정, 다른 폴더로의 자기 복제 없음",
    "- 자식 프로세스: 정규식 작업 격리용으로 동일 exe 를 --isolated-worker 인수로 실행, PATH 의 git 실행, ast-grep 실행 (RUST_FS_MCP_AST_GREP, exe 폴더, PATH, npm 설치 순으로 탐색)",
    "- 모든 파일 접근과 URL 조회는 호출자가 전달한 인수에 한정됨",
    "",
    "## 6. 첨부",
    "",
    "- 제출용 exe 사본: 같은 폴더의 rust-fs-mcp-$versionTag.exe",
    "- 압축 시 비밀번호는 신고 채널이 요구하는 값으로 설정",
    ""
)
$reportPath = Join-Path $outDir "fp-report.md"
[System.IO.File]::WriteAllText($reportPath, ($lines -join "`r`n"), [System.Text.UTF8Encoding]::new($false))
$samplePath = Join-Path $outDir "rust-fs-mcp-$versionTag.exe"
[System.IO.File]::Copy($exe.FullName, $samplePath, $true)
Write-Host "Report: $reportPath"
Write-Host "Sample: $samplePath"
exit 0
