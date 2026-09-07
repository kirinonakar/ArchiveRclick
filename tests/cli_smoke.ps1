[CmdletBinding()]
param([string]$Executable = "$PSScriptRoot\..\dist\msix\package\archive-rclick-cli.exe")
$ErrorActionPreference = 'Stop'
$cli = (Resolve-Path -LiteralPath $Executable).Path
$msixRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\dist\msix'))
$scratch = Join-Path $msixRoot ('.smoke-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $scratch | Out-Null

function Invoke-Cli {
    param([string[]]$Arguments, [int]$ExpectedExit = 0)
    $start = [Diagnostics.ProcessStartInfo]::new($cli)
    $start.WorkingDirectory = $scratch
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $start.RedirectStandardInput = $true
    $start.StandardOutputEncoding = [Text.Encoding]::UTF8
    foreach ($argument in $Arguments) { $start.ArgumentList.Add($argument) }
    $process = [Diagnostics.Process]::Start($start)
    $process.StandardInput.Close()
    $stdout = $process.StandardOutput.ReadToEndAsync()
    $stderr = $process.StandardError.ReadToEndAsync()
    if (-not $process.WaitForExit(60000)) { $process.Kill($true); throw 'CLI timed out' }
    $text = $stdout.GetAwaiter().GetResult() + $stderr.GetAwaiter().GetResult()
    if ($process.ExitCode -ne $ExpectedExit) {
        throw "Expected exit $ExpectedExit, got $($process.ExitCode): $($Arguments -join ' ')`n$text"
    }
    $process.Dispose()
    return $text
}
function Assert-Content([string]$Relative, [string]$Expected) {
    $actual = [IO.File]::ReadAllText((Join-Path $scratch $Relative))
    if ($actual -cne $Expected) { throw "Content mismatch: $Relative" }
}

try {
    $inputRoot = Join-Path $scratch 'input folder'
    New-Item -ItemType Directory -Path (Join-Path $inputRoot 'nested') | Out-Null
    New-Item -ItemType Directory -Path (Join-Path $inputRoot 'empty') | Out-Null
    [IO.File]::WriteAllText((Join-Path $inputRoot '한글.txt'), 'UTF-8 한글 payload')
    [IO.File]::WriteAllText((Join-Path $inputRoot 'nested\data.bin'), 'nested payload')
    [IO.File]::WriteAllText((Join-Path $inputRoot 'README'), 'extensionless')

    $help = Invoke-Cli @('--help')
    if (-not $help.Contains('Usage:')) { throw 'Missing help' }
    Invoke-Cli @('i') | Out-Null
    foreach ($type in @('7z', 'zip', 'tar', 'tar.gz', 'tar.xz', 'tar.zst')) {
        $archive = "sample.$type"
        Invoke-Cli @('a', $archive, 'input folder', '-mx=1', '-mmt=2') | Out-Null
        $readArchive = $archive
        if ($type -eq 'tar.gz') {
            Invoke-Cli @('t', $archive) | Out-Null
            $wrappedListing = Invoke-Cli @('l', $archive)
            if (-not $wrappedListing.Contains('sample.tar')) { throw 'Missing wrapped TAR payload' }
            Invoke-Cli @('x', $archive, "-owrapper-$type", '-y') | Out-Null
            $readArchive = "wrapper-$type\sample.tar"
        }
        $listing = Invoke-Cli @('l', $readArchive, '-slt')
        if (-not ($listing.Contains('한글.txt') -and $listing.Contains('input folder'))) { throw "Bad listing: $type" }
        Invoke-Cli @('t', $readArchive) | Out-Null
        Invoke-Cli @('x', $readArchive, "-oout-$type", '-y') | Out-Null
        Assert-Content "out-$type\input folder\한글.txt" 'UTF-8 한글 payload'
        Assert-Content "out-$type\input folder\nested\data.bin" 'nested payload'
        Invoke-Cli @('e', $readArchive, "-oflat-$type", '-y') | Out-Null
        Assert-Content "flat-$type\data.bin" 'nested payload'
        if (Test-Path -LiteralPath (Join-Path $scratch "flat-$type\input folder")) { throw 'e preserved directories' }
        $before = (Get-FileHash -LiteralPath (Join-Path $scratch $archive)).Hash
        Invoke-Cli @('a', $archive, 'input folder') -ExpectedExit 2 | Out-Null
        if ($before -ne (Get-FileHash -LiteralPath (Join-Path $scratch $archive)).Hash) { throw 'Existing archive changed' }
        Write-Host "PASS $type create/list/test/extract/flatten/preserve-existing"
    }
    Invoke-Cli @('a', 'secret.7z', 'input folder', '-pTestSecret', '-mhe=on', '-mx=1') | Out-Null
    Invoke-Cli @('t', 'secret.7z', '-pWrongPassword') -ExpectedExit 2 | Out-Null
    Invoke-Cli @('x', 'secret.7z', '-pTestSecret', '-osecret', '-y') | Out-Null
    Assert-Content 'secret\input folder\한글.txt' 'UTF-8 한글 payload'

    $bytes = [byte[]]::new(16000)
    [Random]::new(7).NextBytes($bytes)
    [IO.File]::WriteAllBytes((Join-Path $scratch 'random.bin'), $bytes)
    foreach ($type in @('zip', '7z')) {
        Invoke-Cli @('a', "split.$type", 'random.bin', '-v2k', '-mx=0') | Out-Null
        Invoke-Cli @('t', "split.$type.001") | Out-Null
        Invoke-Cli @('x', "split.$type.001", "-osplit-$type", '-y') | Out-Null
        if ((Get-FileHash -LiteralPath (Join-Path $scratch 'random.bin')).Hash -ne
            (Get-FileHash -LiteralPath (Join-Path $scratch "split-$type\random.bin")).Hash) { throw 'Split hash mismatch' }
    }
    Invoke-Cli @('x', 'sample.zip', '*.txt', '-r', '-oselected', '-y') | Out-Null
    Assert-Content 'selected\input folder\한글.txt' 'UTF-8 한글 payload'
    if (Test-Path -LiteralPath (Join-Path $scratch 'selected\input folder\nested\data.bin')) { throw 'Filter failed' }
    [IO.File]::WriteAllText((Join-Path $scratch 'flat-zip\data.bin'), 'keep me')
    Invoke-Cli @('e', 'sample.zip', '-oflat-zip', '-aos') | Out-Null
    Assert-Content 'flat-zip\data.bin' 'keep me'
    Invoke-Cli @('e', 'sample.zip', '-oflat-zip', '-aoa') | Out-Null
    Assert-Content 'flat-zip\data.bin' 'nested payload'
    Invoke-Cli @('a', 'wild.zip', 'input folder\*.*') | Out-Null
    $listing = Invoke-Cli @('l', 'wild.zip')
    if (-not $listing.Contains('README')) { throw '*.* did not match extensionless input' }
    [IO.File]::WriteAllText((Join-Path $scratch 'files.txt'), '"input folder\한글.txt"')
    Invoke-Cli @('a', 'list.zip', '@files.txt') | Out-Null
    Invoke-Cli @('t', 'list.zip') | Out-Null
    Invoke-Cli @('a', 'default', 'random.bin') | Out-Null
    if (-not (Test-Path -LiteralPath (Join-Path $scratch 'default.7z'))) { throw 'Missing default extension' }
    Invoke-Cli @('t', 'missing.7z') -ExpectedExit 2 | Out-Null
    Invoke-Cli @('u', 'sample.7z') -ExpectedExit 7 | Out-Null
    Invoke-Cli @('a', 'invalid.7z', 'random.bin', '-mx=10') -ExpectedExit 7 | Out-Null
    Invoke-Cli @('l', 'sample.zip', '-unsupported') -ExpectedExit 7 | Out-Null
    foreach ($side in @('left', 'right')) {
        New-Item -ItemType Directory -Path (Join-Path $scratch "collisions\$side") -Force | Out-Null
        [IO.File]::WriteAllText((Join-Path $scratch "collisions\$side\same.txt"), $side)
    }
    foreach ($type in @('zip', '7z', 'tar')) {
        Invoke-Cli @('a', "collision.$type", 'collisions') | Out-Null
        Invoke-Cli @('e', "collision.$type", "-ocollision-skip-$type", '-aos') | Out-Null
        Assert-Content "collision-skip-$type\same.txt" 'left'
        Invoke-Cli @('e', "collision.$type", "-ocollision-overwrite-$type", '-aoa') | Out-Null
        Assert-Content "collision-overwrite-$type\same.txt" 'right'
    }
    Write-Host 'PASS flattened duplicate filename conflict handling (zip/7z/tar)'
    $unsafeZip = [IO.Compression.ZipFile]::Open((Join-Path $scratch 'unsafe.zip'), [IO.Compression.ZipArchiveMode]::Create)
    try {
        $entry = $unsafeZip.CreateEntry('../escape.txt')
        $writer = [IO.StreamWriter]::new($entry.Open())
        try { $writer.Write('must not escape') } finally { $writer.Dispose() }
    } finally { $unsafeZip.Dispose() }
    Invoke-Cli @('e', 'unsafe.zip', '-ounsafe-out', '-y') -ExpectedExit 2 | Out-Null
    if (Test-Path -LiteralPath (Join-Path $scratch 'escape.txt')) { throw 'Path traversal escaped destination' }
    Write-Host 'PASS flattened extraction rejects path traversal'
    Write-Host 'PASS passwords/split/filter/overwrite/wildcards/listfile/defaults/error-codes'
}
finally {
    $resolved = [IO.Path]::GetFullPath($scratch)
    if (-not $resolved.StartsWith($msixRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Refusing cleanup outside dist/msix'
    }
    Remove-Item -LiteralPath $resolved -Recurse -Force
}
