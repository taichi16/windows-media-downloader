[CmdletBinding()]
param(
    [string]$CacheDirectory = (Join-Path $PSScriptRoot '..\.tool-cache')
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$projectRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$lockPath = Join-Path $projectRoot 'tools.lock.json'
$resourceRoot = Join-Path $projectRoot 'src-tauri\resources'
$binDirectory = Join-Path $resourceRoot 'bin'
$licenseDirectory = Join-Path $resourceRoot 'licenses'
$cacheRoot = [IO.Path]::GetFullPath($CacheDirectory)

if (-not (Test-Path -LiteralPath $lockPath -PathType Leaf)) {
    throw "找不到工具鎖定檔：$lockPath"
}

$lock = Get-Content -LiteralPath $lockPath -Raw | ConvertFrom-Json
if ($lock.schemaVersion -ne 1 -or $lock.target -ne 'windows-x86_64') {
    throw 'tools.lock.json schema 或 target 不受支援'
}

New-Item -ItemType Directory -Path $cacheRoot, $binDirectory, $licenseDirectory -Force | Out-Null

function Assert-HttpsUrl {
    param([Parameter(Mandatory = $true)][string]$Url)

    $uri = $null
    if (-not [Uri]::TryCreate($Url, [UriKind]::Absolute, [ref]$uri) -or
        $uri.Scheme -ne 'https' -or
        [string]::IsNullOrWhiteSpace($uri.Host) -or
        -not [string]::IsNullOrEmpty($uri.UserInfo)) {
        throw "只允許無 userinfo 的絕對 HTTPS 來源：$Url"
    }
}

function Get-Sha256 {
    param([Parameter(Mandatory = $true)][string]$Path)

    return (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLowerInvariant()
}

function Assert-File {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][long]$Size,
        [Parameter(Mandatory = $true)][string]$Sha256
    )

    $item = Get-Item -LiteralPath $Path -ErrorAction Stop
    if ($item.Length -ne $Size) {
        throw "檔案大小不符：$($item.Name)；預期 $Size，實際 $($item.Length)"
    }
    $actual = Get-Sha256 -Path $Path
    if ($actual -ne $Sha256.ToLowerInvariant()) {
        throw "SHA-256 不符：$($item.Name)"
    }
}

function Get-LockedAsset {
    param(
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][string]$Url,
        [Parameter(Mandatory = $true)][long]$Size,
        [Parameter(Mandatory = $true)][string]$Sha256
    )

    Assert-HttpsUrl -Url $Url
    $destination = Join-Path $cacheRoot $Name
    if (Test-Path -LiteralPath $destination -PathType Leaf) {
        Assert-File -Path $destination -Size $Size -Sha256 $Sha256
        return $destination
    }

    $partial = "$destination.partial"
    if (Test-Path -LiteralPath $partial) {
        Remove-Item -LiteralPath $partial -Force
    }
    try {
        Invoke-WebRequest -UseBasicParsing -Uri $Url -OutFile $partial -MaximumRedirection 5
        Assert-File -Path $partial -Size $Size -Sha256 $Sha256
        Move-Item -LiteralPath $partial -Destination $destination
    }
    finally {
        if (Test-Path -LiteralPath $partial) {
            Remove-Item -LiteralPath $partial -Force
        }
    }
    return $destination
}

function Copy-VerifiedFile {
    param(
        [Parameter(Mandatory = $true)][string]$Source,
        [Parameter(Mandatory = $true)][string]$Destination,
        [Parameter(Mandatory = $true)][long]$Size,
        [Parameter(Mandatory = $true)][string]$Sha256
    )

    Assert-File -Path $Source -Size $Size -Sha256 $Sha256
    $partial = "$Destination.partial"
    if (Test-Path -LiteralPath $partial) {
        Remove-Item -LiteralPath $partial -Force
    }
    try {
        Copy-Item -LiteralPath $Source -Destination $partial
        Assert-File -Path $partial -Size $Size -Sha256 $Sha256
        Move-Item -LiteralPath $partial -Destination $Destination -Force
    }
    finally {
        if (Test-Path -LiteralPath $partial) {
            Remove-Item -LiteralPath $partial -Force
        }
    }
}

$ytAsset = Get-LockedAsset -Name $lock.ytDlp.asset.name -Url $lock.ytDlp.asset.url `
    -Size $lock.ytDlp.asset.size -Sha256 $lock.ytDlp.asset.sha256
$ytSums = Get-LockedAsset -Name 'yt-dlp-SHA2-256SUMS' -Url $lock.ytDlp.checksumFile.url `
    -Size $lock.ytDlp.checksumFile.size -Sha256 $lock.ytDlp.checksumFile.sha256
[void](Get-LockedAsset -Name 'yt-dlp-SHA2-256SUMS.sig' -Url $lock.ytDlp.checksumSignature.url `
    -Size $lock.ytDlp.checksumSignature.size -Sha256 $lock.ytDlp.checksumSignature.sha256)
[void](Get-LockedAsset -Name 'yt-dlp-public.key' -Url $lock.ytDlp.signingKey.url `
    -Size $lock.ytDlp.signingKey.size -Sha256 $lock.ytDlp.signingKey.sha256)

