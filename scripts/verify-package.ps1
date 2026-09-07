[CmdletBinding()]
param(
    [string]$PackageDirectory = (Join-Path $PSScriptRoot '..\src-tauri\target\release')
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$projectRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$packageRoot = [IO.Path]::GetFullPath($PackageDirectory)
$lockPath = Join-Path $projectRoot 'tools.lock.json'
$lock = Get-Content -LiteralPath $lockPath -Raw | ConvertFrom-Json

function Get-Sha256 {
    param([Parameter(Mandatory = $true)][string]$Path)

    (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLowerInvariant()
}

function Assert-LockedFile {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][long]$Size,
        [Parameter(Mandatory = $true)][string]$Sha256
    )

    $item = Get-Item -LiteralPath $Path -ErrorAction Stop
    if (-not $item.PSIsContainer -and $item.Length -eq $Size -and (Get-Sha256 $Path) -eq $Sha256.ToLowerInvariant()) {
        return
    }
    throw "套件檔案驗證失敗：$Path"
}

$appPath = Join-Path $packageRoot 'windows-media-downloader.exe'
$appBytes = [IO.File]::ReadAllBytes($appPath)
if ($appBytes.Length -lt 256 -or $appBytes[0] -ne 0x4d -or $appBytes[1] -ne 0x5a) {
    throw '主程式不是有效的 PE/MZ 執行檔'
}
$peOffset = [BitConverter]::ToInt32($appBytes, 0x3c)
if ($peOffset -lt 0 -or $peOffset + 6 -gt $appBytes.Length -or
    $appBytes[$peOffset] -ne 0x50 -or $appBytes[$peOffset + 1] -ne 0x45 -or
    [BitConverter]::ToUInt16($appBytes, $peOffset + 4) -ne 0x8664) {
    throw '主程式不是預期的 Windows x64 PE'
}

$resourceRoot = Join-Path $packageRoot 'resources'
$manifestPath = Join-Path $resourceRoot 'sidecar-manifest.json'
$manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
if ($manifest.schemaVersion -ne 1) {
    throw 'sidecar manifest schemaVersion 不受支援'
}

$expectedBins = @('yt-dlp.exe', 'ffmpeg.exe', 'ffprobe.exe')
$binDirectory = Join-Path $resourceRoot 'bin'
$binDirectories = @(Get-ChildItem -LiteralPath $binDirectory -Directory)
$actualBins = @(Get-ChildItem -LiteralPath $binDirectory -File | Where-Object Name -ne '.gitkeep' | Select-Object -ExpandProperty Name)
$unexpectedBins = @($actualBins | Where-Object { $_ -notin $expectedBins })
if ($binDirectories.Count -ne 0 -or $actualBins.Count -ne $expectedBins.Count -or $unexpectedBins.Count -ne 0) {
    throw 'resources/bin 必須恰好包含三支鎖定 sidecar'
}
$manifestNames = @($manifest.files.PSObject.Properties.Name)
$unexpectedManifestNames = @($manifestNames | Where-Object { $_ -notin $expectedBins })
if ($manifestNames.Count -ne $expectedBins.Count -or $unexpectedManifestNames.Count -ne 0) {
    throw 'sidecar manifest 必須恰好宣告三支鎖定 sidecar'
}
foreach ($name in $expectedBins) {
    $locked = $lock.portableFiles.PSObject.Properties[$name].Value
    $manifestHash = $manifest.files.PSObject.Properties[$name].Value
    if ($manifestHash -ne $locked.sha256) {
        throw "manifest 與 tools.lock.json 的 $name SHA-256 不一致"
    }
    Assert-LockedFile -Path (Join-Path $binDirectory $name) -Size $locked.size -Sha256 $locked.sha256
}

$licenseDirectory = Join-Path $resourceRoot 'licenses'
$expectedLicenses = @($lock.licenseFiles.PSObject.Properties.Name)
$actualLicenses = @(Get-ChildItem -LiteralPath $licenseDirectory -File | Select-Object -ExpandProperty Name)
$unexpectedLicenses = @($actualLicenses | Where-Object { $_ -notin $expectedLicenses })
if ($actualLicenses.Count -ne $expectedLicenses.Count -or $unexpectedLicenses.Count -ne 0) {
    throw 'resources/licenses 與 tools.lock.json 不一致'
}
foreach ($name in $expectedLicenses) {
    $locked = $lock.licenseFiles.PSObject.Properties[$name].Value
    Assert-LockedFile -Path (Join-Path $licenseDirectory $name) -Size $locked.size -Sha256 $locked.sha256
}

$signature = Get-AuthenticodeSignature -LiteralPath $appPath
Write-Output 'Portable 套件結構、Windows x64 PE、sidecar manifest、工具與授權檔均驗證通過。'
Write-Output "主程式 SHA-256：$(Get-Sha256 $appPath)"
Write-Output "主程式 Authenticode：$($signature.Status)"
