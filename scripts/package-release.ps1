[CmdletBinding()]
param(
    [string]$OutputDirectory = "dist\ArchiveRclick"
)

$ErrorActionPreference = "Stop"
$repository = Split-Path -Parent $PSScriptRoot
$runtimeDirectory = Join-Path $repository "runtime\x64"
$manifestPath = Join-Path $repository "runtime\SHA256SUMS"
$output = [System.IO.Path]::GetFullPath((Join-Path $repository $OutputDirectory))
$distRoot = [System.IO.Path]::GetFullPath((Join-Path $repository "dist"))

if (-not $output.StartsWith($distRoot + [System.IO.Path]::DirectorySeparatorChar,
        [System.StringComparison]::OrdinalIgnoreCase)) {
    throw "Output directory must be a child of $distRoot"
}

Push-Location $repository
try {
    Remove-Item Env:ARCHIVERCLICK_ALLOW_UNSUPPORTED_LIBARCHIVE -ErrorAction SilentlyContinue
    Remove-Item Env:ARCHIVERCLICK_LIBARCHIVE -ErrorAction SilentlyContinue

    $target = "x86_64-pc-windows-msvc"
    cargo build --release --locked --target $target
    if ($LASTEXITCODE -ne 0) { throw "cargo build --release failed" }

    if (Test-Path -LiteralPath $output) {
        Remove-Item -LiteralPath $output -Recurse -Force
    }
    New-Item -ItemType Directory -Force -Path $output | Out-Null

    $expected = [ordered]@{}
    foreach ($line in Get-Content -LiteralPath $manifestPath) {
        if ($line -match '^([0-9a-fA-F]{64})\s{2}(.+)$') {
            $expected[$Matches[2]] = $Matches[1].ToLowerInvariant()
        }
    }
    if ($expected.Count -eq 0) { throw "No runtime hashes found in $manifestPath" }

    foreach ($entry in $expected.GetEnumerator()) {
        $source = Join-Path $runtimeDirectory $entry.Key
        $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $source).Hash.ToLowerInvariant()
        if ($actual -ne $entry.Value) {
            throw "Hash mismatch for $($entry.Key): expected $($entry.Value), got $actual"
        }
        Copy-Item -LiteralPath $source -Destination (Join-Path $output $entry.Key)
    }

    $releaseDirectory = Join-Path $repository "target\$target\release"
    Copy-Item -LiteralPath (Join-Path $releaseDirectory "archive-rclick.exe") -Destination $output
    Copy-Item -LiteralPath (Join-Path $releaseDirectory "archive_rclick.dll") -Destination $output
    Copy-Item -LiteralPath (Join-Path $repository "runtime\THIRD-PARTY-NOTICES.md") -Destination $output
    Copy-Item -LiteralPath $manifestPath -Destination $output
    Copy-Item -LiteralPath (Join-Path $repository "runtime\licenses") -Destination $output -Recurse

    $actualDlls = @(Get-ChildItem -LiteralPath $output -Filter '*.dll' -File | Where-Object { $_.Name -ne 'archive_rclick.dll' } | ForEach-Object Name | Sort-Object)
    $expectedDlls = @($expected.Keys | Sort-Object)
    if (Compare-Object $expectedDlls $actualDlls) {
        throw "Packaged DLL set does not match runtime allowlist"
    }

    $smoke = Start-Process `
        -FilePath (Join-Path $output "archive-rclick.exe") `
        -ArgumentList "--check-runtime" `
        -WindowStyle Hidden `
        -Wait `
        -PassThru
    if ($smoke.ExitCode -ne 0) {
        throw "Packaged runtime smoke check failed with exit code $($smoke.ExitCode)"
    }

    Write-Host "Portable release created at $output"
}
finally {
    Pop-Location
}
