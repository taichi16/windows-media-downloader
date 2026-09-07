# 第三方工具來源與授權紀錄

本文件記錄 portable 測試版所攜帶的外部執行檔。精確、機器可驗證的來源、大小與 SHA-256 以 [`tools.lock.json`](tools.lock.json) 為準；`scripts/fetch-tools.ps1` 只會接受鎖定值完全相符的檔案。

## SQLite / rusqlite

- Rust crate：`rusqlite 0.37.0`，啟用 `bundled` feature，連同 SQLite amalgamation 靜態編譯於 Windows 應用程式。
- 上游專案：<https://github.com/rusqlite/rusqlite>；SQLite：<https://sqlite.org/>。
- rusqlite 以 MIT 授權；SQLite 公開領域／授權聲明依上游發行內容為準。正式散布前應將對應 crate lockfile、NOTICE 與 SQLite 授權聲明納入 SBOM/法律複核。
- SQLite 僅用於本機下載歷史，不是 sidecar，也不接收前端 SQL；DB 固定在 app-local data 並採 schema version 1。

## yt-dlp

- 專案與發行來源：<https://github.com/yt-dlp/yt-dlp>
- 固定版本：`2026.08.19`（GitHub immutable release）
- portable 檔案：`yt-dlp.exe`
- SHA-256：`66674953fe251b89f4d08c5f0e35e0728679bd67ab3d7d05c0562af101dd3e7a`
- 檔案大小：17,840,399 bytes
- 授權：官方 PyInstaller Windows executable 為 GPL-3.0-or-later，並包含第三方元件；portable 隨附 `yt-dlp-LICENSE.txt`、`yt-dlp-THIRD_PARTY_LICENSES.txt` 與 `GPL-3.0.txt`。

2026-09-07 的更新驗證同時核對官方 `SHA2-256SUMS`。其 detached signature 以固定 release tag 內的 `public.key` 驗證成功，完整簽署金鑰指紋為：

```text
AC0C BBE6 848D 6A87 3464  AF4E 57CF 6593 3B5A 7581
```

鎖檔另固定 checksum、signature 與 public key 本身的 SHA-256。更新版本時不得只改 `yt-dlp.exe` 的 hash；必須重新驗證 checksum 簽章與完整指紋。

## FFmpeg / FFprobe

- 上游專案：<https://ffmpeg.org/>
- FFmpeg 官方 Windows 下載頁指向的 binary provider：<https://github.com/BtbN/FFmpeg-Builds>
- 固定 provider tag：`autobuild-2026-09-06-13-06`
- 固定 provider commit：`8267213e26c1031621e6e1210fe3aa4867214f6a`
- 固定版本：`n9.0.1-26-g5c8e7e2433-20260906`
- archive：`ffmpeg-n9.0.1-26-g5c8e7e2433-win64-lgpl-9.0.zip`
- archive SHA-256：`4700c0bcb523466fdf5e36e22ad4ff3fadf33f203e2dbfdc78f5b4cd068b8818`
- `ffmpeg.exe` SHA-256：`48a2a8da07b627335489956064a9b51e081f0d677cfeff154dd89b5af2dcaeff`
- `ffprobe.exe` SHA-256：`aeddf918ef0542997c7db606c23bc516881e43409c39de436516a22d00d588ed`
- 授權：選用 `win64-lgpl-9.0` 靜態 build；執行檔自述組態停用 `libx264`、`libx265` 等 GPL-only library。portable 隨附 archive 內的 `FFmpeg-LICENSE.txt` 與 `BtbN-FFmpeg-Builds-LICENSE.txt`。

2026-09-07 的更新驗證同時核對該固定 release 的 `checksums.sha256`，並在解壓前拒絕 rooted path 與 `..` traversal entry。

## 發行限制

此成果是未簽署的本機 portable 測試版，不代表已完成公開散布所需的法律審查。公開發行前仍須：

1. 依固定版本準備所有適用元件的對應原始碼／建置資訊及授權告知，特別是靜態 LGPL build 的重新連結義務。
2. 產生 SBOM 並由發行負責人完成第三方授權複核。
3. 對應用程式、sidecar 與最終套件執行 Authenticode 程式碼簽署與簽章驗證。
