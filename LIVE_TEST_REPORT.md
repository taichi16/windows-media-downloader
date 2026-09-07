# 指定平台 Live 驗收報告

日期：2026-09-07（Asia/Taipei）  
範圍：使用者指定的小鴨影音與歐樂影院各一個公開頁面。完整 URL 不寫入應用程式 log；本報告只記錄平台、媒體 host 與必要技術結果。

## 安全閘門

兩個來源頁均可經 Windows TLS 驗證直接取得 HTTP 200，無須登入、Cookie、CAPTCHA、付費牆、地區規避、TLS 停用或不明權杖。頁面公開 player JSON 的 `encrypt`、`trysee`、`points` 均為數字 0。Rust adapter 對實際頁面完成下列驗證：

- 頁面與媒體皆為 HTTPS；禁止 redirect 與 proxy，remote peer/DNS 皆為公開位址。
- 小鴨影音媒體只出現在 `v2.ppqrrs.com`／`v2.adfg8.vip`；歐樂影院只出現在 `europe.olemovienews.com`。
- master 與 variant 皆為 `#EXTM3U`；leaf 為 VOD 且有 `#EXT-X-ENDLIST`。
- 所有 URI 數量在 5,000 上限內；未見 `#EXT-X-KEY` 或 `#EXT-X-SESSION-KEY`。
- Olevod 對空 User-Agent 回 403；固定使用誠實 app 識別 `WindowsMediaDownloader/0.1` 即回 200。這不是瀏覽器 impersonation，程式明確禁止 `--impersonate`。

## Probe

使用鎖定的 yt-dlp `2026.08.19`，參數包含 `--simulate`、`--skip-download`、`--no-playlist`、`--no-plugin-dirs`、`--no-js-runtimes`、`--no-remote-components`。`--compat-options no-certifi` 只讓 Windows standalone 使用作業系統信任鏈，未停用 TLS 驗證。

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

## 既有平台回歸 probe

另以一個公開、19 秒的 YouTube ID 分別經 `www.youtube.com` 與 `music.youtube.com` 執行相同的不下載 probe；兩者結束碼皆為 0、extractor=`youtube`、availability=`public`。沒有下載該內容。因 portable 尚未攜帶 yt-dlp EJS 與受信任 JavaScript runtime，這只驗證基本路徑，不能宣稱所有 YouTube 格式完整可用。
