param(
    [ValidateSet("All", "Prepare", "Engine", "App")]
    [string]$Mode = "All",
    [int]$EntryCount = 100000,
    [int]$PayloadMiB = 128,
    [int]$StartupRuns = 5,
    [int]$IdleSeconds = 5,
    [string]$ResultsPath = "perf/results/latest.json",
    [string]$LibArchivePath = "",
    [switch]$AllowUnsupportedRuntime,
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$script:RepoRoot = Split-Path -Parent $PSScriptRoot
$script:WorkRoot = Join-Path $PSScriptRoot "work"
$script:FixtureRoot = Join-Path $script:WorkRoot "fixture"
$script:ArchivePath = Join-Path $script:WorkRoot "large-list.tar"
$script:ExtractRoot = Join-Path $script:WorkRoot "extracted"
$script:ReleaseExe = Join-Path $script:RepoRoot "target\release\archive-rclick.exe"
$script:PerfExe = Join-Path $script:RepoRoot "target\release\examples\perf_engine.exe"

function Resolve-ResultPath {
    param([string]$Path)
    if ([System.IO.Path]::IsPathRooted($Path)) {
        return [System.IO.Path]::GetFullPath($Path)
    }
    return [System.IO.Path]::GetFullPath((Join-Path $script:RepoRoot $Path))
}

function New-Fixture {
    if ($EntryCount -lt 1) { throw "EntryCount must be positive" }
    if ($PayloadMiB -lt 0) { throw "PayloadMiB cannot be negative" }

    New-Item -ItemType Directory -Force -Path $script:WorkRoot | Out-Null
    if (Test-Path -LiteralPath $script:FixtureRoot) {
        Remove-Item -LiteralPath $script:FixtureRoot -Recurse -Force
    }
    New-Item -ItemType Directory -Force -Path $script:FixtureRoot | Out-Null

    $manifest = Join-Path $script:WorkRoot "fixture-manifest.txt"
    $utf8 = [System.Text.UTF8Encoding]::new($false)
    $writer = [System.IO.StreamWriter]::new($manifest, $false, $utf8, 1048576)
    try {
        for ($index = 0; $index -lt $EntryCount; $index++) {
            $group = [Math]::Floor($index / 1000)
            $writer.WriteLine(("entries/group-{0:D4}/file-{1:D6}.txt" -f $group, $index))
        }
        if ($PayloadMiB -gt 0) {
            $writer.WriteLine("payload.bin")
        }
    } finally {
        $writer.Dispose()
    }

    $payload = Join-Path $script:FixtureRoot "payload.bin"
    if ($PayloadMiB -gt 0) {
        $stream = [System.IO.File]::Open($payload, [System.IO.FileMode]::Create, [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
        try {
            $stream.SetLength([int64]$PayloadMiB * 1MB)
        } finally {
            $stream.Dispose()
        }
    }

    if (Test-Path -LiteralPath $script:ArchivePath) {
        Remove-Item -LiteralPath $script:ArchivePath -Force
    }
    tar.exe -cf $script:ArchivePath -C $script:WorkRoot --files-from $manifest
    if ($LASTEXITCODE -ne 0) { throw "tar.exe failed to create the performance archive" }

    [ordered]@{
        archive = $script:ArchivePath
        requested_entries = $EntryCount
        payload_mib = $PayloadMiB
        archive_bytes = (Get-Item -LiteralPath $script:ArchivePath).Length
    }
}

function Invoke-Build {
    if ($SkipBuild) { return }
    Push-Location $script:RepoRoot
    try {
        cargo build --release --example perf_engine
        if ($LASTEXITCODE -ne 0) { throw "release performance-engine build failed" }
        cargo build --release --bin archive-rclick
        if ($LASTEXITCODE -ne 0) { throw "release application build failed" }
    } finally {
        Pop-Location
    }
}

function Set-RuntimeEnvironment {
    if ($LibArchivePath) {
        $env:ARCHIVERCLICK_LIBARCHIVE = [System.IO.Path]::GetFullPath($LibArchivePath)
    }
    if ($AllowUnsupportedRuntime) {
        $env:ARCHIVERCLICK_ALLOW_UNSUPPORTED_LIBARCHIVE = "1"
    }
}

function Invoke-EngineProbe {
    if (Test-Path -LiteralPath $script:ExtractRoot) {
        Remove-Item -LiteralPath $script:ExtractRoot -Recurse -Force
    }
    $output = & $script:PerfExe $script:ArchivePath $script:ExtractRoot
    if ($LASTEXITCODE -ne 0) { throw "perf_engine failed with exit code $LASTEXITCODE" }
    return ($output -join "`n" | ConvertFrom-Json)
}

function Stop-ProbeProcess {
    param([System.Diagnostics.Process]$Process)
    if ($null -eq $Process) { return }
    if (-not $Process.HasExited) {
        $Process.CloseMainWindow() | Out-Null
        if (-not $Process.WaitForExit(1500)) {
            Stop-Process -Id $Process.Id -Force
            $Process.WaitForExit()
        }
    }
    $Process.Dispose()
}

function Measure-WindowReady {
    param([string[]]$Arguments)
    $start = [System.Diagnostics.Stopwatch]::StartNew()
    $process = Start-Process -FilePath $script:ReleaseExe -ArgumentList $Arguments -PassThru -WindowStyle Normal
    try {
        $deadline = [DateTime]::UtcNow.AddSeconds(30)
        while ([DateTime]::UtcNow -lt $deadline) {
            if ($process.HasExited) { throw "application exited before creating a window (code $($process.ExitCode))" }
            $process.Refresh()
            if ($process.MainWindowHandle -ne [IntPtr]::Zero) {
                $start.Stop()
                return [Math]::Round($start.Elapsed.TotalMilliseconds, 3)
            }
            Start-Sleep -Milliseconds 10
        }
        throw "application did not expose a main window within 30 seconds"
    } finally {
        Stop-ProbeProcess $process
    }
}

function Measure-AppProbe {
    $cold = New-Object System.Collections.Generic.List[double]
    for ($run = 0; $run -lt $StartupRuns; $run++) {
        $cold.Add((Measure-WindowReady @()))
    }

    $largeOpen = Measure-WindowReady @($script:ArchivePath)
    $process = Start-Process -FilePath $script:ReleaseExe -PassThru -WindowStyle Normal
    try {
        $deadline = [DateTime]::UtcNow.AddSeconds(30)
        while ([DateTime]::UtcNow -lt $deadline) {
            if ($process.HasExited) { throw "application exited during idle-memory probe" }
            $process.Refresh()
            if ($process.MainWindowHandle -ne [IntPtr]::Zero) { break }
            Start-Sleep -Milliseconds 10
        }
        Start-Sleep -Seconds $IdleSeconds
        $process.Refresh()
        $idleWorkingSet = $process.WorkingSet64
        $idlePrivate = $process.PrivateMemorySize64
    } finally {
        Stop-ProbeProcess $process
    }

    $sorted = @($cold | Sort-Object)
    $middle = [Math]::Floor($sorted.Count / 2)
    $median = if (($sorted.Count % 2) -eq 0) {
        ($sorted[$middle - 1] + $sorted[$middle]) / 2
    } else {
        $sorted[$middle]
    }
    [ordered]@{
        startup_window_ready_ms = $cold
        startup_window_ready_median_ms = [Math]::Round($median, 3)
        idle_sample_after_seconds = $IdleSeconds
        idle_working_set_mib = [Math]::Round($idleWorkingSet / 1MB, 3)
        idle_private_mib = [Math]::Round($idlePrivate / 1MB, 3)
        large_archive_window_ready_ms = $largeOpen
        note = "Window-ready timing proves process/window readiness only; engine timings separately measure complete list/model preparation."
    }
}

Set-RuntimeEnvironment
$result = [ordered]@{
    generated_at_utc = [DateTime]::UtcNow.ToString("o")
    host = [ordered]@{
        os = [System.Environment]::OSVersion.VersionString
        processor_count = [System.Environment]::ProcessorCount
        powershell = $PSVersionTable.PSVersion.ToString()
    }
    parameters = [ordered]@{
        mode = $Mode
        entry_count = $EntryCount
        payload_mib = $PayloadMiB
        startup_runs = $StartupRuns
        idle_seconds = $IdleSeconds
        allow_unsupported_runtime = [bool]$AllowUnsupportedRuntime
    }
}

if ($Mode -in @("All", "Prepare")) {
    $result.fixture = New-Fixture
} elseif (-not (Test-Path -LiteralPath $script:ArchivePath)) {
    throw "Run with -Mode Prepare or -Mode All before measuring"
} else {
    $result.fixture = [ordered]@{
        archive = $script:ArchivePath
        archive_bytes = (Get-Item -LiteralPath $script:ArchivePath).Length
    }
}

if ($Mode -in @("All", "Engine", "App")) {
    Invoke-Build
}
if ($Mode -in @("All", "Engine")) {
    $result.engine = Invoke-EngineProbe
}
if ($Mode -in @("All", "App")) {
    $result.app = Measure-AppProbe
}

$resolvedResults = Resolve-ResultPath $ResultsPath
$resultsDirectory = Split-Path -Parent $resolvedResults
New-Item -ItemType Directory -Force -Path $resultsDirectory | Out-Null
$result | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $resolvedResults -Encoding utf8
$result | ConvertTo-Json -Depth 8

