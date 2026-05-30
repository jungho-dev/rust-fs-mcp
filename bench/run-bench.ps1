# run-bench.ps1
# Benchmark: rust-fs-mcp (stdio MCP) vs native pwsh/CLI on fixed filesystem tasks.
# Measures per-call latency (Stopwatch, persistent server) and response payload size.
param(
  [int]$Iterations = 25,
  [int]$Warmup = 4
)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

$BIN  = 'C:\JUNGHO\1.Language\6.Rust\target\release\rust-fs-mcp.exe'
$REPO = 'C:\JUNGHO\9.Workspace\2.Project\3.rust\rust-fs-mcp'
$BIG  = "$REPO\src\tools\fs_tools.rs"
$SRC  = "$REPO\src"
$OUT  = "$REPO\bench"
$PAT  = 'pub\s+fn\s+\w+'                       # search pattern, case-sensitive, both sides

$rsFiles = @(Get-ChildItem -Path $SRC -Recurse -File -Filter *.rs | ForEach-Object { $_.FullName } | Sort-Object)

# ---------------------------------------------------------------- stats helpers
function Pctl($arr, $p) {
  $s = @($arr | Sort-Object)
  $idx = [math]::Ceiling($p / 100.0 * $s.Count) - 1
  if ($idx -lt 0) { $idx = 0 }
  return $s[$idx]
}
function Bytes($s) { return [System.Text.Encoding]::UTF8.GetByteCount([string]$s) }
function EstTok($s) { return [math]::Round((Bytes $s) / 4.0) }   # rough byte/4 token proxy

# ---------------------------------------------------------------- MCP transport
function Start-Mcp([string]$compact) {
  $psi = New-Object System.Diagnostics.ProcessStartInfo
  $psi.FileName = $BIN
  $psi.RedirectStandardInput  = $true
  $psi.RedirectStandardOutput = $true
  $psi.RedirectStandardError  = $true
  $psi.UseShellExecute = $false
  $psi.CreateNoWindow  = $true
  $psi.StandardOutputEncoding = [System.Text.Encoding]::UTF8
  $psi.StandardInputEncoding  = [System.Text.Encoding]::UTF8
  [void]$psi.EnvironmentVariables.Remove('RUST_FS_MCP_COMPACT')
  $psi.EnvironmentVariables['RUST_FS_MCP_COMPACT'] = $compact
  $p = [System.Diagnostics.Process]::Start($psi)
  $p.StandardInput.AutoFlush = $true
  $p.StandardInput.WriteLine('{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}')
  [void]$p.StandardOutput.ReadLine()
  return $p
}
function Stop-Mcp($p) {
  try { $p.StandardInput.Close() } catch {}
  try { if (-not $p.WaitForExit(1500)) { $p.Kill() } } catch {}
}
function Invoke-Mcp($p, [string]$json) {
  $sw = [System.Diagnostics.Stopwatch]::StartNew()
  $p.StandardInput.WriteLine($json)
  $resp = $p.StandardOutput.ReadLine()
  $sw.Stop()
  return [pscustomobject]@{ ms = $sw.Elapsed.TotalMilliseconds; resp = [string]$resp }
}
function McpJson([string]$name, $arguments) {
  $obj = [ordered]@{ jsonrpc = '2.0'; id = 99; method = 'tools/call'; params = [ordered]@{ name = $name; arguments = $arguments } }
  return ($obj | ConvertTo-Json -Depth 30 -Compress)
}

# ---------------------------------------------------------------- native runner
function Invoke-Native([scriptblock]$sb) {
  $sw = [System.Diagnostics.Stopwatch]::StartNew()
  $out = (& $sb 2>$null | Out-String -Width 512)
  $sw.Stop()
  return [pscustomobject]@{ ms = $sw.Elapsed.TotalMilliseconds; out = [string]$out }
}

