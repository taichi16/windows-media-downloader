# 效能測試方法與閘門

`scripts/benchmark-fixture.mjs` 是本機可重現、僅 loopback 的控制基準。它啟動只綁定 `127.0.0.1` 的 HTTP Range fixture server，讓每個工作以實際 HTTP response stream 下載確定內容、驗證完整 hash，再同時測量 concurrency 1、2、3、5；分塊不是 yt-dlp 網路 fragment。這個 server 不經應用程式 SSRF allowlist，也不使用 live 站台或 ffmpeg。每個組合至少執行三次並取 median，輸出包含 elapsed milliseconds、throughput、checksum、CPU/環境摘要與 `fixtureOnly=true`。

```powershell
node scripts/benchmark-fixture.mjs --fixture-mib 64 --repetitions 3 --out-dir benchmarks/out
```

基準不是 live 站台測試，不能推論 yt-dlp、YouTube、YouTube Music、小鴨影音或歐樂影院的網路速度、格式可用性或轉檔成本。未提供外部 baseline 時輸出 `gate=not-evaluated`；只有使用 `--baseline-minutes N` 明確提供固定基準影片實測分鐘數，才會在超過 20 分鐘時輸出 `gate=fail`，並列出瓶頸建議。本機 loopback fixture 絕不會冒充 live 結果；報告不寫入帳號或暫存的個人路徑。

## 2026-09-07 結果與決策

- 五輪 64 MiB、每組 3 次的本機 fixture，在全域 concurrency 3、分塊 1/2/4/8 時，中位吞吐依序為：512.38／517.55／465.64／408.73、422.56／419.84／468.57／429.49、568.20／539.45／485.39／451.98、552.13／514.18／565.46／457.40，以及本次 UI 改版最終驗收的 534.04／546.39／554.92／452.90 MiB/s。各輪最佳分塊不同，證明 loopback 會受 cache／排程與環境波動影響，不能拿它替 yt-dlp 的 HLS fragment 調參；20 分鐘 gate 仍為 `not-evaluated`。
- 兩個外部 CDN 的單連線取樣只有約 0.36–0.41 MB/s，因此 HLS 採固定 4 concurrent fragments 作為有界折衷，而不是宣稱「fixture 證明 4 最快」。全域仍只啟動 3 筆工作，最多 12 個 HLS fragment request；完整基準完成前不得再無上限提高。
- 每站另以 HTTPS Range 206、最多 1 MiB 做單連線外部取樣：歐樂影院 411,295 B/s，小鴨影音 358,985 B/s。這只是單一時間點、單一 segment 的 CDN 樣本，不可當作完整影片速度保證。
- 極短端到端 smoke：歐樂影院 3.040 秒影片在 4.548 秒內完成；小鴨影音 3.019 秒音訊（含 MP3 後處理）在 7.208 秒內完成。固定啟動成本使它們不能線性外推完整影片。
- 完整固定基準影片未下載，因此 20 分鐘閘門的誠實狀態仍是 **not-evaluated**。這個限制保留在測試報告，不會用估算宣稱通過。

影片模式優先直接格式與 remux，不做不必要的影像重編碼；音訊模式轉為 MP3，會受 CPU 與來源格式影響。若後續經授權的完整固定基準超過 20 分鐘，必須記錄網路／fragment／remux 各階段，再重新比較 fragment 2/4/8 與全域工作併發，不能停用 TLS 或繞過站台限速。
