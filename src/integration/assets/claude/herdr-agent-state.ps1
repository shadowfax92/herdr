# installed by herdr
# managed by herdr; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# HERDR_INTEGRATION_ID=claude
# HERDR_INTEGRATION_VERSION=12

param([string]$Action = "")

if ($Action -ne "session") { exit 0 }
if ($env:HERDR_ENV -ne "1") { exit 0 }
if ([string]::IsNullOrWhiteSpace($env:HERDR_PANE_ID)) { exit 0 }

# Read hook input as raw bytes so UTF-8 payloads decode correctly even when the
# console input code page is not UTF-8 (for example CP949 on Korean Windows).
$stdinReader = New-Object System.IO.StreamReader([Console]::OpenStandardInput(), [System.Text.Encoding]::UTF8)
$inputText = $stdinReader.ReadToEnd()
try {
    $payload = if ([string]::IsNullOrWhiteSpace($inputText)) { $null } else { $inputText | ConvertFrom-Json }
} catch {
    exit 0
}

$propertyNames = @($payload.PSObject.Properties.Name)
if ((Test-Path Env:CURSOR_VERSION) -or $propertyNames -ccontains "cursor_version") { exit 0 }
if (-not ($propertyNames -ccontains "hook_event_name") -or $payload.hook_event_name -isnot [string] -or $payload.hook_event_name -cne "SessionStart") { exit 0 }
if (-not [string]::IsNullOrWhiteSpace($payload.agent_id)) { exit 0 }

$sessionId = $payload.session_id
if ([string]::IsNullOrWhiteSpace($sessionId)) { exit 0 }

# The pane shell PID survives server handoff. If its lookup fails, inspect the
# whole ancestry instead: another agent still proves nesting, while a lookup
# failure alone must not silently hide a top-level session for its lifetime.
$herdr = if ([string]::IsNullOrWhiteSpace($env:HERDR_BIN_PATH)) { "herdr" } else { $env:HERDR_BIN_PATH }
$shellPid = $null
$infoProcess = $null
try {
    # Bound the optional CLI query as well as WMI below within the hook's 10s
    # budget. Drain both pipes asynchronously so diagnostic output cannot block.
    $start = New-Object System.Diagnostics.ProcessStartInfo
    $start.FileName = $herdr
    $arguments = @("pane", "process-info", "--pane", $env:HERDR_PANE_ID)
    if ($herdr.EndsWith(".ps1", [StringComparison]::OrdinalIgnoreCase)) {
        $start.FileName = [System.Diagnostics.Process]::GetCurrentProcess().MainModule.FileName
        $arguments = @("-NoProfile", "-File", $herdr) + $arguments
    }
    $start.Arguments = ($arguments | ForEach-Object { '"' + $_.Replace('"', '\"') + '"' }) -join ' '
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $infoProcess = [System.Diagnostics.Process]::Start($start)
    $stdout = $infoProcess.StandardOutput.ReadToEndAsync()
    $stderr = $infoProcess.StandardError.ReadToEndAsync()
    if (-not $infoProcess.WaitForExit(2000)) {
        $infoProcess.Kill()
        throw "pane ownership lookup timed out"
    }
    $info = $stdout.Result | ConvertFrom-Json
    $shellPid = [uint32]::Parse([string]$info.result.process_info.shell_pid)
} catch {
    # Keep inspecting without a boundary when the server is busy/unavailable.
} finally {
    if ($null -ne $infoProcess) { $infoProcess.Dispose() }
}

$processes = @{}
for ($attempt = 0; $attempt -lt 2; $attempt++) {
    try {
        Get-CimInstance Win32_Process -OperationTimeoutSec 2 -ErrorAction Stop | ForEach-Object {
            $processes[[uint32]$_.ProcessId] = $_
        }
        if ($processes.Count -gt 0) { break }
    } catch {
        $processes = @{}
    }
}
if ($processes.Count -eq 0) { exit 0 }

$cliPid = [uint32]0
$knownReporter = [uint32]::TryParse($env:CLAUDE_PID, [ref]$cliPid) -and $processes.ContainsKey($cliPid)
$currentPid = if ($knownReporter) { $cliPid } else { [uint32]$PID }
$cliName = if ($knownReporter) { $processes[$cliPid].Name } else { $null }
$reporterSeen = $knownReporter
$visited = New-Object 'System.Collections.Generic.HashSet[uint32]'
# When CLAUDE_PID is absent/stale, the first agent above the hook is its reporter.
# Only a second agent proves nesting; a truncated chain preserves reporting.
while ($processes.ContainsKey($currentPid) -and $visited.Add($currentPid)) {
    $process = $processes[$currentPid]
    $name = $process.Name.ToLowerInvariant().TrimStart('.') -replace '\.exe$', '' -replace '-wrapped$', ''
    $isAgent = ($null -ne $cliName -and $process.Name -eq $cliName) -or $name -in @("claude", "claude-code", "codex")
    if ((-not $knownReporter -or $currentPid -ne $cliPid) -and $isAgent) {
        if ($reporterSeen) { exit 0 }
        $reporterSeen = $true
    }
    if ($currentPid -eq $shellPid) { break }
    $currentPid = [uint32]$process.ParentProcessId
}

$seq = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
try {
    $args = @(
        "pane",
        "report-agent-session",
        $env:HERDR_PANE_ID,
        "--source",
        "herdr:claude",
        "--agent",
        "claude",
        "--seq",
        "$seq",
        "--agent-session-id",
        "$sessionId"
    )
    if ($payload.transcript_path -is [string] -and -not [string]::IsNullOrWhiteSpace($payload.transcript_path)) {
        $args += @("--agent-session-path", "$($payload.transcript_path)")
    }
    if ($payload.hook_event_name -eq "SessionStart" -and $payload.source -is [string] -and -not [string]::IsNullOrWhiteSpace($payload.source)) {
        $args += @("--session-start-source", "$($payload.source)")
    }
    & $herdr @args 2>$null | Out-Null
} catch {
}
