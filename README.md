# Windows 影音下載工具（Tauri 2 MVP）

這是與既有 macOS/Python 程式隔離的 Windows 11 x64 未簽署 portable 測試版。它使用 Tauri 2、Rust 與固定版本的 yt-dlp／FFmpeg／FFprobe；缺少 sidecar 或 SHA-256 不符時會 fail closed。本階段不製作 NSIS/MSI、自動更新或正式程式碼簽章。YouTube／YouTube Music 的完整支援仍受 yt-dlp EJS 與受信任 JavaScript runtime 尚未納入並鎖定的限制，不能假設僅靠目前三個 sidecar 即完整可用。

## Portable 測試方式

1. 核對 ZIP 旁的 `.sha256`，再把整個資料夾解壓；不要直接在 ZIP 內執行，也不要只搬移主程式而遺漏相鄰的 `resources/`。
2. 執行 `windows-media-downloader.exe`。本測試版未簽署，Windows 可能顯示未知發行者；不要關閉 Defender 或放寬系統安全設定。
3. 先選平台並按「Probe（不下載）」；只對自己擁有、已獲授權或條款允許的內容按「開始下載」。預設完成檔位於使用者 Downloads 下的 `Windows Media Downloader`；可在沒有排隊或執行中工作時，透過 Rust 原生資料夾選擇器改用其他通過驗證的本機目錄。
4. 若 sidecar 狀態不是「已驗證」，不要自行替換 EXE 或修改 manifest；以套件內 `scripts/verify-package.ps1` 從解壓資料夾重新驗證。
5. 下方工作控制台以「即時工作」與「下載紀錄」分頁呈現；清單在固定高度的 viewport 內捲動，不會因工作筆數增加而無限撐高視窗。分頁可用 Tab 聚焦，再以左右方向鍵、Home 或 End 切換。

## 功能範圍

