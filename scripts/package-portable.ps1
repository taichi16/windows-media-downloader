[CmdletBinding()]
param(
    [string]$Version = '0.1.0',
    [switch]$SkipBuild
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

if ($Version -notmatch '^\d+\.\d+\.\d+([-.][0-9A-Za-z.-]+)?$') {
    throw 'Version 格式不安全'
}

$projectRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$releaseRoot = Join-Path $projectRoot 'src-tauri\target\release'
$distRoot = [IO.Path]::GetFullPath((Join-Path $projectRoot 'dist-portable'))
$folderName = "Windows-Media-Downloader-$Version-portable-x64"
$stage = [IO.Path]::GetFullPath((Join-Path $distRoot $folderName))
$zipPath = [IO.Path]::GetFullPath((Join-Path $distRoot "$folderName.zip"))
$expectedStagePrefix = $distRoot.TrimEnd('\') + '\'
if (-not $stage.StartsWith($expectedStagePrefix, [StringComparison]::OrdinalIgnoreCase) -or
    -not $zipPath.StartsWith($expectedStagePrefix, [StringComparison]::OrdinalIgnoreCase)) {
    throw '拒絕使用專案 dist-portable 以外的 staging 路徑'
}

if (-not $SkipBuild) {
    & (Join-Path $PSScriptRoot 'fetch-tools.ps1')
    Push-Location $projectRoot
    try {
        & npm.cmd run build
        if ($LASTEXITCODE -ne 0) { throw '前端建置失敗' }
        & npm.cmd run tauri -- build --no-bundle
        if ($LASTEXITCODE -ne 0) { throw 'Tauri portable 建置失敗' }
    }
    finally {
        Pop-Location
    }
}

New-Item -ItemType Directory -Path $distRoot -Force | Out-Null
if (Test-Path -LiteralPath $stage) {
    Remove-Item -LiteralPath $stage -Recurse -Force
}
if (Test-Path -LiteralPath $zipPath) {
    Remove-Item -LiteralPath $zipPath -Force
}

$stageBin = Join-Path $stage 'resources\bin'
$stageLicenses = Join-Path $stage 'resources\licenses'
$stageScripts = Join-Path $stage 'scripts'
New-Item -ItemType Directory -Path $stageBin, $stageLicenses, $stageScripts -Force | Out-Null
Copy-Item -LiteralPath (Join-Path $releaseRoot 'windows-media-downloader.exe') -Destination $stage
Copy-Item -LiteralPath (Join-Path $releaseRoot 'resources\sidecar-manifest.json') -Destination (Join-Path $stage 'resources')
foreach ($name in @('yt-dlp.exe', 'ffmpeg.exe', 'ffprobe.exe')) {
    Copy-Item -LiteralPath (Join-Path $releaseRoot "resources\bin\$name") -Destination $stageBin
}
Get-ChildItem -LiteralPath (Join-Path $releaseRoot 'resources\licenses') -File |
    ForEach-Object { Copy-Item -LiteralPath $_.FullName -Destination $stageLicenses }
foreach ($name in @('README.md', 'SECURITY.md', 'THREAT_MODEL.md', 'PERFORMANCE.md', 'PLATFORM_CAPABILITIES.md', 'LIVE_TEST_REPORT.md', 'THIRD_PARTY_NOTICES.md', 'tools.lock.json')) {
    Copy-Item -LiteralPath (Join-Path $projectRoot $name) -Destination $stage
}
foreach ($name in @('verify-package.ps1', 'verify-archive.ps1')) {
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot $name) -Destination $stageScripts
}

& (Join-Path $PSScriptRoot 'verify-package.ps1') -PackageDirectory $stage

$sumLines = Get-ChildItem -LiteralPath $stage -File -Recurse |
    Sort-Object FullName |
    ForEach-Object {
        $relative = [IO.Path]::GetRelativePath($stage, $_.FullName).Replace('\', '/')
        "$((Get-FileHash -Algorithm SHA256 -LiteralPath $_.FullName).Hash.ToLowerInvariant())  $relative"
    }
$utf8NoBom = [Text.UTF8Encoding]::new($false)
[IO.File]::WriteAllLines((Join-Path $stage 'SHA256SUMS.txt'), $sumLines, $utf8NoBom)

Compress-Archive -LiteralPath $stage -DestinationPath $zipPath -CompressionLevel Optimal
$zipHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $zipPath).Hash.ToLowerInvariant()
[IO.File]::WriteAllText("$zipPath.sha256", "$zipHash  $([IO.Path]::GetFileName($zipPath))`r`n", $utf8NoBom)

& (Join-Path $PSScriptRoot 'verify-archive.ps1') -ZipPath $zipPath

Write-Output "Portable：$stage"
Write-Output "ZIP：$zipPath"
Write-Output "ZIP SHA-256：$zipHash"
