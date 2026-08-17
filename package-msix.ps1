[CmdletBinding()]
param(
    [string]$OutputDirectory = "dist\msix",
    [string]$IdentityName = "kirinonakar.ArchiveRclick",
    [string]$Publisher = "CN=5F40B554-380B-47DB-92AD-D044EB10269C",
    [string]$PublisherDisplayName = "kirinonakar",
    [string]$DisplayName = "ArchiveRclick",
    [switch]$StoreSubmission,
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$repository = $PSScriptRoot
$cargoManifestPath = Join-Path $repository "Cargo.toml"
$distRoot = [System.IO.Path]::GetFullPath((Join-Path $repository "dist"))
$output = [System.IO.Path]::GetFullPath((Join-Path $repository $OutputDirectory))
$templatePath = Join-Path $repository "packaging\msix\Package.appxmanifest.template.xml"
$runtimeDirectory = Join-Path $repository "runtime\x64"
$runtimeHashesPath = Join-Path $repository "runtime\SHA256SUMS"
$releaseDirectory = Join-Path $repository "target\release"

function Assert-ChildPath {
    param(
        [string]$Path,
        [string]$Parent,
        [string]$Description
    )

    $parentWithSeparator = $Parent.TrimEnd('\') + [System.IO.Path]::DirectorySeparatorChar
    if (-not $Path.StartsWith($parentWithSeparator, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "$Description must be inside $Parent"
    }
}

function Invoke-Native {
    param(
        [string]$FilePath,
        [string[]]$ArgumentList
    )

    & $FilePath @ArgumentList
    if ($LASTEXITCODE -ne 0) {
        throw "$([System.IO.Path]::GetFileName($FilePath)) failed with exit code $LASTEXITCODE"
    }
}

function New-ScaledPng {
    param(
        [string]$Source,
        [string]$Destination,
        [int]$Size
    )

    Add-Type -AssemblyName System.Drawing
    $sourceImage = $null
    $bitmap = $null
    $graphics = $null
    try {
        $sourceImage = [System.Drawing.Image]::FromFile($Source)
        $bitmap = [System.Drawing.Bitmap]::new(
            $Size,
            $Size,
            [System.Drawing.Imaging.PixelFormat]::Format32bppArgb
        )
        $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
        $graphics.Clear([System.Drawing.Color]::Transparent)
        $graphics.CompositingMode = [System.Drawing.Drawing2D.CompositingMode]::SourceCopy
        $graphics.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
        $graphics.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
        $graphics.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::HighQuality
        $graphics.DrawImage($sourceImage, 0, 0, $Size, $Size)
        $bitmap.Save($Destination, [System.Drawing.Imaging.ImageFormat]::Png)
    }
    finally {
        if ($graphics) { $graphics.Dispose() }
        if ($bitmap) { $bitmap.Dispose() }
        if ($sourceImage) { $sourceImage.Dispose() }
    }
}

function Get-CargoPackageVersion {
    param([string]$ManifestPath)

    $inPackageTable = $false
    foreach ($line in Get-Content -LiteralPath $ManifestPath) {
        if ($line -match '^\s*\[package\]\s*$') {
            $inPackageTable = $true
            continue
        }
        if ($inPackageTable -and $line -match '^\s*\[') {
            break
        }
        if ($inPackageTable -and $line -match '^\s*version\s*=\s*"([^"]+)"') {
            return $Matches[1]
        }
    }
    throw "Could not find the [package] version in $ManifestPath"
}

function Find-WindowsKitTool {
    param([string]$Name)

    $candidates = @()
    $kitsRoot = Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10\bin"
    if (Test-Path -LiteralPath $kitsRoot) {
        $candidates = @(Get-ChildItem -LiteralPath $kitsRoot -Filter $Name -File -Recurse -ErrorAction SilentlyContinue |
            Where-Object { $_.DirectoryName -match "\\x64$" } |
            Sort-Object FullName -Descending)
    }
    $command = Get-Command $Name -ErrorAction SilentlyContinue
    if ($command) {
        $candidates += [System.IO.FileInfo]$command.Source
    }
    $tool = $candidates | Select-Object -First 1
    if (-not $tool) {
        throw "Could not find $Name. Install the Windows 10/11 SDK."
    }
    $tool.FullName
}

function New-PackageManifest {
    param(
        [string]$Template,
        [string]$Destination
    )

    $manifest = Get-Content -LiteralPath $Template -Raw
    $replacements = [ordered]@{
        "__PACKAGE_NAME__" = [System.Security.SecurityElement]::Escape($IdentityName)
        "__PUBLISHER__" = [System.Security.SecurityElement]::Escape($Publisher)
        "__VERSION__" = [System.Security.SecurityElement]::Escape($Version)
        "__DISPLAY_NAME__" = [System.Security.SecurityElement]::Escape($DisplayName)
        "__PUBLISHER_DISPLAY_NAME__" = [System.Security.SecurityElement]::Escape($PublisherDisplayName)
    }
    foreach ($entry in $replacements.GetEnumerator()) {
        $manifest = $manifest.Replace($entry.Key, $entry.Value)
    }
    if ($manifest -match "__[A-Z0-9_]+__") {
        throw "The generated manifest still contains an unreplaced placeholder"
    }
    [System.IO.File]::WriteAllText($Destination, $manifest, [System.Text.UTF8Encoding]::new($false))
}

if (-not (Test-Path -LiteralPath $templatePath)) {
    throw "Manifest template not found: $templatePath"
}
Assert-ChildPath -Path $output -Parent $distRoot -Description "Output directory"

if ($IdentityName -notmatch '^[A-Za-z0-9][A-Za-z0-9.-]{0,49}$') {
    throw "IdentityName must contain only letters, digits, dots, and hyphens and be at most 50 characters"
}
if ([string]::IsNullOrWhiteSpace($Publisher) -or $Publisher.Contains('"') -or $Publisher.Contains('<')) {
    throw "Publisher must be the exact Publisher string from Partner Center"
}
$cargoVersion = Get-CargoPackageVersion -ManifestPath $cargoManifestPath
if ($cargoVersion -notmatch '^(?<major>[1-9][0-9]{0,4})\.(?<minor>[0-9]{1,5})\.(?<patch>[0-9]{1,5})$') {
    throw "Cargo package version '$cargoVersion' must be a stable Store-compatible x.y.z version with a non-zero major component"
}
$Version = "$($Matches.major).$($Matches.minor).$($Matches.patch).0"
if (-not $SkipBuild) {
    Push-Location $repository
    try {
        cargo build --release --locked
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build --release failed"
        }
    }
    finally {
        Pop-Location
    }
}

$requiredBinaries = @(
    (Join-Path $releaseDirectory "archive-rclick.exe"),
    (Join-Path $releaseDirectory "archive_rclick_core.dll")
)
foreach ($binary in $requiredBinaries) {
    if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
        throw "Release binary not found: $binary"
    }
}

$expected = [ordered]@{}
foreach ($line in Get-Content -LiteralPath $runtimeHashesPath) {
    if ($line -match '^([0-9a-fA-F]{64})\s{2}(.+)$') {
        $expected[$Matches[2]] = $Matches[1].ToLowerInvariant()
    }
}
if ($expected.Count -eq 0) {
    throw "No runtime hashes found in $runtimeHashesPath"
}
foreach ($entry in $expected.GetEnumerator()) {
    $source = Join-Path $runtimeDirectory $entry.Key
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
        throw "Runtime file not found: $source"
    }
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $source).Hash.ToLowerInvariant()
    if ($actual -ne $entry.Value) {
        throw "Runtime hash mismatch for $($entry.Key): expected $($entry.Value), got $actual"
    }
}

