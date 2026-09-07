# 平台能力矩陣（Windows MVP）

| 平台 | hostname 規則 | URL 結構驗證 | live smoke | MVP 狀態 |
| --- | --- | --- | --- | --- |
| YouTube | `youtube.com`、`www.youtube.com`、`m.youtube.com`、`youtu.be` | HTTPS、DNS、無 playlist | 2026-09-07：公開 19 秒影片 probe 通過；未下載 | 基本流程可用；完整格式仍待 EJS／受信任 JavaScript runtime 納入並鎖定 |
| YouTube Music | `music.youtube.com`、`www.music.youtube.com` | HTTPS、DNS、無 playlist | 2026-09-07：同一公開 ID 經 Music host probe 通過；未下載 | 基本流程可用；完整格式仍待 EJS／受信任 JavaScript runtime 納入並鎖定 |
| Facebook | 僅 `www.facebook.com` | HTTPS、DNS、無 playlist、公開匿名 reel 或 page/video 純數字 ID 路徑；PCB 為 path segment，任何 query 均拒絕 | 2026-09-07：兩個使用者明確授權的公開 URL（reel 與 PCB page/video 各一）probe 與約 3 秒 video smoke 通過 | 僅對上述兩個授權 URL 條件式通過；不宣稱整站或未來可用，改版、受限內容或 extractor 非 `facebook` 會 fail closed |
| 小鴨影音 | 頁面僅 `play.777tv.ai`；媒體僅 `v2.ppqrrs.com`、`v2.adfg8.vip` | HTTPS、公開 DNS/peer、固定 player JSON、旗標皆 0、無 key、VOD ENDLIST | 2026-09-07：指定 URL adapter/probe 通過；3.019 秒 MP3 樣本通過 ffprobe | 條件式可用；只驗證指定頁面與當時站台格式，改版後可 fail closed |
| 歐樂影院 | 頁面僅 `olevod.com`、`www.olevod.com`；媒體僅 `europe.olemovienews.com` | HTTPS、公開 DNS/peer、固定 player JSON、旗標皆 0、無 key、VOD ENDLIST | 2026-09-07：指定 URL adapter/probe 通過；3.040 秒 H.264/AAC MP4 樣本通過 ffprobe | 條件式可用；只驗證指定頁面與當時站台格式，改版後可 fail closed |

所有平台均只有 audio/video；字幕、登入、Cookie、CAPTCHA、地區/付費牆/DRM/存取控制繞過不在範圍內。

小鴨影音與歐樂影院的 yt-dlp extractor 均為 `generic`；程式先由 Rust adapter 取得並驗證公開 HLS，再把已驗證 target 交給固定 sidecar。兩站 probe 的 `availability` 為缺省 `null`、格式 `has_drm=false`。公開可連線與未見保護不等同於程式能判定著作權；使用者仍須確認下載權利。

Facebook 不使用自訂 HLS adapter；Rust 只把經 allowlist 驗證並最小化的 `www.facebook.com` 來源 URL 交給固定 yt-dlp。每筆 probe 會拒絕直播、非 `not_live` lifecycle、playlist marker，並要求 extractor 為 `facebook`。本次結果只適用於兩個授權公開 URL，不擴張為整站或未來支援。
