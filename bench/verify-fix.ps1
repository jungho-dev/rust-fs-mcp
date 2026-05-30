# verify-fix.ps1
# Confirm: (A) file-write echoes an elided input.content; (B) file-read carries body once.
param([Parameter(Mandatory)][string]$Bin)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

function Start-Mcp([string]$bin) {
  $psi = New-Object System.Diagnostics.ProcessStartInfo
  $psi.FileName = $bin
  $psi.RedirectStandardInput  = $true
  $psi.RedirectStandardOutput = $true
  $psi.UseShellExecute = $false
  $psi.CreateNoWindow  = $true
  $psi.StandardOutputEncoding = [System.Text.Encoding]::UTF8
  $psi.StandardInputEncoding  = [System.Text.Encoding]::UTF8
  # No RUST_FS_MCP_COMPACT env => code default ON.
  $p = [System.Diagnostics.Process]::Start($psi)
  $p.StandardInput.AutoFlush = $true
  $p.StandardInput.WriteLine('{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}')
  [void]$p.StandardOutput.ReadLine()
  return $p
}
function Call($p, [string]$json) {
  $p.StandardInput.WriteLine($json)
  return $p.StandardOutput.ReadLine()
}
function McpJson($id, $name, $arguments) {
  return ([ordered]@{ jsonrpc='2.0'; id=$id; method='tools/call'; params=[ordered]@{ name=$name; arguments=$arguments } } | ConvertTo-Json -Depth 30 -Compress)
}

$tmp  = Join-Path $env:TEMP ("fixchk-{0}.md" -f $PID)
$body = '# review report ' + ('가나다 line of content body 본문 테스트 payload ' * 200)
$bodyLen = $body.Length

$srv  = Start-Mcp $Bin
$wresp = Call $srv (McpJson 2 'file-write' @{ items = @(@{ path = $tmp; content = $body }) })
$rresp = Call $srv (McpJson 3 'file-read'  @{ paths = @($tmp) })
try { $srv.StandardInput.Close(); [void]$srv.WaitForExit(1500) } catch {}

"input body length      = $bodyLen chars"
""
"=== WRITE ==="
$wj = $wresp | ConvertFrom-Json
$winput = $wj.result.structuredContent.results[0].input
"echoed input.content   = '$([string]$winput.content)'"
"echoed length          = $(([string]$winput.content).Length) chars"
$wllm = ($wj.result.structuredContent | ConvertTo-Json -Depth 40 -Compress)
"WRITE llm payload      = $($wllm.Length) chars (transport raw $($wresp.Length))"
""
"=== READ ==="
$rsc = $rj = $rresp | ConvertFrom-Json
$ritem = $rj.result.structuredContent.results[0].result.structuredContent
"read item sc keys      = $($ritem.PSObject.Properties.Name -join ', ')"
"read sc has 'content'  = $([bool]($ritem.PSObject.Properties.Name -contains 'content'))"
$rllm = ($rj.result.structuredContent | ConvertTo-Json -Depth 40 -Compress)
"READ llm payload       = $($rllm.Length) chars (body once would be ~$bodyLen)"

Remove-Item -LiteralPath $tmp -ErrorAction SilentlyContinue