$escapedYtName = [regex]::Escape($lock.ytDlp.asset.name)
$ytChecksumLine = Select-String -LiteralPath $ytSums -Pattern "^$($lock.ytDlp.asset.sha256)\s+\*?$escapedYtName$"
if (-not $ytChecksumLine) {
    throw '官方 yt-dlp SHA2-256SUMS 未包含鎖定的 yt-dlp.exe 雜湊'
}

$ffmpegArchive = Get-LockedAsset -Name $lock.ffmpeg.asset.name -Url $lock.ffmpeg.asset.url `
    -Size $lock.ffmpeg.asset.size -Sha256 $lock.ffmpeg.asset.sha256
$ffmpegSums = Get-LockedAsset -Name 'ffmpeg-checksums.sha256' -Url $lock.ffmpeg.checksumFile.url `
    -Size $lock.ffmpeg.checksumFile.size -Sha256 $lock.ffmpeg.checksumFile.sha256
$escapedArchiveName = [regex]::Escape($lock.ffmpeg.asset.name)
$ffmpegChecksumLine = Select-String -LiteralPath $ffmpegSums -Pattern "^$($lock.ffmpeg.asset.sha256)\s+\*?$escapedArchiveName$"
if (-not $ffmpegChecksumLine) {
    throw 'BtbN checksums.sha256 未包含鎖定的 FFmpeg archive 雜湊'
}

Add-Type -AssemblyName System.IO.Compression.FileSystem
$archive = [IO.Compression.ZipFile]::OpenRead($ffmpegArchive)
try {
    foreach ($entry in $archive.Entries) {
        $normalized = $entry.FullName.Replace('/', '\')
        if ([IO.Path]::IsPathRooted($normalized) -or ($normalized.Split('\') -contains '..')) {
            throw "FFmpeg archive 含不安全路徑：$($entry.FullName)"
        }
    }
}
finally {
    $archive.Dispose()
}

$stage = Join-Path $cacheRoot ('.extract-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $stage | Out-Null
try {
    [IO.Compression.ZipFile]::ExtractToDirectory($ffmpegArchive, $stage)
    $archiveRootName = [IO.Path]::GetFileNameWithoutExtension($lock.ffmpeg.asset.name)
    $archiveRoot = Join-Path $stage $archiveRootName
    $ffmpegSource = Join-Path $archiveRoot 'bin\ffmpeg.exe'
    $ffprobeSource = Join-Path $archiveRoot 'bin\ffprobe.exe'
    $ffmpegLicenseSource = Join-Path $archiveRoot $lock.licenseFiles.'FFmpeg-LICENSE.txt'.archivePath

    Copy-VerifiedFile -Source $ytAsset -Destination (Join-Path $binDirectory 'yt-dlp.exe') `
        -Size $lock.portableFiles.'yt-dlp.exe'.size -Sha256 $lock.portableFiles.'yt-dlp.exe'.sha256
    Copy-VerifiedFile -Source $ffmpegSource -Destination (Join-Path $binDirectory 'ffmpeg.exe') `
        -Size $lock.portableFiles.'ffmpeg.exe'.size -Sha256 $lock.portableFiles.'ffmpeg.exe'.sha256
    Copy-VerifiedFile -Source $ffprobeSource -Destination (Join-Path $binDirectory 'ffprobe.exe') `
        -Size $lock.portableFiles.'ffprobe.exe'.size -Sha256 $lock.portableFiles.'ffprobe.exe'.sha256
    Copy-VerifiedFile -Source $ffmpegLicenseSource -Destination (Join-Path $licenseDirectory 'FFmpeg-LICENSE.txt') `
        -Size $lock.licenseFiles.'FFmpeg-LICENSE.txt'.size -Sha256 $lock.licenseFiles.'FFmpeg-LICENSE.txt'.sha256
}
finally {
    $expectedStagePrefix = $cacheRoot.TrimEnd('\') + '\.extract-'
    if (-not $stage.StartsWith($expectedStagePrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "拒絕清理非預期 staging 路徑：$stage"
    }
    if (Test-Path -LiteralPath $stage) {
        Remove-Item -LiteralPath $stage -Recurse -Force
    }
}

foreach ($property in $lock.licenseFiles.PSObject.Properties) {
    if (-not $property.Value.PSObject.Properties['url']) {
        continue
    }
    $licenseAsset = Get-LockedAsset -Name $property.Name -Url $property.Value.url `
        -Size $property.Value.size -Sha256 $property.Value.sha256
    Copy-VerifiedFile -Source $licenseAsset -Destination (Join-Path $licenseDirectory $property.Name) `
        -Size $property.Value.size -Sha256 $property.Value.sha256
}

$manifest = [ordered]@{
    schemaVersion = 1
    files = [ordered]@{
        'yt-dlp.exe' = $lock.portableFiles.'yt-dlp.exe'.sha256
        'ffmpeg.exe' = $lock.portableFiles.'ffmpeg.exe'.sha256
        'ffprobe.exe' = $lock.portableFiles.'ffprobe.exe'.sha256
    }
}
$manifestJson = ($manifest | ConvertTo-Json -Depth 4) + [Environment]::NewLine
$utf8NoBom = [Text.UTF8Encoding]::new($false)
[IO.File]::WriteAllText((Join-Path $resourceRoot 'sidecar-manifest.json'), $manifestJson, $utf8NoBom)

Write-Output '可信 sidecar 已驗證並放入 src-tauri/resources/bin。'
Write-Output "yt-dlp $($lock.ytDlp.version)"
Write-Output "FFmpeg $($lock.ffmpeg.version)"
