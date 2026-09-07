# Windows 版安全設計與已知限制

## 已實作控制

1. URL 必須是 HTTPS、無 userinfo、無顯式 port、無 IP literal，且 hostname 必須完全符合平台 allowlist；Olevod 僅接受 `olevod.com` 與 `www.olevod.com`，Facebook 僅接受 `www.facebook.com`。
2. 啟動下載與 probe 前，以系統 DNS 解析 allowlisted hostname；所有結果只要有私有、loopback、link-local、multicast、unspecified、CGNAT、文件/保留或 benchmark 位址即拒絕。
3. yt-dlp 使用 app resource 中固定 `yt-dlp.exe` 絕對路徑；固定加入 `--ignore-config`、`--no-plugin-dirs`、`--no-js-runtimes`、`--no-remote-components`、`--no-playlist`，並以 `--` 分隔 URL。前端不能提供任意參數、程序或輸出路徑。Windows standalone 固定使用官方 `--compat-options no-certifi` 改採 Windows 系統信任鏈，並未關閉憑證驗證；禁止 `--no-check-certificates`。
4. sidecar 必須由 manifest SHA-256 驗證；`yt-dlp.exe`、`ffmpeg.exe`、`ffprobe.exe` 三者皆必須存在且宣告，`bin/` 中除三檔（及 `.gitkeep`）外的任何檔案、目錄或 reparse point 會拒絕；example manifest placeholder 永遠不能通過。`tools.lock.json` 固定官方／官方指向來源、release/tag/commit、大小、hash、checksum、簽署金鑰指紋與授權。SHA-256 本身不是執行期信任根，未簽署測試版仍須防止 portable 目錄被本機攻擊者同時竄改 binary 與 manifest。
5. 輸出根由 Rust 唯一持有，預設為使用者 Downloads 下的 `Windows Media Downloader`；使用者只能透過 Rust 呼叫的 native folder picker 選取自訂本機目錄，前端不能傳入路徑。自訂設定只存 app-local schema-versioned JSON，設定檔先以 `symlink_metadata`、一般檔案、16 KiB 上限與 reparse 檢查，再以暫存檔 flush/sync 和 Windows write-through replace 落盤。選擇及每批啟動時拒絕 UNC／verbatim UNC、mapped drive、磁碟根、非目錄、不可寫、reparse/symlink/junction、Windows／Program Files／ProgramData、程式與 resources 目錄及其子目錄；自訂根消失時不會重建。暫存使用 UUID 子目錄，完成檔副檔名為固定 allowlist，且只把單一非 symlink 媒體檔原子 rename 至輸出根。
6. stdout/stderr 持續排水但各自最多保留 64 KiB；錯誤顯示最多 2,000 字元。進度值只接受百分比、速度與 ETA 的解析結果。
7. 不使用 shell、localhost HTTP server、Cookie 匯入或 `--no-check-certificates`；不處理登入、CAPTCHA、付費牆、DRM 或存取控制繞過。
8. 小鴨影音與歐樂影院頁面由 Rust HTTPS client 解析，禁止 redirect/proxy，使用 native TLS、20 秒 timeout、1 MiB response 上限及固定誠實識別 `WindowsMediaDownloader/0.1`。yt-dlp 對這兩站亦使用相同固定 User-Agent；不使用 `--impersonate`。
9. 兩站 HLS 只接受固定媒體 host；驗證所有 manifest/segment URI 的 host 與公開 DNS、最多 3 層／5,000 URI，要求 `#EXTM3U` 與 leaf `#EXT-X-ENDLIST`，遇到 `EXT-X-KEY`／`SESSION-KEY` 即拒絕。
10. 變更輸出目錄時，後端在開啟 picker 前、picker 返回後及設定鎖內再次確認沒有 queued/running 工作；批次以同一個已驗證 root snapshot 執行，取得 concurrency permit 後建立 job directory 前及 finalize 前再檢查 root、`.incomplete` 與 job directory。這些重複檢查只縮小同一權限程序的 TOCTOU，不能提供 kernel sandbox 或 ACL 級保證。
11. 工作狀態卡的移除命令只接受 completed/failed/cancelled，且與狀態檢查在同一 jobs mutex 內完成；清除已結束卡片不刪媒體、不清歷史。終態下載紀錄以 bundled SQLite 存於 app-local data，DB 邏輯由 mutex 序列化，最多保留 1,000 筆、UI 顯示 100 筆；只保存去敏來源、檔名、標題、固定錯誤類別與時間，不保存完整路徑、Cookie、token 或 sidecar 輸出。DB 初始化、鎖定、損壞或讀寫失敗只產生固定去敏 warning，不改寫下載成功／失敗結果。
12. Facebook 只接受 `/reel/<純數字ID>`、`/<頁面純數字ID>/videos/<影片純數字ID>` 或最小必要的 `/頁面ID/videos/pcb.<貼文ID>/<影片ID>` 來源形態；PCB 是 path segment，ID 有長度與純數字限制，任何 query（包含 tracking、簽章與 token）均拒絕；fragment 會正規化丟棄，userinfo、port、groups/private/live/watch/profile 與 fbcdn host 均拒絕。Facebook 不走小鴨影音／歐樂影院 HLS adapter，也不接受前端提供 target。