- 保留 YouTube、YouTube Music；小鴨影音只接受 `play.777tv.ai`；歐樂影院只接受 `olevod.com` 與 `www.olevod.com`。Facebook（條件式）只接受 `www.facebook.com` 的公開匿名 `/reel/<純數字ID>` 或 `/<頁面純數字ID>/videos/pcb.<貼文純數字ID>/<影片純數字ID>` 形態（亦接受其最小化 canonical page/video ID 形態）；PCB 是 path segment，Facebook 任一 query 一律拒絕，fragment 只會正規化移除，群組／私人／直播／watch／profile 路徑及 fbcdn URL 一律拒絕。Facebook 僅對本報告記載的兩個使用者授權公開 URL 條件式驗證，不宣稱整站或未來可用。2026-09-07 的 YouTube／YouTube Music 公開 URL 不下載 probe 均成功，但完整格式支援仍待將 yt-dlp 的 EJS 與受信任 JavaScript runtime 納入 manifest、來源與版本鎖定。
- MMOV（實驗性／條件式）只接受 `https://hk.mmov.io/vodplay/<數字作品ID>/<數字線路>-<數字集數>.html`；來源不得含 query、fragment、userinfo、顯式 port 或 percent encoding。Rust adapter 只以固定 native-TLS client 解析唯一 `videoSrc`，不執行 JavaScript、不接受使用者直接輸入 CDN URL；HLS egress 僅允許 `bfikuncdn.com:443` 與 `kkzycdn.com:65`，每個請求都做精確 effective-port、公開 DNS/peer 檢查。2026-09-07 一個使用者授權公開來源已完成條件式 adapter/probe/video smoke 驗證，不宣稱整站或未來支援。
- 僅 HTTPS；固定平台/hostname allowlist；啟動下載前再次 DNS 解析並拒絕 IP literal、userinfo、localhost、私有、loopback、link-local、multicast、保留位址。
- 小鴨影音／歐樂影院 adapter 只解析頁面內固定 JSON，不執行 JavaScript；要求 `encrypt`、`trysee`、`points` 均為數字 0，並驗證 HLS host、所有 URI host、公開 DNS/peer、三層上限、5,000 URI 上限、`#EXTM3U`、VOD `#EXT-X-ENDLIST`，遇到 key 或 live playlist 即拒絕。
- MMOV adapter 同樣只允許最多 3 層、5,000 個 URI、4096 字元 URI 的 HLS VOD；最多檢查 32 個 distinct manifest、aggregate response body 8 MiB，整段 HLS scan 60 秒 timeout，單一 response 1 MiB／20 秒 timeout。manifest 必須以 `#EXTM3U` 開頭，leaf 必須有 `#EXT-X-ENDLIST`，只允許 `METHOD=NONE` 且無 URI 的 `EXT-X-KEY`，任何加密、live-only tag、DASH、master/leaf 混合或非 allowlist URI 均 fail closed。
- 每批與全域未完成佇列最多 5 筆，預設最多 3 筆同時工作；支援 audio（MP3）與 video 模式。同一批或既有排隊／執行中的工作若具有相同平台、正規化 URL 與模式會原子拒絕重複；相同平台與 URL 但 audio/video 模式不同則是不同工作。已完成、失敗或取消的歷史工作不阻止使用者重新提交。
- probe 使用固定 `yt-dlp --ignore-config --no-plugin-dirs --no-js-runtimes --no-remote-components --no-playlist --simulate --skip-download --print` 最小 JSON metadata template，不建立輸出檔；明確 DRM、需登入、受限 availability、直播／排程／已結束直播 lifecycle、playlist marker 或未知非空狀態則拒絕，缺省 optional 欄位不誤判為 DRM。Facebook 的 probe 另要求 extractor 為 `facebook`；MMOV 的 probe 另要求 extractor=`generic` 且 protocol=`m3u8_native`，並且其 target 必須先由 Rust adapter/HLS scanner 驗證。每筆實際下載前都會重新執行相同 probe。
- 下載使用固定、已驗證的 resources sidecar 路徑，不接受任意 CLI 參數或輸出路徑；固定停用未驗證 plugin、JavaScript runtime 與 remote component。兩個直接 HLS 平台固定 `--concurrent-fragments 4`，全域最多 3 筆工作；使用 `.incomplete/<UUID>` 暫存，成功後才移入本批 Rust 驗證後的輸出根。
- 輸出目錄由 Rust 持有；前端 IPC payload 不含路徑。預設根為 `Downloads\Windows Media Downloader`，自訂根只保存於 app-local schema-versioned JSON，寫入前後會檢查一般檔案、大小、reparse point 與原子替換。選取及每批啟動前會拒絕 UNC、mapped drive、磁碟根、不可寫、Windows／Program Files／ProgramData、程式及 resources 目錄與其子目錄；自訂目錄消失時不會偷偷重建，工作實際開始前會再次驗證。這些檢查無法消除同一權限程序的極短 TOCTOU，詳見 `SECURITY.md`。
- 單筆/全部取消會以 Windows `taskkill.exe /PID /T /F`（絕對路徑、非 shell）終止程序樹，再回收 child handle。
- 工作狀態卡可個別移除或清除所有已結束卡片；這些操作只改本機 UI 狀態，不刪除媒體檔、不終止其他工作，也不清除下載紀錄。排隊中／執行中的工作必須先取消。
- 下載紀錄使用 bundled SQLite（`app_local_data_dir()/download-history.sqlite3`），只在終態以去敏平台、模式、來源、標題、檔名、固定錯誤類別與時間欄位落盤；不保存完整輸出路徑、Cookie、token 或 sidecar 輸出。最多保留 1,000 筆，UI 顯示最新 100 筆；累計工作耗時是每筆從接受排程至終態（含佇列等待）的加總，並行時不是牆鐘時間。資料庫損壞、鎖定或寫入失敗會顯示固定 warning、不中斷下載；「清除全部歷史」只清除 SQLite 紀錄，不刪媒體或狀態卡。

字幕、跨重啟續傳、自動更新、登入/Cookie、CAPTCHA、地區/付費牆/DRM/存取控制繞過均刻意不支援。小鴨影音、歐樂影院與 MMOV 均僅對報告記載的指定公開來源條件式驗證，不是整站或未來版本保證。MMOV smoke 的精確 cut 使用 `force-keyframes` 造成樣本轉碼，不能推論正式下載 codec/速度；66 分鐘及 20 分鐘完整效能閘門均為 `not-evaluated`。詳見 `LIVE_TEST_REPORT.md`。

## 建置

需要 Node.js 20+、Rust stable MSVC toolchain、Windows WebView2 Runtime。

```powershell
git clone https://github.com/taichi16/windows-media-downloader.git
cd windows-media-downloader
npm ci
./scripts/fetch-tools.ps1
npm run build
cd src-tauri
cargo fmt --all -- --check
cargo test --locked
cargo build --release --locked
```

開發 UI/Tauri：

```powershell
cd windows-media-downloader
npm run tauri dev
```

`npm run tauri -- build --no-bundle` 的可攜執行檔位於 `src-tauri/target/release/windows-media-downloader.exe`；同層的 `resources/` 是 Tauri resource 輸出目錄。建置產物不應提交至 git。

