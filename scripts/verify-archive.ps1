[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$ZipPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$resolvedZip = (Resolve-Path -LiteralPath $ZipPath).Path
$expectedRoot = [IO.Path]::GetFileNameWithoutExtension($resolvedZip)
Add-Type -AssemblyName System.IO.Compression.FileSystem
$archive = [IO.Compression.ZipFile]::OpenRead($resolvedZip)
try {
    $names = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    foreach ($entry in $archive.Entries) {
        $name = $entry.FullName.Replace('\', '/')
        if (-not $names.Add($name) -or
            $name.StartsWith('/') -or
            [IO.Path]::IsPathRooted($name) -or
            ($name.Split('/') -contains '..') -or
            -not $name.StartsWith("$expectedRoot/", [StringComparison]::Ordinal)) {
            throw "ZIP 含重複或不安全路徑：$name"
        }
    }

    $sumEntry = $archive.GetEntry("$expectedRoot/SHA256SUMS.txt")
    if ($null -eq $sumEntry) {
        throw 'ZIP 缺少 SHA256SUMS.txt'
    }
    $reader = [IO.StreamReader]::new($sumEntry.Open(), [Text.UTF8Encoding]::new($false), $true)
    try {
        $sumText = $reader.ReadToEnd()
    }
    finally {
        $reader.Dispose()
    }

    $expected = @{}
    foreach ($line in $sumText -split "\r?\n") {
        if ([string]::IsNullOrWhiteSpace($line)) { continue }
        if ($line -notmatch '^([0-9a-f]{64})  (.+)$') {
            throw 'SHA256SUMS.txt 格式錯誤'
        }
        if ($expected.ContainsKey($Matches[2])) {
            throw "SHA256SUMS.txt 含重複路徑：$($Matches[2])"
        }
        $expected[$Matches[2]] = $Matches[1]
    }

    $verified = 0
    foreach ($entry in $archive.Entries) {
        $name = $entry.FullName.Replace('\', '/')
        if ($name.EndsWith('/') -or $name -eq "$expectedRoot/SHA256SUMS.txt") { continue }
        $relative = $name.Substring($expectedRoot.Length + 1)
        if (-not $expected.ContainsKey($relative)) {
            throw "ZIP 檔案未列於 SHA256SUMS.txt：$relative"
        }
        $sha = [Security.Cryptography.SHA256]::Create()
        $stream = $entry.Open()
        try {
            $actual = [Convert]::ToHexString($sha.ComputeHash($stream)).ToLowerInvariant()
        }
        finally {
            $stream.Dispose()
            $sha.Dispose()
        }
        if ($actual -ne $expected[$relative]) {
            throw "ZIP entry SHA-256 不符：$relative"
        }
        $verified++
    }
    if ($verified -ne $expected.Count) {
        throw "ZIP 驗證數量不符：預期 $($expected.Count)，實際 $verified"
    }
}
finally {
    $archive.Dispose()
}

$zipHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $resolvedZip).Hash.ToLowerInvariant()
Write-Output "ZIP 安全路徑與 $verified 個 entry SHA-256 全部通過。"
Write-Output "ZIP SHA-256：$zipHash"

