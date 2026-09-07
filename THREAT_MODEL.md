# 威脅模型

## 資產

- 使用者檔案系統、下載媒體與程序完整性。
- yt-dlp/FFmpeg/FFprobe sidecar 完整性及其可執行權限。
- 使用者輸入的 URL、平台選擇、模式與工作狀態。
- Tauri IPC 與前端 UI 的命令界線。
- app-local SQLite 下載紀錄（來源識別、標題、檔名、終態與耗時統計）。

## 主要攻擊面與緩解

| 威脅 | 例子 | MVP 緩解 | 剩餘風險 |
| --- | --- | --- | --- |
| SSRF/內網探測 | URL 指向 `127.0.0.1`、RFC1918、DNS 回傳私有 IP | HTTPS、固定 hostname、啟動前所有 DNS 結果檢查 | yt-dlp 後續解析/redirect 沒有獨立 proxy boundary |
| Host confusion | `youtube.com.evil`、userinfo、IP literal | 精確 allowlist、拒絕 userinfo/port/IP | 站台本身內容仍可能惡意 |
| MMOV CDN/playlist 混淆 | page `videoSrc` 指向非 allowlist host、固定 host 使用錯誤 port、manifest redirect 或子 URI 跳出 allowlist | source 只接受精確 `hk.mmov.io` VOD path；Rust 只解析唯一有界 `videoSrc`；media 只接受固定 exact host/port，BDZY 的 `tsb3` 限定為 segment-only；每個 effective endpoint 做公開 DNS 與 response peer 檢查；HLS 3 層／5,000 URI、`#EXTM3U`、leaf ENDLIST、clear-key only、拒絕 live-only/DASH/mixed playlist | yt-dlp/FFmpeg 後續連線仍不在獨立 proxy 或 OS 網路 sandbox 內，存在執行期 redirect/DNS rebinding TOCTOU |
| 參數注入 | URL/前端傳 `--exec`、任意輸出路徑 | 固定 Vec args、`--`、不接受 CLI 欄位、固定 output template | yt-dlp 本身仍需受信任且定期更新 |
| Sidecar 供應鏈 | 被替換的 yt-dlp/FFmpeg，或 binary 與 manifest 同時被替換 | 固定官方／官方指向 release、checksum/signature 指紋、archive/檔案 SHA-256、resource 固定路徑、formal manifest、fail closed；停用 plugin、JS runtime、remote component | 未簽署 portable 仍可能被可寫入本機目錄的攻擊者同時替換；EJS/runtime 尚未納入供應鏈 |
| 路徑穿越/覆寫 | title、extension、symlink、junction、被替換的自訂根 | Rust 持有 root、native picker 不接受前端路徑、UUID、extension allowlist、設定檔 atomic write-through replace、root/incomplete/job reparse 檢查、非 symlink 單檔、原子 rename | 同一權限程序可在檢查與 filesystem syscall 間競態；Windows ACL、kernel sandbox 與 mapped-drive 狀態需在目標環境額外驗收 |
| 資源耗盡 | 播放清單、直播 lifecycle、無界 stderr、無限工作 | `--no-playlist`、probe 拒絕 live/非 `not_live` 與 playlist marker、5 筆/3 concurrency、64 KiB capture、timeout | 網路/媒體大小仍受 OS 磁碟與 yt-dlp 行為影響 |
| 程序殘留 | ffmpeg 子程序在取消後繼續 | 固定 taskkill `/T` + child kill/reap | 啟動/取消極短競態，不能宣稱 Job Object 完整保證 |
| 隱私外洩 | Cookie、登入資料、外部 HTTP server | `--ignore-config`、不匯入 Cookie、Tauri IPC、嚴格 CSP | 使用者自行提供的公開 URL 仍會送至目標平台 |
| 歷史資料殘留／越界 | DB 被鎖定、損壞、注入 query/路徑或透過 UI 清理誤刪媒體 | DB 固定 app-local、schema transaction、busy timeout、mutex 序列化、job_id 冪等、最多保留 1,000、來源/標題/檔名去敏截長、固定錯誤 code；清理只刪歷史表 | 同一使用者權限仍可讀取 app-local 檔案；SQLite 失敗時無法提供歷史但不影響下載；不提供加密保管 |

## 信任邊界

前端是不受信任輸入來源；Rust command layer 重新驗證所有平台/URL/批次限制與輸出 root。輸出目錄 picker 由 Rust 的 Tauri dialog plugin 呼叫，前端不取得 dialog/fs/shell 權限，也不能把路徑或 CLI 引數放進 start payload。Rust 只啟動 sidecar 中固定的 executables，不信任 PATH。Tauri capability 僅使用 core default，未開放 shell、fs 或 HTTP plugin；下載網路流量由 yt-dlp 直接處理，因此 redirect/DNS rebinding 限制必須對使用者揭露。

Facebook 的 trust boundary 另限制為 `www.facebook.com` 及兩種公開匿名純數字 ID 路徑；PCB 是 path segment，任何 query（包含追蹤、簽章與 token）均拒絕，canonical history/source 不保留 query，且不接受 fbcdn 媒體 URL。每筆 probe 另拒絕直播、非 `not_live` lifecycle、playlist marker，Facebook extractor 不為 `facebook` 時也拒絕。這些規則的單元測試不等於 Facebook live 驗證；本報告只記錄兩個特定授權 URL 的條件式結果，不代表整站或未來狀態。

MMOV 的 trust boundary 只允許 `hk.mmov.io` page；使用者不能提供 CDN target，Rust adapter 會在 native TLS、無 redirect/proxy 的 bounded request 中解析唯一 `videoSrc`，再遞迴掃描精確 host/port allowlist。2026-09-07 僅針對使用者指定頁面完成匿名 live probe 與約 3 秒 smoke；完整 66 分鐘與 20 分鐘效能門檻仍未驗證。這不代表整站、其他來源或未來頁面持續可用。文件不把 adapter 的 preflight 誤述為 yt-dlp/FFmpeg 的完整網路 sandbox。

歷史 store 是獨立的本機信任邊界：manager 只在狀態已決定後傳入受限欄位，store 不接受前端任意 SQL、路徑或原始 stderr。SQLite 鎖定、損壞及寫入錯誤以固定 warning fail-soft；它們不能把已完成媒體回寫為 failed。狀態卡清理和歷史清理為不同 command；兩者都不刪除任何媒體檔，也不終止、修改或影響其他下載程序。