# ---------------------------------------------------------------- task matrix
$tasks = @(
  @{ id='T1'; label='read whole file (63KB/1756L)';
     mcp = (McpJson 'file-read'   @{ items = @(@{ path = $BIG }) });
     nat = { Get-Content -Raw -LiteralPath $BIG } }
  @{ id='T2'; label='read line range 500..699';
     mcp = (McpJson 'file-lines'  @{ items = @(@{ path = $BIG; offset = 500; length = 200 }) });
     nat = { Get-Content -LiteralPath $BIG | Select-Object -Skip 499 -First 200 } }
  @{ id='T3'; label='recursive dir listing (src)';
     mcp = (McpJson 'dir-list'    @{ items = @(@{ path = $SRC; depth = 6 }) });
     nat = { Get-ChildItem -Path $SRC -Recurse -Name } }
  @{ id='T4'; label='regex content search (pub fn)';
     mcp = (McpJson 'search-regex' @{ items = @(@{ path = $SRC; pattern = $PAT; filePattern = '*.rs'; ignoreCase = $false; contextLines = 2 }) });
     nat = { & rg --color never -n -C 2 --no-heading -e $PAT -g '*.rs' $SRC } }
  @{ id='T5'; label='file metadata batch (16 files)';
     mcp = (McpJson 'file-infos'  @{ paths = $rsFiles });
     nat = { $rsFiles | ForEach-Object { Get-Item -LiteralPath $_ } | Select-Object FullName,Length,LastWriteTimeUtc,Mode } }
  @{ id='T6'; label='git status';
     mcp = (McpJson 'git-status'  @{ path = $REPO });
     nat = { & git -C $REPO status } }
)

# ---------------------------------------------------------------- run MCP (compact 1 and 0)
$rows = New-Object System.Collections.Generic.List[object]
$samples = [ordered]@{}

foreach ($compact in @('1','0')) {
  $srv = Start-Mcp $compact
  foreach ($t in $tasks) {
    1..$Warmup | ForEach-Object { [void](Invoke-Mcp $srv $t.mcp) }
    $ms = New-Object System.Collections.Generic.List[double]
    $lastResp = ''
    1..$Iterations | ForEach-Object {
      $r = Invoke-Mcp $srv $t.mcp
      $ms.Add($r.ms); $lastResp = $r.resp
    }
    # LLM-visible payload = result.structuredContent (what Claude Code serializes into tool_result),
    # NOT the raw JSON-RPC line and NOT the top-level content summary.
    $llm = ''
    try { $llm = (([string]$lastResp | ConvertFrom-Json).result.structuredContent | ConvertTo-Json -Depth 40 -Compress) } catch { $llm = [string]$lastResp }
    $rows.Add([pscustomobject]@{
      task=$t.id; desc=$t.label; mode=("mcp-compact$compact");
      p50=[math]::Round((Pctl $ms 50),2); mean=[math]::Round(($ms|Measure-Object -Average).Average,2);
      min=[math]::Round(($ms|Measure-Object -Minimum).Minimum,2); p90=[math]::Round((Pctl $ms 90),2);
      transChars=$lastResp.Length; transBytes=(Bytes $lastResp);
      llmChars=$llm.Length; llmBytes=(Bytes $llm); llmTok=(EstTok $llm)
    })
    $samples["$($t.id)_mcp_c$compact"] = $lastResp
  }
  Stop-Mcp $srv
}

# ---------------------------------------------------------------- run native
foreach ($t in $tasks) {
  1..$Warmup | ForEach-Object { [void](Invoke-Native $t.nat) }
  $ms = New-Object System.Collections.Generic.List[double]
  $lastOut = ''
  1..$Iterations | ForEach-Object {
    $r = Invoke-Native $t.nat
    $ms.Add($r.ms); $lastOut = $r.out
  }
  $rows.Add([pscustomobject]@{
    task=$t.id; desc=$t.label; mode='native';
    p50=[math]::Round((Pctl $ms 50),2); mean=[math]::Round(($ms|Measure-Object -Average).Average,2);
    min=[math]::Round(($ms|Measure-Object -Minimum).Minimum,2); p90=[math]::Round((Pctl $ms 90),2);
    transChars=$lastOut.Length; transBytes=(Bytes $lastOut);
    llmChars=$lastOut.Length; llmBytes=(Bytes $lastOut); llmTok=(EstTok $lastOut)
  })
  $samples["$($t.id)_native"] = $lastOut
}

# ---------------------------------------------------------------- persist + print
$rows | Export-Csv -Path "$OUT\results.csv" -NoTypeInformation -Encoding UTF8
$rows | ConvertTo-Json -Depth 6 | Set-Content -Path "$OUT\results.json" -Encoding UTF8
$samples | ConvertTo-Json -Depth 6 | Set-Content -Path "$OUT\samples.json" -Encoding UTF8

"=== ENV ==="
"iterations=$Iterations warmup=$Warmup"
"bin=$BIN"
(Get-Item $BIN).LastWriteTime
"rsFiles=$($rsFiles.Count)"
""
"=== RESULTS (latency ms ; transport=raw JSON-RPC line ; llm=result.structuredContent serialized) ==="
$rows | Sort-Object task, mode | Format-Table task, mode, p50, mean, p90, transChars, llmChars, llmTok -AutoSize
