# 指定平台 Live 驗收報告

日期：2026-09-07（Asia/Taipei）  
範圍：使用者指定的小鴨影音、歐樂影院、MMOV 與 Facebook 公開來源之條件式驗證。完整 URL 不寫入應用程式 log；本報告只記錄平台、媒體 host 與必要技術結果。

## 安全閘門

兩個來源頁均可經 Windows TLS 驗證直接取得 HTTP 200，無須登入、Cookie、CAPTCHA、付費牆、地區規避、TLS 停用或不明權杖。頁面公開 player JSON 的 `encrypt`、`trysee`、`points` 均為數字 0。Rust adapter 對實際頁面完成下列驗證：

- 頁面與媒體皆為 HTTPS；禁止 redirect 與 proxy，remote peer/DNS 皆為公開位址。
- 小鴨影音媒體只出現在 `v2.ppqrrs.com`／`v2.adfg8.vip`；歐樂影院只出現在 `europe.olemovienews.com`。
- master 與 variant 皆為 `#EXTM3U`；leaf 為 VOD 且有 `#EXT-X-ENDLIST`。
- 所有 URI 數量在 5,000 上限內；未見 `#EXT-X-KEY` 或 `#EXT-X-SESSION-KEY`。
- Olevod 對空 User-Agent 回 403；固定使用誠實 app 識別 `WindowsMediaDownloader/0.1` 即回 200。這不是瀏覽器 impersonation，程式明確禁止 `--impersonate`。

## Probe

使用鎖定的 yt-dlp `2026.08.19`，參數包含 `--simulate`、`--skip-download`、`--no-playlist`、`--no-plugin-dirs`、`--no-js-runtimes`、`--no-remote-components`。只有 MMOV 加入 `--compat-options no-certifi`，讓該平台使用 Windows 作業系統信任鏈且不會停用 TLS 驗證；其他平台不套用此選項。

| 平台 | adapter | 媒體 host | extractor | 時長 | availability | has_drm |
| --- | --- | --- | --- | ---: | --- | --- |
| 歐樂影院 | 通過 | `europe.olemovienews.com` | generic | 8,034 秒 | null（extractor 未提供） | false |
| 小鴨影音 | 通過 | `v2.ppqrrs.com` | generic | 5,912 秒 | null（extractor 未提供） | false |

`availability=null` 不是「明確授權」判定；它只表示 generic extractor 未提供該欄位。實際下載權利仍由使用者確認。

## 最小實際下載

通過上述閘門後，只下載每站約 3 秒並以固定 FFprobe 驗證。未下載完整影片，且沒有保留或散布樣本。

| 平台／模式 | yt-dlp 結束碼 | 端到端時間 | 輸出 | FFprobe |
| --- | ---: | ---: | --- | --- |
| 歐樂影院／video | 0 | 4.548 秒 | 636,779 bytes MP4 | 3.040 秒；H.264 + AAC |
| 小鴨影音／audio | 0 | 7.208 秒 | 75,127 bytes MP3 | 3.019 秒；MP3 audio |

樣本計算 SHA-256 並完成驗證後已移至 Windows 資源回收筒，專案與 portable 套件均未包含下載內容。

## 效能與限制

單一 HLS segment 的 HTTPS Range 取樣上限為 1 MiB：歐樂影院 411,295 B/s、小鴨影音 358,985 B/s。外部 CDN、時間與網路都會變動，因此不能以這兩個值保證整片完成時間。完整固定基準未下載，20 分鐘閘門狀態為 `not-evaluated`。

程式對兩個直接 HLS 平台固定 4 個 concurrent fragments，全域最多同時 3 筆；影片維持 copy/remux，避免不必要重編碼。這是效能保護措施，不會規避站台限速。完整方法與本機 fixture 結果見 `PERFORMANCE.md`。

本報告只驗證指定 URL 在上述日期與環境的行為。站台改版、媒體 host 變更、憑證、下架或加入保護後，程式會 fail closed；不能據此宣稱所有節目、所有 URL 或未來狀態皆支援。

## Facebook（條件式驗證）

2026-09-07 由 Sol 以兩個使用者明確授權的公開 URL 執行安全 probe 與各約 3 秒的 video smoke。以下僅記錄去敏後的必要結果；不記錄完整 URL、title、fbcdn 位址或執行期 token，也不由測試結果判定著作權。

### 安全 probe

- 使用鎖定的 yt-dlp；本次執行檔 SHA-256 以 `666749...e7a` 表示（僅保留去敏前綴／尾碼）。
- 兩個來源均採匿名、無 Cookie、無登入、無 bypass；reel 公開影片 ID `1584857163015601` 結束碼 0、3.222 秒、duration 14.997 秒、extractor=`facebook`。
- PCB page/video 公開影片 ID `2093187601588241` 原始 path 結束碼 0、1.910 秒、duration 42 秒；canonical 相同影片 ID 結束碼 0、2.242 秒。
- 兩個來源的 `availability`、`is_live`、`live_status`、playlist 欄位均為 null／無 marker；DRM metadata 未出現已知 marker，不將此寫成明確 false。
- 這些結果是匿名解析與公開可存取的實證，不代表整站、未來版本或著作權授權；站台變更或受限內容仍會依安全閘門拒絕。

### 最小 video smoke

每筆下載前均先通過相同安全 probe，僅取樣約 3 秒；樣本驗證後已永久清除，忽略測試目錄為空，沒有保存或散布內容。

