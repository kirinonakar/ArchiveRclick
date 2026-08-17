[CmdletBinding()]
param(
    [string]$OutputDirectory = "dist\msix",
    [string]$IdentityName = "ArchiveRclick",
    [string]$Publisher = "CN=ArchiveRclick",
    [string]$PublisherDisplayName = "ArchiveRclick",
    [string]$DisplayName = "ArchiveRclick",
    [string]$Version = "1.0.0.0",
    [switch]$StoreSubmission,
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$repository = $PSScriptRoot
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
if ($Version -notmatch '^[1-9][0-9]{0,4}\.[0-9]{1,5}\.[0-9]{1,5}\.0$') {
    throw "Version must be a four-part Store version such as 1.0.0.0; the fourth part must be 0"
}
if ($StoreSubmission -and $IdentityName -eq "ArchiveRclick" -and $Publisher -eq "CN=ArchiveRclick") {
    throw "StoreSubmission requires the exact Identity Name and Publisher from Partner Center. Pass -IdentityName and -Publisher explicitly."
}

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

# The source artwork is square and already contains the application icon. The
# same unqualified asset works for the manifest's package logo slots and keeps
# packaging deterministic without requiring an image-editing dependency.
foreach ($assetName in @("StoreLogo.png", "Square150x150Logo.png", "Square44x44Logo.png")) {
    Copy-Item -LiteralPath (Join-Path $repository "app.png") -Destination (Join-Path $assetsRoot $assetName)
}

$manifestPath = Join-Path $packageRoot "AppxManifest.xml"
New-PackageManifest -Template $templatePath -Destination $manifestPath

$safeVersion = $Version.Replace('.', '_')
$packageFileName = "${IdentityName}_${safeVersion}_x64.msix"
$msixPath = Join-Path $output $packageFileName
$makeAppx = Find-WindowsKitTool -Name "makeappx.exe"
Invoke-Native -FilePath $makeAppx -ArgumentList @("pack", "/d", $packageRoot, "/p", $msixPath, "/o")

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

$certificatePath = Join-Path $output "${IdentityName}_signing.cer"
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

if (-not $StoreSubmission) {
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
Write-Host "Certificate: $certificatePath"
Write-Host "Store upload file: $uploadPath"
if ($StoreSubmission) {
    Write-Warning "The generated .cer is for local testing only; Microsoft signs the MSIX during Store publication."
}
if (-not $StoreSubmission) {
    Write-Host "The MSIX is signed with the development certificate. Trust the .cer before installing it."
    if ($IdentityName -eq "ArchiveRclick" -and $Publisher -eq "CN=ArchiveRclick") {
        Write-Warning "This build uses the default development identity. For Partner Center, pass the exact Name and Publisher shown in the app's identity details with -StoreSubmission."
    }
}
