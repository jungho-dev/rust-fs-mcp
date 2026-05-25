param(
    [string] $Repo = "C:/JUNGHO/9.Workspace/2.Project/2.Node/rust-fs-mcp",
    [string] $Root = "C:/Users/jungh/.codex/.tmp/fs-mcp-vs-rust-benchmark-2026-05-24",
    [int] $Samples = 5
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Quote-Arg {
    param([string] $Value)

    '"' + ($Value -replace '"', '\"') + '"'
}

function New-McpServer {
    param(
        [string] $Name,
        [string] $Command,
        [string[]] $ArgsList,
        [string] $Cwd
    )

    $info = [System.Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $Command
    $info.Arguments = ($ArgsList | ForEach-Object { Quote-Arg $_ }) -join " "
    $info.WorkingDirectory = $Cwd
    $info.UseShellExecute = $false
    $info.RedirectStandardInput = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $info.StandardOutputEncoding = [System.Text.Encoding]::UTF8
    $info.StandardErrorEncoding = [System.Text.Encoding]::UTF8

    $proc = [System.Diagnostics.Process]::Start($info)
    if ($null -eq $proc) {
        throw "Failed to start $Name"
    }

    [pscustomobject]@{
        Name = $Name
        Command = $Command
        ArgsList = $ArgsList
        Cwd = $Cwd
        Process = $proc
        NextId = 1
    }
}

function Stop-McpServer {
    param([object] $Server)

    if ($null -eq $Server) {
        return
    }

    try {
        $Server.Process.StandardInput.Close()
    }
    catch {
    }

    if (-not $Server.Process.WaitForExit(1000)) {
        $Server.Process.Kill()
        $Server.Process.WaitForExit()
    }

    $Server.Process.Dispose()
}

function Invoke-Rpc {
    param(
        [object] $Server,
        [string] $Method,
        [object] $Params
    )

    $id = $Server.NextId
    $Server.NextId += 1
    $request = [ordered]@{
        jsonrpc = "2.0"
        id = $id
        method = $Method
        params = $Params
    } | ConvertTo-Json -Compress -Depth 80

    $watch = [System.Diagnostics.Stopwatch]::StartNew()
    $Server.Process.StandardInput.WriteLine($request)
    $Server.Process.StandardInput.Flush()

    $task = $Server.Process.StandardOutput.ReadLineAsync()
    if (-not $task.Wait(120000)) {
        throw "$($Server.Name) timed out waiting for $Method response"
    }
    $line = $task.Result
    $watch.Stop()

    if ([string]::IsNullOrWhiteSpace($line)) {
        throw "$($Server.Name) returned an empty response for $Method"
    }

    [pscustomobject]@{
        WallMs = [int] [Math]::Round($watch.Elapsed.TotalMilliseconds)
        Response = ($line | ConvertFrom-Json)
        LineChars = $line.Length
    }
}

function Invoke-ToolCall {
    param(
        [object] $Server,
        [string] $ToolName,
        [object] $Arguments
    )

    Invoke-Rpc $Server "tools/call" ([ordered]@{
        name = $ToolName
        arguments = $Arguments
    })
}

function Remove-Ansi {
    param([string] $Text)

    [regex]::Replace($Text, "\x1B\[[0-9;]*m", "")
}

function Get-SummaryNumber {
    param(
        [string] $Text,
        [string] $Name
    )

    $plain = Remove-Ansi $Text
    $pattern = [regex]::Escape($Name) + "\s*=\s*([0-9,]+)"
    $match = [regex]::Match($plain, $pattern)
    if (-not $match.Success) {
        return $null
    }

    [int64] ($match.Groups[1].Value -replace ",", "")
}

function Get-Prop {
    param(
        [object] $Value,
        [string] $Name
    )

    if ($null -eq $Value) {
        return $null
    }

    $prop = $Value.PSObject.Properties[$Name]
    if ($null -eq $prop) {
        return $null
    }

    $prop.Value
}

function Get-Backend {
    param([object] $Structured)

    $results = Get-Prop $Structured "results"
    if ($null -eq $results -or $results.Count -eq 0) {
        return $null
    }

    $first = $results[0]
    $result = Get-Prop $first "result"
    $inner = Get-Prop $result "structuredContent"
    Get-Prop $inner "backend"
}

function Convert-Metric {
    param(
        [string] $TaskName,
        [string] $Tool,
        [int] $Sample,
        [string] $Phase,
        [object] $Call
    )

    $response = $Call.Response
    $error = Get-Prop $response "error"
    if ($null -ne $error) {
        return [pscustomobject] [ordered]@{
            taskName = $TaskName
            tool = $Tool
            sample = $Sample
            phase = $Phase
            ok = $false
            wallMs = $Call.WallMs
            durationMs = $null
            tokens = $null
            contentsChars = $null
            structuredTextChars = $null
            returnedTextChars = $null
            backend = $null
            status = "error"
            error = $error.message
            isError = $true
        }
    }

    $result = $response.result
    $summary = ""
    if ($result.content.Count -gt 0) {
        $summary = [string] $result.content[0].text
    }

    $outer = $result.structuredContent
    $data = $outer.data
    $structured = $data.structuredContent
    $text = Get-Prop $data "text"
    $isError = Get-Prop $result "isError"
    if ($null -eq $isError) {
        $isError = $false
    }

    [pscustomobject] [ordered]@{
        taskName = $TaskName
        tool = $Tool
        sample = $Sample
        phase = $Phase
        ok = (-not [bool] $isError) -and $outer.status -eq "success"
        wallMs = $Call.WallMs
        durationMs = $outer.durationMs
        tokens = Get-SummaryNumber $summary "tokens"
        contentsChars = Get-SummaryNumber $summary "contents"
        structuredTextChars = Get-SummaryNumber $summary "structuredText"
        returnedTextChars = if ($null -eq $text) { $null } else { ([string] $text).Length }
        backend = Get-Backend $structured
        status = $outer.status
        error = $outer.error
        isError = [bool] $isError
    }
}

function Get-Stats {
    param([object[]] $Values)

    $nums = @($Values | Where-Object { $null -ne $_ } | ForEach-Object { [double] $_ })
    if ($nums.Count -eq 0) {
        return [pscustomobject] [ordered]@{ n = 0; mean = $null; median = $null; min = $null; max = $null; stdev = $null }
    }

    $sum = ($nums | Measure-Object -Sum).Sum
    $mean = $sum / $nums.Count
    $sorted = @($nums | Sort-Object)
    $mid = [int] [Math]::Floor($sorted.Count / 2)
    $median = if ($sorted.Count % 2 -eq 0) {
        ($sorted[$mid - 1] + $sorted[$mid]) / 2
    }
    else {
        $sorted[$mid]
    }
    $variance = 0.0
    foreach ($num in $nums) {
        $variance += [Math]::Pow($num - $mean, 2)
    }
    $variance = $variance / $nums.Count

    [pscustomobject] [ordered]@{
        n = $nums.Count
        mean = [Math]::Round($mean, 3)
        median = [Math]::Round($median, 3)
        min = ($nums | Measure-Object -Minimum).Minimum
        max = ($nums | Measure-Object -Maximum).Maximum
        stdev = [Math]::Round([Math]::Sqrt($variance), 3)
    }
}

function New-Aggregate {
    param([object[]] $Runs)

    $rows = @()
    foreach ($group in ($Runs | Group-Object taskName, tool)) {
        $items = @($group.Group)
        $okItems = @($items | Where-Object { $_.ok })
        $rows += [pscustomobject] [ordered]@{
            taskName = $items[0].taskName
            tool = $items[0].tool
            okRuns = $okItems.Count
            totalRuns = $items.Count
            durationMs = Get-Stats @($okItems | ForEach-Object { $_.durationMs })
            wallMs = Get-Stats @($okItems | ForEach-Object { $_.wallMs })
            tokens = Get-Stats @($okItems | ForEach-Object { $_.tokens })
            contentsChars = Get-Stats @($okItems | ForEach-Object { $_.contentsChars })
            structuredTextChars = Get-Stats @($okItems | ForEach-Object { $_.structuredTextChars })
            backends = @($okItems | ForEach-Object { $_.backend } | Where-Object { $null -ne $_ } | Sort-Object -Unique)
        }
    }

    $rows
}

function Get-Mean {
    param(
        [object[]] $Aggregate,
        [string] $TaskName,
        [string] $Tool,
        [string] $Metric
    )

    $row = @($Aggregate | Where-Object { $_.taskName -eq $TaskName -and $_.tool -eq $Tool } | Select-Object -First 1)
    if ($row.Count -eq 0) {
        return $null
    }

    $row.$Metric.mean
}

function Get-DeltaPct {
    param(
        [double] $Base,
        [double] $Variant
    )

    if ($Base -eq 0) {
        return $null
    }

    [Math]::Round((($Variant - $Base) / $Base) * 100, 3)
}

$dataDir = Join-Path $Root "data"
$largeText = Join-Path $dataDir "large_text_128m.txt"
$largeBlob = Join-Path $dataDir "large_blob_128m.bin"
$smallDir = Join-Path $dataDir "small"
$copyDir = Join-Path $Root "copies"
[System.IO.Directory]::CreateDirectory($copyDir) | Out-Null

foreach ($required in @($largeText, $largeBlob, $smallDir)) {
    if (-not (Test-Path -LiteralPath $required)) {
        throw "Missing benchmark input: $required"
    }
}

$smallPaths = foreach ($i in 0..49) {
    Join-Path $smallDir ("small_{0:D4}.txt" -f $i)
}

$rustExe = Join-Path $Repo "target/release/rust-fs-mcp.exe"
if (-not (Test-Path -LiteralPath $rustExe)) {
    throw "Missing release executable: $rustExe"
}

$tools = @(
    [pscustomobject]@{
        Name = "fs-mcp"
        Id = "fs"
        Command = "bun"
        ArgsList = @("C:/Users/jungh/.codex/mcp/mcp-fs.ts")
        Cwd = "C:/Users/jungh/.codex"
    },
    [pscustomobject]@{
        Name = "rust-fs-mcp"
        Id = "rust"
        Command = $rustExe
        ArgsList = @()
        Cwd = $Repo
    }
)

$tasks = @(
    [pscustomobject]@{
        Name = "dir_list_512_small"
        ToolName = "dir-list"
        MakeArgs = {
            [ordered]@{ items = @([ordered]@{ path = $smallDir; depth = 1; includeFiles = $true; maxEntries = 600 }) }
        }
        Cleanup = $null
    },
    [pscustomobject]@{
        Name = "file_infos_52_paths"
        ToolName = "file-infos"
        MakeArgs = {
            [ordered]@{ paths = @($largeText, $largeBlob) + $smallPaths }
        }
        Cleanup = $null
    },
    [pscustomobject]@{
        Name = "file_read_256k_slice"
        ToolName = "file-read"
        MakeArgs = {
            [ordered]@{ items = @([ordered]@{ path = $largeText; offset = 67108864; length = 262144 }) }
        }
        Cleanup = $null
    },
    [pscustomobject]@{
        Name = "search_regex_large_text"
        ToolName = "search-regex"
        MakeArgs = {
            [ordered]@{ items = @([ordered]@{ path = $largeText; pattern = "benchmark_line_11184[0-9][0-9]"; maxResults = 20 }) }
        }
        Cleanup = $null
    },
    [pscustomobject]@{
        Name = "file_copy_128m_blob"
        ToolName = "file-copy"
        MakeArgs = {
            param([int] $Sample, [object] $Tool)

            $dest = Join-Path $copyDir ("copy_{0}_{1}.bin" -f $Tool.Id, $Sample)
            [ordered]@{ items = @([ordered]@{ source = $largeBlob; destination = $dest; force = $true }) }
        }
        Cleanup = {
            param([int] $Sample, [object] $Tool, [object] $Server)

            $dest = Join-Path $copyDir ("copy_{0}_{1}.bin" -f $Tool.Id, $Sample)
            Invoke-ToolCall $Server "file-remove" ([ordered]@{
                items = @([ordered]@{ path = $dest; force = $true })
            }) | Out-Null
        }
    },
    [pscustomobject]@{
        Name = "file_lines_2000_large_text"
        ToolName = "file-lines"
        MakeArgs = {
            [ordered]@{ items = @([ordered]@{ path = $largeText; offset = 500000; length = 2000 }) }
        }
        Cleanup = $null
    }
)

$startedAt = [DateTimeOffset]::Now
$servers = @{}
$runs = @()
$warmups = @()
$invalid = @()

try {
    foreach ($tool in $tools) {
        $server = New-McpServer $tool.Name $tool.Command $tool.ArgsList $tool.Cwd
        $servers[$tool.Name] = $server
        Invoke-Rpc $server "initialize" ([ordered]@{
            protocolVersion = "2025-06-18"
            capabilities = [ordered]@{}
            clientInfo = [ordered]@{ name = "fs-mcp-benchmark"; version = "1.0.0" }
        }) | Out-Null
    }

    foreach ($task in $tasks) {
        foreach ($tool in $tools) {
            $server = $servers[$tool.Name]
            $args = & $task.MakeArgs 0 $tool
            $call = Invoke-ToolCall $server $task.ToolName $args
            $warmups += Convert-Metric $task.Name $tool.Name 0 "warmup" $call
            if ($null -ne $task.Cleanup) {
                & $task.Cleanup 0 $tool $server
            }
        }

        foreach ($sample in 1..$Samples) {
            $order = if ($sample % 2 -eq 0) { @($tools[1], $tools[0]) } else { $tools }
            foreach ($tool in $order) {
                $server = $servers[$tool.Name]
                $args = & $task.MakeArgs $sample $tool
                $call = Invoke-ToolCall $server $task.ToolName $args
                $runs += Convert-Metric $task.Name $tool.Name $sample "measure" $call
                if ($null -ne $task.Cleanup) {
                    & $task.Cleanup $sample $tool $server
                }
            }
        }
    }
}
finally {
    foreach ($server in $servers.Values) {
        Stop-McpServer $server
    }
}

$aggregate = @(New-Aggregate $runs)
$comparisons = @()
foreach ($task in $tasks) {
    $fsDuration = Get-Mean $aggregate $task.Name "fs-mcp" "durationMs"
    $rustDuration = Get-Mean $aggregate $task.Name "rust-fs-mcp" "durationMs"
    $fsWall = Get-Mean $aggregate $task.Name "fs-mcp" "wallMs"
    $rustWall = Get-Mean $aggregate $task.Name "rust-fs-mcp" "wallMs"
    $fsTokens = Get-Mean $aggregate $task.Name "fs-mcp" "tokens"
    $rustTokens = Get-Mean $aggregate $task.Name "rust-fs-mcp" "tokens"
    $comparisons += [pscustomobject] [ordered]@{
        taskName = $task.Name
        fsDurationMeanMs = $fsDuration
        rustDurationMeanMs = $rustDuration
        rustDurationVsFsPct = Get-DeltaPct $fsDuration $rustDuration
        fsWallMeanMs = $fsWall
        rustWallMeanMs = $rustWall
        rustWallVsFsPct = Get-DeltaPct $fsWall $rustWall
        fsTokensMean = $fsTokens
        rustTokensMean = $rustTokens
        rustTokensVsFsPct = Get-DeltaPct $fsTokens $rustTokens
    }
}

$invalid += [pscustomobject] [ordered]@{
    taskName = "file_read_256k_slice"
    reason = "fs-mcp and rust-fs-mcp expose different offset/length semantics for file-read on this task; compare file-lines for line-range behavior."
}

[ordered]@{
    benchmarkName = "fs-mcp vs rust-fs-mcp bundled stdio benchmark"
    startedAt = $startedAt.ToString("o")
    finishedAt = [DateTimeOffset]::Now.ToString("o")
    environment = [ordered]@{
        platform = [System.Environment]::OSVersion.Platform.ToString()
        repo = $Repo
        root = $Root
        dataDir = $dataDir
        largeText = $largeText
        largeTextBytes = (Get-Item -LiteralPath $largeText).Length
        largeBlob = $largeBlob
        largeBlobBytes = (Get-Item -LiteralPath $largeBlob).Length
        smallDir = $smallDir
        smallFiles = 512
        samples = $Samples
        cacheState = "OS filesystem cache not cleared; one warm-up per task/tool excluded."
        measuredTokenKind = "MCP tool summary output tokens, not model API input/output/cache tokens."
        apiUsageAvailability = "Model API input/output/cache tokens, TTFT, and cost are not exposed by local stdio MCP calls."
    }
    tools = @($tools | ForEach-Object {
        [ordered]@{
            name = $_.Name
            command = $_.Command
            args = $_.ArgsList
            cwd = $_.Cwd
        }
    })
    fixedTask = [ordered]@{
        tasks = @($tasks | ForEach-Object { $_.Name })
        successCriteria = @(
            "Both stdio servers complete the same JSON-RPC tool calls without exception.",
            "Each measured task has five successful samples per tool after one warm-up.",
            "Report includes speed, wall time, returned-token metrics, validity notes, and residual uncertainty."
        )
        invalidComparableTasks = $invalid
    }
    warmups = $warmups
    runs = $runs
    aggregate = $aggregate
    comparisons = $comparisons
    invalidRuns = @($runs | Where-Object { -not $_.ok })
} | ConvertTo-Json -Depth 80