## sidecar 供應與校驗

`tools.lock.json` 固定 yt-dlp 官方 immutable release，以及 FFmpeg 官網指向的 BtbN Windows LGPL build；包含版本、release/tag/commit、大小、SHA-256、授權與簽署金鑰指紋。執行：

```powershell
./scripts/fetch-tools.ps1
```

腳本會下載或重用 cache、核對官方 checksum、拒絕 ZIP traversal、再次核對解壓後執行檔，然後生成下列 resources。執行檔與正式 manifest 不提交至 git，但會放入 portable 套件：

```text
target/release/
├─ windows-media-downloader.exe
└─ resources/                   # 與 exe 同層，非 exe 目錄內
   ├─ sidecar-manifest.json     # 由發行流程產生，非 example
   └─ bin/
      ├─ yt-dlp.exe             # 必要
      ├─ ffmpeg.exe             # 下載必要；MVP 不信任 PATH
      └─ ffprobe.exe            # 必要；完成後驗證媒體 duration
```

manifest 格式請參照 `src-tauri/resources/sidecar-manifest.json.example`。hash 必須是實際檔案的 64 字元 SHA-256；缺檔、placeholder、未知檔名或不符時 fail closed。來源、hash、簽章驗證與授權細節見 `THIRD_PARTY_NOTICES.md`。

SHA-256 manifest 不是執行期信任根：若攻擊者能同時替換 binary 與 manifest，仍可重算 hash。本測試版以鎖定來源與建置時驗證建立可信基線，但尚未 Authenticode 簽署。由於目前未封裝並驗證 yt-dlp EJS 與 JavaScript runtime，完整 YouTube/YouTube Music 發行仍是阻塞項目；程式不會從 PATH 偷載 runtime。

## Portable 打包與驗證

完成 `npm run tauri -- build --no-bundle` 後：

```powershell
./scripts/package-portable.ps1 -SkipBuild
./scripts/verify-package.ps1 -PackageDirectory ./dist-portable/Windows-Media-Downloader-0.1.0-portable-x64
./scripts/verify-archive.ps1 -ZipPath ./dist-portable/Windows-Media-Downloader-0.1.0-portable-x64.zip
```

驗證腳本會確認主程式是 Windows x64 PE、sidecar manifest 恰好三項、工具與授權檔大小/SHA-256 全部吻合，並回報主程式 Authenticode 狀態。ZIP 驗證另會拒絕重複、absolute 或 traversal entry，並串流核對內含 `SHA256SUMS.txt` 的每一項。

## 測試與 benchmark

Rust 測試涵蓋 URL/hostname、特殊 IP、userinfo、第五/第六筆、平台 adapter、HLS VOD/key/host/上限、固定 probe/下載參數、進度解析、路徑 traversal、sidecar hash。預設測試不連網；live adapter 測試必須明確設定各平台對應的環境變數（含 `WMD_LIVE_MMOV_URL`）並以 `--ignored` 執行。前端 `npm run build` 同時執行 TypeScript typecheck 與 Vite build。

可控本機 loopback HTTP fixture benchmark（不連外網、不冒充 live 站台）：

```powershell
node scripts/benchmark-fixture.mjs --fixture-mib 64 --repetitions 3 --out-dir benchmarks/out
```

它會啟動僅綁定 `127.0.0.1` 的 HTTP Range fixture server，以實際 HTTP response stream、hash 完整性與 concurrency 1/2/3/5 測量 fixture 分塊 1/2/4/8；每組至少三次取 median，輸出 JSON 與 Markdown。這個 server 不經應用程式 SSRF allowlist，分塊數只代表本機 Range 請求，不代表 yt-dlp 網路 fragment。未提供外部 baseline 時 `gate=not-evaluated`；只有明確傳入 `--baseline-minutes N` 才會在 N > 20 時標記 `gate=fail`，否則標記 `pass`。`--fixture-mib`（1..512）與 `--repetitions`（3..20）必須是有限安全整數且有上限。fixture 數據不能推論 yt-dlp、ffmpeg、YouTube、小鴨影音、歐樂影院或 MMOV live 表現；MMOV 尚無 20 分鐘或 66 分鐘外部完整基準，兩者均維持 `not-evaluated`。

安全細節、redirect/DNS rebinding 限制與威脅模型請見 `SECURITY.md`、`THREAT_MODEL.md`；平台狀態請見 `PLATFORM_CAPABILITIES.md`。