if (Test-Path -LiteralPath $output) {
    Remove-Item -LiteralPath $output -Recurse -Force
}
$packageRoot = Join-Path $output "package"
$assetsRoot = Join-Path $packageRoot "Assets"
New-Item -ItemType Directory -Force -Path $assetsRoot | Out-Null

Copy-Item -LiteralPath (Join-Path $releaseDirectory "archive-rclick.exe") -Destination $packageRoot
Copy-Item -LiteralPath (Join-Path $releaseDirectory "archive_rclick_core.dll") -Destination $packageRoot
foreach ($entry in $expected.GetEnumerator()) {
    Copy-Item -LiteralPath (Join-Path $runtimeDirectory $entry.Key) -Destination $packageRoot
}
Copy-Item -LiteralPath (Join-Path $repository "runtime\THIRD-PARTY-NOTICES.md") -Destination $packageRoot
Copy-Item -LiteralPath (Join-Path $repository "THIRD-PARTY-LICENSES.md") -Destination $packageRoot
Copy-Item -LiteralPath $runtimeHashesPath -Destination $packageRoot
Copy-Item -LiteralPath (Join-Path $repository "runtime\licenses") -Destination $packageRoot -Recurse

# Generate the manifest logos at their actual target sizes. Using the 980px
# source directly in every slot makes Explorer resample the package icon again
# for small context-menu surfaces, which can make it look soft or squashed.
$sourceArtwork = Join-Path $repository "app.png"
$assetSizes = [ordered]@{
    "StoreLogo.png" = 50
    "StoreLogo.scale-200.png" = 100
    "Square150x150Logo.png" = 150
    "Square150x150Logo.scale-200.png" = 300
    "Square44x44Logo.png" = 44
    "Square44x44Logo.scale-200.png" = 88
    "Square44x44Logo.scale-400.png" = 176
}
foreach ($asset in $assetSizes.GetEnumerator()) {
    New-ScaledPng `
        -Source $sourceArtwork `
        -Destination (Join-Path $assetsRoot $asset.Key) `
        -Size $asset.Value
}

$manifestPath = Join-Path $packageRoot "AppxManifest.xml"
New-PackageManifest -Template $templatePath -Destination $manifestPath

$safeVersion = $Version.Replace('.', '_')
$packageFileName = "${IdentityName}_${safeVersion}_x64.msix"
$msixPath = Join-Path $output $packageFileName
$makeAppx = Find-WindowsKitTool -Name "makeappx.exe"
Invoke-Native -FilePath $makeAppx -ArgumentList @("pack", "/d", $packageRoot, "/p", $msixPath, "/o")

$certificatePath = Join-Path $output "${IdentityName}_signing.cer"

if ($StoreSubmission) {
    # Partner Center signs the package during Store publication. Do not create
    # or attach a local development certificate to the upload artifact.
    Write-Host "Skipping local development certificate for Store submission."
}
else {
    # A locally sideloaded MSIX must be signed, and the certificate subject must
    # exactly match the Publisher value in AppxManifest.xml. Reuse the same
    # development certificate from the current user's certificate store so the
    # exported .cer remains stable across packaging runs. Only the public
    # certificate is written next to the MSIX.
    foreach ($certificateCommand in @("New-SelfSignedCertificate", "Export-Certificate")) {
        if (-not (Get-Command $certificateCommand -ErrorAction SilentlyContinue)) {
            throw "Could not find $certificateCommand. Run this script on Windows with the PKI PowerShell module installed."
        }
    }

    $certificateFriendlyName = "$IdentityName MSIX development certificate"
    $certificate = Get-ChildItem -LiteralPath "Cert:\CurrentUser\My" |
        Where-Object {
            $_.Subject -eq $Publisher -and
            $_.FriendlyName -eq $certificateFriendlyName -and
            $_.HasPrivateKey -and
            $_.NotAfter -gt (Get-Date)
        } |
        Sort-Object NotBefore -Descending |
        Select-Object -First 1

    if ($certificate) {
        Write-Host "Reusing development certificate: $($certificate.Thumbprint)"
    }
    else {
        $certificateParameters = @{
            Type = "Custom"
            Subject = $Publisher
            FriendlyName = $certificateFriendlyName
            KeyUsage = "DigitalSignature"
            KeyAlgorithm = "RSA"
            KeyLength = 2048
            HashAlgorithm = "SHA256"
            KeyExportPolicy = "NonExportable"
            CertStoreLocation = "Cert:\CurrentUser\My"
            NotAfter = (Get-Date).AddYears(3)
            TextExtension = @(
                "2.5.29.37={text}1.3.6.1.5.5.7.3.3"
                "2.5.29.19={text}"
            )
        }
        $certificate = New-SelfSignedCertificate @certificateParameters
        if (-not $certificate) {
            throw "Could not create the MSIX development certificate"
        }
        Write-Host "Created development certificate: $($certificate.Thumbprint)"
    }

    Export-Certificate -Cert $certificate -FilePath $certificatePath -Type CERT | Out-Null
    $signTool = Find-WindowsKitTool -Name "signtool.exe"
    Invoke-Native -FilePath $signTool -ArgumentList @(
        "sign", "/fd", "SHA256", "/sha1", $certificate.Thumbprint, $msixPath
    )
}

$unpacked = Join-Path $output "package-verify"
Invoke-Native -FilePath $makeAppx -ArgumentList @("unpack", "/p", $msixPath, "/d", $unpacked, "/o")
try {
    $verifyManifest = Join-Path $unpacked "AppxManifest.xml"
    if (-not (Test-Path -LiteralPath $verifyManifest -PathType Leaf)) {
        throw "The generated MSIX does not contain AppxManifest.xml"
    }
    foreach ($required in @("archive-rclick.exe", "archive_rclick_core.dll", "Assets\Square44x44Logo.png")) {
        if (-not (Test-Path -LiteralPath (Join-Path $unpacked $required) -PathType Leaf)) {
            throw "The generated MSIX is missing $required"
        }
    }
}
finally {
    Remove-Item -LiteralPath $unpacked -Recurse -Force -ErrorAction SilentlyContinue
}

$uploadRoot = Join-Path $output "msixupload"
New-Item -ItemType Directory -Force -Path $uploadRoot | Out-Null
Copy-Item -LiteralPath $msixPath -Destination $uploadRoot

$pdbFiles = @(Get-ChildItem -LiteralPath $releaseDirectory -Filter "*.pdb" -File -ErrorAction SilentlyContinue)
if ($pdbFiles.Count -gt 0) {
    $symbolsPath = Join-Path $uploadRoot ("${IdentityName}_${safeVersion}_x64.appxsym")
    $symbolStage = Join-Path $output "symbols"
    New-Item -ItemType Directory -Force -Path $symbolStage | Out-Null
    foreach ($pdb in $pdbFiles) {
        Copy-Item -LiteralPath $pdb.FullName -Destination $symbolStage
    }
    Compress-Archive -Path (Join-Path $symbolStage "*") -DestinationPath $symbolsPath -CompressionLevel Optimal
    Remove-Item -LiteralPath $symbolStage -Recurse -Force
}

$uploadPath = Join-Path $output ("${IdentityName}_${safeVersion}_x64.msixupload")
if (Test-Path -LiteralPath $uploadPath) {
    Remove-Item -LiteralPath $uploadPath -Force
}
Compress-Archive -Path (Join-Path $uploadRoot "*") -DestinationPath $uploadPath -CompressionLevel Optimal
Remove-Item -LiteralPath $uploadRoot -Recurse -Force

Write-Host "MSIX package: $msixPath"
if (-not $StoreSubmission) {
    Write-Host "Certificate: $certificatePath"
}
Write-Host "Store upload file: $uploadPath"
if ($StoreSubmission) {
    Write-Host "The .msixupload contains the unsigned package for Partner Center submission."
}
if (-not $StoreSubmission) {
    Write-Host "The MSIX is signed with the development certificate. Trust the .cer before installing it."
}
