# ArchiveRclick 우클릭 컨텍스트 메뉴 헬퍼
# 사용법: archive-rclick-context.ps1 <extract|zip|7z> <경로>
#   extract - 압축 파일명의 하위 폴더에 압축 풀기 (zip: 내장, 그 외: 7-Zip 필요)
#   zip     - 대상(파일/폴더)명.zip 생성
#  7z      - 대상(파일/폴더)명.7z 생성 (7-Zip 필요)
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('extract', 'zip', '7z')]
    [string]$Action,

    [Parameter(Mandatory = $true)]
    [string]$Target
)

$ErrorActionPreference = 'Stop'

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.IO.Compression.FileSystem -ErrorAction SilentlyContinue
Add-Type -AssemblyName System.IO.Compression -ErrorAction SilentlyContinue

$ArchiveExtensions = @(
    '.zip', '.zipx', '.7z', '.rar', '.tar', '.gz', '.bz2',
    '.xz', '.zst', '.cab', '.lha', '.lzh', '.tgz', '.tbz2', '.txz'
)

function Show-Error([string]$Message) {
    [System.Windows.Forms.MessageBox]::Show(
        $Message,
        'ArchiveRclick',
        [System.Windows.Forms.MessageBoxButtons]::OK,
        [System.Windows.Forms.MessageBoxIcon]::Error
    ) | Out-Null
    exit 1
}

function Find-7Zip {
    $candidates = @(
        (Join-Path $env:ProgramFiles '7-Zip\7z.exe'),
        (Join-Path ${env:ProgramFiles(x86)} '7-Zip\7z.exe'),
        (Join-Path $env:LOCALAPPDATA 'Programs\7-Zip\7z.exe')
    )
    foreach ($candidate in $candidates) {
        if ($candidate -and (Test-Path -LiteralPath $candidate)) {
            return $candidate
        }
    }
    return $null
}

# .tar.gz 처럼 이중 확장자면 .tar까지 제거한 폴더명 반환
function Get-ArchiveFolderName([string]$Path) {
    $fileName = [System.IO.Path]::GetFileName($Path)
    foreach ($double in @('.tar.gz', '.tar.bz2', '.tar.xz', '.tar.zst')) {
        if ($fileName.Length -gt $double.Length -and $fileName.EndsWith($double, [System.StringComparison]::OrdinalIgnoreCase)) {
            return $fileName.Substring(0, $fileName.Length - $double.Length)
        }
    }
    return [System.IO.Path]::GetFileNameWithoutExtension($Path)
}

# 이미 존재하면 "이름 (2)", "이름 (3)" ... 순서로 중복 없는 경로 반환
function Get-UniquePath([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) {
        return $Path
    }
    $directory = Split-Path -Parent $Path
    $baseName = [System.IO.Path]::GetFileNameWithoutExtension($Path)
    $extension = [System.IO.Path]::GetExtension($Path)
    for ($i = 2; ; $i++) {
        $candidate = Join-Path $directory ('{0} ({1}){2}' -f $baseName, $i, $extension)
        if (-not (Test-Path -LiteralPath $candidate)) {
            return $candidate
        }
    }
}

switch ($Action) {
    'extract' {
        if (-not (Test-Path -LiteralPath $Target -PathType Leaf)) {
            Show-Error "압축 파일을 찾을 수 없습니다.`n$Target"
        }
        $extension = [System.IO.Path]::GetExtension($Target).ToLowerInvariant()
        if ($extension -notin $ArchiveExtensions) {
            Show-Error "지원하지 않는 압축 파일입니다.`n$Target"
        }
        $destination = Join-Path (Split-Path -Parent $Target) (Get-ArchiveFolderName $Target)
        New-Item -ItemType Directory -Force -Path $destination | Out-Null

        if ($extension -eq '.zip') {
            try {
                Expand-Archive -LiteralPath $Target -DestinationPath $destination -Force
            }
            catch {
                Show-Error "ZIP 압축 풀기에 실패했습니다.`n$_"
            }
        }
        else {
            $sevenZip = Find-7Zip
            if (-not $sevenZip) {
                Show-Error "7-Zip이 설치되어 있지 않아 압축을 풀 수 없습니다.`n(7z/rar/tar 등 해제에는 7-Zip이 필요합니다)"
            }
            & $sevenZip x -y "-o$destination" $Target 2>&1 | Out-Null
            if ($LASTEXITCODE -ne 0) {
                Show-Error "7-Zip 압축 풀기에 실패했습니다. (오류 코드: $LASTEXITCODE)"
            }
        }
        Start-Process explorer.exe -ArgumentList ('"' + $destination + '"')
        exit 0
    }

    'zip' {
        if (-not (Test-Path -LiteralPath $Target)) {
            Show-Error "대상 경로를 찾을 수 없습니다.`n$Target"
        }
        $archivePath = Get-UniquePath ($Target + '.zip')
        try {
            Compress-Archive -LiteralPath $Target -DestinationPath $archivePath
        }
        catch {
            Show-Error "ZIP 생성에 실패했습니다.`n$_"
        }
        Start-Process explorer.exe -ArgumentList ('/select,"' + $archivePath + '"')
        exit 0
    }

    '7z' {
        if (-not (Test-Path -LiteralPath $Target)) {
            Show-Error "대상 경로를 찾을 수 없습니다.`n$Target"
        }
        $sevenZip = Find-7Zip
        if (-not $sevenZip) {
            Show-Error "7-Zip이 설치되어 있지 않아 7z 압축을 만들 수 없습니다.`nhttps://www.7-zip.org/"
        }
        $archivePath = Get-UniquePath ($Target + '.7z')
        & $sevenZip a -t7z -y $archivePath $Target 2>&1 | Out-Null
        if ($LASTEXITCODE -ne 0) {
            Show-Error "7Z 생성에 실패했습니다. (오류 코드: $LASTEXITCODE)"
        }
        Start-Process explorer.exe -ArgumentList ('/select,"' + $archivePath + '"')
        exit 0
    }
}