| 來源形態 | yt-dlp 結束碼 | 下載時間 | 輸出 | FFprobe | 估算速率 | 取樣 SHA-256 |
| --- | ---: | ---: | --- | --- | ---: | --- |
| reel 公開影片 ID `1584857163015601` | 0 | 4.022 秒 | 823,117 bytes MP4 | 3.009 秒；AV1 + AAC | 約 204,659 B/s | `12c521...f48d0` |
| PCB canonical 公開影片 ID `2093187601588241` | 0 | 2.172 秒 | 1,241,749 bytes MP4 | 3.083 秒；H.264 video-only | 約 571,708 B/s | `ab4316...07d6` |

約 3 秒 smoke 不能代表整片下載；20 分鐘完整基準閘門仍為 `not-evaluated`。第二個樣本本身未含可辨識音訊串流，不代表 audio 模式失效，但表示該來源可能沒有音訊。Facebook 不接受 fbcdn URL、Cookie、token、登入、直播或存取控制繞過。

## MMOV（條件式驗證）

2026-09-07 由 Sol 以一個使用者明確授權的公開來源完成 Rust opt-in adapter 驗證；與既有小鴨影音／歐樂影院回歸同次執行，adapter 測試 1 passed、總耗時 7.71 秒。以下只記錄去敏後的必要結果，不記錄完整 URL、CDN URL/path、title 或執行期 token。

### 安全 probe

- 鎖定的 yt-dlp SHA-256 已吻合；probe 不下載、無 Cookie、無登入、無 bypass，結束碼 0。
- extractor=`generic`、protocol=`m3u8_native`、duration=4,012 秒、availability=`null`、DRM metadata=`false`；`is_live`、`live_status` 與 playlist markers 均為 `null`。
- 結果只代表該指定公開頁面在當日條件式通過，不宣稱整站或未來狀態。

### 最小 video smoke

約 3 秒 video smoke 結束碼 0，wall time 7.223 秒，輸出 2,950,946 bytes；以輸出 bytes／牆鐘粗估約 0.39 MiB/s（3.268 Mbit/s），不代表完整下載效能。FFprobe duration=3.040 秒，1920x1080 MPEG-4 video + AAC 44.1kHz stereo。

此精確 cut 使用 `force-keyframes` 造成樣本轉碼，不能推論正式下載的 codec 或速度。樣本已從 `.live-smoke/mmov-sol-20260907` 刪除且不可復原，目錄無殘留。

MMOV 的完整 66 分鐘與 20 分鐘效能閘門仍為 `not-evaluated`。adapter preflight 不等同於 yt-dlp/FFmpeg 後續連線的完整網路 sandbox，redirect/DNS rebinding 仍列為殘餘 TOCTOU 風險。

### 來源 `510813/4-1` 的條件式失敗

2026-09-07 重新檢查使用者回報的來源時，MMOV HTML 頁面可匿名取得並解析出 allowlist 內的 HLS URL，但媒體端點 `bfikuncdn.com` 對固定應用程式 User-Agent、一般瀏覽器 User-Agent、Referer 與 Origin 組合均回傳 nginx `403 Forbidden`。因此此來源在當時環境不可下載；程式維持 fail closed，不嘗試 Cookie、登入、代理、地區規避或存取控制繞過。錯誤訊息已改為顯示實際拒絕請求的 HTTPS host，避免誤判為來源頁面本身被拒絕。

### 來源 `510813/5-1` 的條件式驗證

2026-09-07 檢查此使用者指定來源時，頁面解析出 `b3.bdzybf22.com:443` manifest；其 4,036 個 segment URI 全部落在 `tsb3.bdzybf22.com:443`。兩個 host 的 DNS 均只回傳本次觀測到的公開 IPv4，manifest 約 435 KiB、標記 VOD 與 ENDLIST、未含 KEY，URI 數低於 5,000 上限。程式僅加入這兩個 exact host/port，並把 `tsb3` 限定為 manifest 內的 segment-only host，不允許萬用 `*.bdzybf22.com`。

不下載 probe 結束碼 0、3.444 秒，extractor=`generic`、protocol=`m3u8_native`、duration=8,070 秒、DRM metadata=`false`。yt-dlp 原生 HLS `--test` smoke 結束碼 0、4.915 秒，取得首段 341,036 bytes MP4；FFprobe 為 2.023 秒、1920x816 H.264 + AAC 44.1kHz stereo。segment URL 使用 `.jpeg` 副檔名，FFmpeg 精確切段會依其安全副檔名規則拒絕，但正式下載使用的 yt-dlp 原生 HLS 路徑可正常處理；未關閉 FFmpeg 安全檢查。樣本 SHA-256 為 `663f88...ad5672`，驗證後已移至 Windows 資源回收筒。完整 134.5 分鐘下載與 20 分鐘效能閘門仍為 `not-evaluated`。

### 來源 `512338/4-1` 的動態 URI 行為

2026-09-07 使用者回報一次 `MMOV HLS URL 不得包含 query 或 fragment`；隨後直接檢查頁面、master、child manifest 與 1,387 個 segment URI，均未含 query／fragment，四個 KEY 全為 `METHOD=NONE`。同一 Rust adapter 第一次重現失敗，之後連續五次 MMOV 解析通過，顯示站台回應具有短暫動態差異。程式沒有放寬 query 規則；只在此特定錯誤時等待 300 ms 並從來源頁重新執行一次完整安全解析，第二次不合規即拒絕。重試不保存或傳遞任何 query/token，也不套用於 403、host allowlist 或其他安全錯誤。

## 既有平台回歸 probe

另以一個公開、19 秒的 YouTube ID 分別經 `www.youtube.com` 與 `music.youtube.com` 執行相同的不下載 probe；兩者結束碼皆為 0、extractor=`youtube`、availability=`public`。沒有下載該內容。因 portable 尚未攜帶 yt-dlp EJS 與受信任 JavaScript runtime，這只驗證基本路徑，不能宣稱所有 YouTube 格式完整可用。
