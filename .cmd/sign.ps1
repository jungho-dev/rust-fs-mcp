# sign.ps1

# - build.cmd 가 호출하는 release exe Authenticode 서명
# - RUST_FS_MCP_SIGN_THUMBPRINT: CurrentUser\My 의 사내 코드서명 인증서 지문
# - 지문 미지정 시 로컬 자체서명 인증서 사용, 없으면 CurrentUser\My 에 생성
# - 자체서명 인증서는 신뢰 루트 미등록 (서명 존재만 보장)
# - RUST_FS_MCP_SIGN_TIMESTAMP: 타임스탬프 서버 URL (폐쇄망 기본 생략)

param(
    [Parameter(Mandatory = $true)]
    [string] $ExePath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

# 1. 서명 대상 확인 ------------------------------------------------------------
if (-not (Test-Path -LiteralPath $ExePath -PathType Leaf)) {
    [Console]::Error.WriteLine("Sign target not found: $ExePath")
    exit 1
}

# 2. 인증서 선택 ---------------------------------------------------------------
$localSubject = "CN=rust-fs-mcp Local Code Signing"
$now = Get-Date
$signingCerts = @(Get-ChildItem -LiteralPath "Cert:\CurrentUser\My" -CodeSigningCert)
$cert = $null
if ($env:RUST_FS_MCP_SIGN_THUMBPRINT) {
    $thumbprint = $env:RUST_FS_MCP_SIGN_THUMBPRINT.Replace(" ", "").ToUpperInvariant()
    $cert = $signingCerts |
        Where-Object { $_.Thumbprint -eq $thumbprint -and $_.HasPrivateKey } |
        Select-Object -First 1
    if ($null -eq $cert) {
        [Console]::Error.WriteLine("Code signing cert not found in CurrentUser\My: $thumbprint")
        exit 1
    }
}
else {
    $cert = $signingCerts |
        Where-Object { $_.Subject -eq $localSubject -and $_.HasPrivateKey -and $_.NotAfter -gt $now.AddDays(30) } |
        Sort-Object NotAfter -Descending |
        Select-Object -First 1
}

# 3. 자체서명 인증서 생성 ------------------------------------------------------
if ($null -eq $cert) {
    $certArgs = @{
        Type = "CodeSigningCert"
        Subject = $localSubject
        CertStoreLocation = "Cert:\CurrentUser\My"
        KeyAlgorithm = "RSA"
        KeyLength = 3072
        HashAlgorithm = "SHA256"
        KeyExportPolicy = "NonExportable"
        NotAfter = $now.AddYears(5)
    }
    $cert = New-SelfSignedCertificate @certArgs
    Write-Host "Created self-signed code signing cert: $($cert.Thumbprint)"
}

# 4. Authenticode 서명 ---------------------------------------------------------
$signArgs = @{
    FilePath = $ExePath
    Certificate = $cert
    HashAlgorithm = "SHA256"
}
if ($env:RUST_FS_MCP_SIGN_TIMESTAMP) {
    $signArgs.TimestampServer = $env:RUST_FS_MCP_SIGN_TIMESTAMP
}
$signature = Set-AuthenticodeSignature @signArgs
if ($null -eq $signature.SignerCertificate) {
    [Console]::Error.WriteLine("Signing failed: $($signature.StatusMessage)")
    exit 1
}
Write-Host "Signed: $ExePath [$($signature.Status)] $($cert.Subject) $($cert.Thumbprint)"
exit 0