Probe 的 `availability` 欄位可依 extractor 缺省（null）；若有值則只接受 `public`／`unlisted`，其他非空狀態（包含 `private`、`premium_only`、`subscriber_only`、`needs_auth`）一律拒絕。`has_drm` 取格式層明確標記；只要出現 `true` 或 `maybe` 即拒絕，缺省欄位不被誤判為 DRM。另讀取 `is_live`、`live_status`、`playlist_id` 與 `playlist_count`；直播、排程、已結束直播、非 `not_live` lifecycle 或 playlist marker 一律拒絕。Facebook probe 必須回報 `facebook` extractor，否則拒絕；這些 probe 閘門會在每筆實際下載前重新執行。

## 本機歷史資料

SQLite 檔案固定在 Tauri `app_local_data_dir()/download-history.sqlite3`，schema 透過 transaction 由 `PRAGMA user_version=0` 遷移至 1；遇到較新版本不會刪除或覆寫。來源先通過既有 URL 正規化；一般平台移除全部 query 與 fragment，只保留允許的 scheme/host/必要安全 path。YouTube／YouTube Music 的 watch URL 僅保留安全 `v`，`youtu.be` 僅保留第一個安全短網址 ID；Facebook 僅保留 `/reel/<ID>` 或 `/頁面ID/videos/<影片ID>`，query（包含 fbcdn 簽章與 token）一律在來源驗證階段拒絕，歷史不保存。標題與檔名以 UTF-8 字元數截長，錯誤只映射到固定 enum-like code 與固定摘要。歷史不是 resume store，也不是 log。

歷史寫入採 best-effort：先完成 `DownloadStatus` 終態更新，再以已擷取的接受排程時間／probe title context 寫入；工作狀態卡即使隨後被移除，仍不會回刪該筆歷史。`elapsed_ms` 包含佇列等待，累計值是保留集合的逐工作總和，並行時不等於牆鐘時間。UI 的「清除全部歷史」只刪資料表，不刪媒體檔或狀態卡；此操作與「清除已結束」完全分離。

## Redirect、DNS rebinding 與程序樹限制

平台 adapter 自己的 HTTP client 禁止 redirect，並對實際 peer 與所有 HLS URI host 做檢查；但目前沒有獨立 HTTP proxy 可攔截 yt-dlp/FFmpeg 下載 fragment 時的每一次 redirect，也沒有把所有後續 DNS 查詢鎖在啟動前結果的 OS 網路沙箱。因此仍不能宣稱完整防止 fragment redirect／DNS rebinding。若要提升保證，需在後續版本加入可驗證的 downloader network boundary，並重新進行威脅建模。

取消時先由可信 Windows `GetSystemDirectoryW` 取得 System32 目錄，再以絕對路徑呼叫其中的 `taskkill.exe` 與 `/T` 遞迴選項，另呼叫 Tokio child 的 kill/reap。從 child 啟動到取消之間仍存在極短競態；taskkill 只針對本程式保存的 child PID，不接受前端 PID，失敗時工作會維持取消狀態且不會誤報成功。hash 驗證到 executable 啟動之間仍有本機 TOCTOU，正式 portable 目錄須由受信任發行流程保護，不能把 manifest 當作執行期信任根。

## 發行前清單

- 以 `scripts/fetch-tools.ps1` 重新驗證鎖定的 yt-dlp/FFmpeg/FFprobe、正式 manifest 與授權檔；更新 yt-dlp 時重新驗證 release checksum 簽章與完整 fingerprint。另需決定並鎖定 yt-dlp EJS 與受信任 JavaScript runtime 的 portable 供應鏈；本 MVP 不從 PATH 載入。
- 在乾淨 Windows 11 x64 環境執行 `cargo build --release --locked`、Rust test、前端 build 與 fixture benchmark。
- 只以公開且明確有權下載的內容做 live smoke；記錄實際條件與日期，不把單一 URL 結果擴張成整站永久保證。
- 檢查 portable 目錄沒有 `.incomplete` 殘留、logs、Cookie、秘密或未追蹤 binaries。
