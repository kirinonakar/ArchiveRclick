[CmdletBinding()]
param(
    [string]$OutputDirectory = "dist\ArchiveRclick"
)

$ErrorActionPreference = "Stop"
# The script works both from the repository root and from scripts/. Pick the
# directory that actually contains the Cargo.toml.
$repository = if (Test-Path -LiteralPath (Join-Path $PSScriptRoot "Cargo.toml")) {
    $PSScriptRoot
} else {
    Split-Path -Parent $PSScriptRoot
}
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

    # The library target is named archive_rclick_core while the executable
    # keeps the package name archive-rclick, so their .pdb files (which Cargo
    # names after each crate) no longer collide on MSVC.
    cargo build --release --locked
    if ($LASTEXITCODE -ne 0) { throw "cargo build --release failed" }

    if (Test-Path -LiteralPath $output) {
        # A file from the previous package can be locked by a running app or
        # by Explorer (when the right-click menu is registered from this
        # folder). Loaded DLLs can be renamed but not deleted, so rename any
        # locked file out of the way and remove the rest; the renamed
        # leftovers disappear on the next packaging run.
        $script:keptFiles = @()
        Get-ChildItem -LiteralPath $output -File -Recurse -ErrorAction SilentlyContinue | ForEach-Object {
            $filePath = $_.FullName
            $fileName = $_.Name
            try {
                Remove-Item -LiteralPath $filePath -Force -ErrorAction Stop
            } catch {
                $suffix = "old-" + [guid]::NewGuid().ToString('N').Substring(0, 8)
                Rename-Item -LiteralPath $filePath -NewName "$fileName.$suffix" -Force -ErrorAction SilentlyContinue
                $script:keptFiles = @($script:keptFiles) + $fileName
            }
        }
        Remove-Item -LiteralPath $output -Recurse -Force -ErrorAction SilentlyContinue
        if ($keptFiles.Count -gt 0) {
            Write-Host "Kept locked file(s) from the previous package: $($keptFiles -join ', '). Restart Explorer or close the running app, then package again to remove them."
        }
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

    $releaseDirectory = Join-Path $repository "target\release"
    Copy-Item -LiteralPath (Join-Path $releaseDirectory "archive-rclick.exe") -Destination $output
    Copy-Item -LiteralPath (Join-Path $releaseDirectory "archive_rclick_core.dll") -Destination $output
    Copy-Item -LiteralPath (Join-Path $repository "runtime\THIRD-PARTY-NOTICES.md") -Destination $output
    Copy-Item -LiteralPath (Join-Path $repository "THIRD-PARTY-LICENSES.md") -Destination $output
    Copy-Item -LiteralPath $manifestPath -Destination $output
    Copy-Item -LiteralPath (Join-Path $repository "runtime\licenses") -Destination $output -Recurse

    $actualDlls = @(Get-ChildItem -LiteralPath $output -Filter '*.dll' -File | Where-Object { $_.Name -ne 'archive_rclick_core.dll' } | ForEach-Object Name | Sort-Object)
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
