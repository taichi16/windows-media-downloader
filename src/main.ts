import { invoke } from "@tauri-apps/api/core";
import "./styles.css";

type Platform = "youtube" | "youtube-music" | "little-duck" | "olevod" | "facebook";
type Mode = "audio" | "video";
type JobState = "queued" | "running" | "completed" | "failed" | "cancelled";

interface DownloadRequest {
  platform: Platform;
  url: string;
  mode: Mode;
}

interface DownloadStatus extends DownloadRequest {
  id: string;
  state: JobState;
  progress_percent: number;
  speed_bps: number | null;
  eta_seconds: number | null;
  output_path: string | null;
  error: string | null;
}

interface SidecarStatus {
  ready: boolean;
  message: string;
}

interface OutputDirectoryInfo {
  path: string;
  is_default: boolean;
  warning: string | null;
}

interface HistoryRecord {
  job_id: string;
  platform: Platform;
  mode: Mode;
  probe_title: string | null;
  source_link: string;
  terminal_status: "completed" | "failed" | "cancelled" | string;
  started_at_ms: number;
  finished_at_ms: number;
  elapsed_ms: number;
  output_filename: string | null;
  error_code: string | null;
  error_summary: string | null;
}

interface HistoryStats {
  total: number;
  completed: number;
  failed: number;
  cancelled: number;
  cumulative_elapsed_ms: number;
}

interface HistoryList {
  records: HistoryRecord[];
  stats: HistoryStats;
  warning: string | null;
  limit: number;
}

interface HistoryClearResult {
  removed: number;
  warning: string | null;
}

const MAX_ROWS = 5;
type ConsoleTab = "jobs" | "history";

const appRoot = document.querySelector<HTMLDivElement>("#app") as HTMLDivElement;
if (!appRoot) throw new Error("找不到應用程式根節點");

// `rows` are unsubmitted editor drafts.  Successful start consumes only the
// snapshot sent for that IPC call; status history lives separately in
// `statuses` and is never used as an editor buffer.
let rows: DownloadRequest[] = [newRow()];
let statuses: DownloadStatus[] = [];
let busy = false;
let messageText = "";
let messageIsError = false;
let sidecarStatus: SidecarStatus = { ready: false, message: "sidecar 檢查中…" };
let outputDirectory: OutputDirectoryInfo | null = null;
let outputNotice = "";
let outputNoticeIsError = false;
let aboutTrigger: HTMLButtonElement | null = null;
let historyClearTrigger: HTMLButtonElement | null = null;
let historyRefreshInFlight = false;
let historyBusy = false;
let historyNotice = "";
let historyNoticeIsError = false;
let activeConsoleTab: ConsoleTab = "jobs";
let jobsScrollTop = 0;
let historyScrollTop = 0;
let historyData: HistoryList = {
  records: [],
  stats: { total: 0, completed: 0, failed: 0, cancelled: 0, cumulative_elapsed_ms: 0 },
  warning: null,
  limit: 100,
};

function newRow(): DownloadRequest {
  return { platform: "youtube", url: "", mode: "video" };
}

function cloneRequest(request: DownloadRequest): DownloadRequest {
  return { platform: request.platform, url: request.url, mode: request.mode };
}

function mergeStatuses(current: DownloadStatus[], accepted: DownloadStatus[]): DownloadStatus[] {
  const merged = [...current];
  const indexes = new Map(merged.map((status, index) => [status.id, index]));
  for (const status of accepted) {
    const index = indexes.get(status.id);
    if (index === undefined) {
      indexes.set(status.id, merged.length);
      merged.push(status);
    } else {
      merged[index] = status;
    }
  }
  return merged;
}

function platformName(platform: string): string {
  return ({
    youtube: "YouTube",
    "youtube-music": "YouTube Music",
    "little-duck": "小鴨影音（條件式）",
    olevod: "歐樂影院（條件式）",
    facebook: "Facebook（條件式）",
  } as Record<string, string>)[platform] ?? "未知平台";
}

function stateName(state: string): string {
  return ({
    queued: "排隊中",
    running: "下載中",
    completed: "完成",
    failed: "失敗",
    cancelled: "已取消",
  } as Record<string, string>)[state] ?? "未知";
}

function modeName(mode: string): string {
  return ({ audio: "Audio", video: "Video" } as Record<string, string>)[mode] ?? "未知模式";
}

function formatSpeed(value: number | null): string {
  if (value === null || !Number.isFinite(value) || value < 0) return "—";
  if (value >= 1024 * 1024) return `${(value / 1024 / 1024).toFixed(1)} MiB/s`;
  if (value >= 1024) return `${(value / 1024).toFixed(1)} KiB/s`;
  return `${value.toFixed(0)} B/s`;
}

function formatEta(value: number | null): string {
  if (value === null || !Number.isFinite(value) || value < 0) return "ETA —";
  return `ETA ${Math.floor(value)}s`;
}

function consoleTabLabel(tab: ConsoleTab): string {
  return tab === "jobs" ? "即時工作" : "下載紀錄";
}

function badgeCount(value: number): string {
  const count = safeCount(value);
  return count > 99 ? "99+" : String(count);
}

function jobStateClass(state: string): string {
  return ["queued", "running", "completed", "failed", "cancelled"].includes(state) ? state : "unknown";
}

function captureConsoleViewport(): void {
  const jobs = document.querySelector<HTMLElement>("#jobs");
  const history = document.querySelector<HTMLElement>("#history-list");
  const visibleScrollTop = (viewport: HTMLElement | null): number | null => {
    if (!viewport || viewport.closest<HTMLElement>('[role="tabpanel"]')?.hidden) return null;
    return viewport.scrollTop;
  };
  const visibleJobsScrollTop = visibleScrollTop(jobs);
  const visibleHistoryScrollTop = visibleScrollTop(history);
  if (visibleJobsScrollTop !== null) jobsScrollTop = visibleJobsScrollTop;
  if (visibleHistoryScrollTop !== null) historyScrollTop = visibleHistoryScrollTop;
}

function restoreConsoleViewport(): void {
  const jobs = document.querySelector<HTMLElement>("#jobs");
  const history = document.querySelector<HTMLElement>("#history-list");
  if (jobs) jobs.scrollTop = jobsScrollTop;
  if (history) history.scrollTop = historyScrollTop;
}

function setConsoleTab(tab: ConsoleTab, focus = false): void {
  if (activeConsoleTab === tab && !focus) return;
  activeConsoleTab = tab;
  render();
  if (focus) {
    document.querySelector<HTMLButtonElement>(`[role="tab"][data-console-tab="${tab}"]`)?.focus();
  }
}

function isConsoleTab(value: string | undefined): value is ConsoleTab {
  return value === "jobs" || value === "history";
}

function render(): void {
  captureConsoleViewport();
  const aboutWasOpen = document.querySelector<HTMLDialogElement>("#about-dialog")?.open ?? false;
  const historyClearWasOpen = document.querySelector<HTMLDialogElement>("#history-clear-dialog")?.open ?? false;
  const visibleOutputPath = outputDirectory?.path ?? "載入中…";
  const visibleOutputNotice = outputNotice || outputDirectory?.warning || "";
  const visibleHistoryNotice = historyNotice || historyData.warning || "";
  appRoot.innerHTML = `
    <section class="shell">
      <header class="hero">
        <div class="brand-lockup">
          <img class="brand-mark" src="/logo.svg" width="48" height="48" alt="" aria-hidden="true" />
          <div>
          <p class="eyebrow">WINDOWS 11 · PORTABLE MVP</p>
          <h1>Windows 影音下載工具</h1>
          <p class="subtitle">固定平台允許清單、HTTPS 與啟動前 DNS 安全驗證。</p>
          </div>
        </div>
        <div class="hero-actions">
          <div id="sidecar" class="status-pill${sidecarStatus.ready ? " ready" : ""}" title="${escapeAttribute(sidecarStatus.message)}">${sidecarLabel()}</div>
          <button id="about" class="secondary compact" type="button">關於</button>
        </div>
      </header>
      <section class="panel">
        <div class="panel-heading">
          <div>
            <h2>下載工作</h2>
            <p>最多 5 筆；同一時間最多 3 筆。小鴨影音與歐樂影院已於指定公開頁面完成驗證；站台變更時會安全拒絕。</p>
          </div>
          <span class="counter">${rows.length} / ${MAX_ROWS}</span>
        </div>
        <div class="output-settings">
          <span class="output-label">完成檔目錄${outputDirectory === null ? "（載入中）" : outputDirectory.is_default ? "（預設）" : "（自訂）"}</span>
          <strong id="output-path" title="${escapeAttribute(visibleOutputPath)}" aria-label="完成檔目錄：${escapeAttribute(visibleOutputPath)}">${escapeHtml(visibleOutputPath)}</strong>
          <span id="output-note" class="output-status${outputNoticeIsError ? " error" : ""}" aria-live="polite" ${visibleOutputNotice ? "" : "hidden"}>${escapeHtml(visibleOutputNotice)}</span>
          <button id="choose-output" class="secondary" type="button" ${busy ? "disabled" : ""}>選擇儲存資料夾</button>
        </div>
        <div id="rows" class="rows"></div>
        <div class="actions">
          <button id="add" class="secondary" ${rows.length >= MAX_ROWS || busy ? "disabled" : ""}>新增工作</button>
          <button id="probe" class="secondary" ${busy ? "disabled" : ""}>Probe（不下載）</button>
          <button id="start" class="primary" ${busy ? "disabled" : ""}>開始下載</button>
          <button id="cancel-all" class="danger">取消全部</button>
        </div>
        <output id="message" class="message${messageIsError ? " error" : ""}" aria-live="polite" ${messageText ? "" : "hidden"}>${escapeHtml(messageText)}</output>
      </section>
      <section class="panel control-console" aria-label="工作控制台">
        <div class="console-tabs" role="tablist" aria-label="即時工作與下載紀錄">
          <button id="tab-jobs" class="console-tab" type="button" role="tab" data-console-tab="jobs" aria-controls="jobs-panel" aria-selected="${activeConsoleTab === "jobs"}" tabindex="${activeConsoleTab === "jobs" ? "0" : "-1"}">
            <span>${consoleTabLabel("jobs")}</span><span id="jobs-badge" class="tab-badge" aria-label="追蹤工作數 ${badgeCount(statuses.length)}">${badgeCount(statuses.length)}</span>
          </button>
          <button id="tab-history" class="console-tab" type="button" role="tab" data-console-tab="history" aria-controls="history-panel" aria-selected="${activeConsoleTab === "history"}" tabindex="${activeConsoleTab === "history" ? "0" : "-1"}">
            <span>${consoleTabLabel("history")}</span><span id="history-badge" class="tab-badge" aria-label="下載紀錄總數 ${badgeCount(historyData.stats.total)}">${badgeCount(historyData.stats.total)}</span>
          </button>
        </div>
        <div id="jobs-panel" class="console-panel" role="tabpanel" aria-labelledby="tab-jobs" tabindex="0" ${activeConsoleTab === "jobs" ? "" : "hidden"}>
          <div class="console-panel-toolbar">
            <div class="console-toolbar-heading">
              <div>
                <h2>工作狀態</h2>
                <p>預設完成檔位於 Windows 的 Downloads&#92;Windows Media Downloader；暫存位於其下 .incomplete&#92;UUID，成功且必要的 ffprobe 驗證通過後才移入輸出目錄。</p>
              </div>
              <button id="clear-finished" class="secondary" type="button" ${busy ? "disabled" : ""}>清除已結束</button>
            </div>
          </div>
          <div id="jobs" class="jobs list-viewport"><p class="empty">尚未建立下載工作。</p></div>
        </div>
        <div id="history-panel" class="console-panel" role="tabpanel" aria-labelledby="tab-history" tabindex="0" ${activeConsoleTab === "history" ? "" : "hidden"}>
          <div class="console-panel-toolbar">
            <div class="console-toolbar-heading">
              <div>
                <h2>下載紀錄</h2>
                <p>僅保留本機最新 1000 筆，畫面顯示最新 ${historyData.limit} 筆；累計工作耗時是各工作自接受排程至終態的總和，並行時不是牆鐘時間。</p>
              </div>
              <div class="history-actions">
                <button id="history-refresh" class="secondary" type="button" ${historyRefreshInFlight || historyBusy || busy ? "disabled" : ""}>重新整理</button>
                <button id="history-clear" class="secondary" type="button" ${historyBusy || busy ? "disabled" : ""}>清除全部歷史</button>
              </div>
            </div>
            <div class="history-stats" aria-label="下載紀錄統計">
              <span>總數 <strong>${safeCount(historyData.stats.total)}</strong></span>
              <span>完成 <strong>${safeCount(historyData.stats.completed)}</strong></span>
              <span>失敗 <strong>${safeCount(historyData.stats.failed)}</strong></span>
              <span>取消 <strong>${safeCount(historyData.stats.cancelled)}</strong></span>
              <span>累計耗時 <strong>${formatElapsed(historyData.stats.cumulative_elapsed_ms)}</strong></span>
            </div>
            <p class="history-notice${historyNoticeIsError || Boolean(historyData.warning) ? " error" : ""}" aria-live="polite" ${visibleHistoryNotice ? "" : "hidden"}>${escapeHtml(visibleHistoryNotice)}</p>
          </div>
          <div id="history-list" class="history-list list-viewport">${renderHistoryRecords()}</div>
        </div>
      </section>
      <footer>不支援字幕、跨重啟續傳、自動更新、登入、Cookie、CAPTCHA、DRM 或付費牆繞過。</footer>
      <dialog id="about-dialog" aria-labelledby="about-title" aria-describedby="about-description">
        <div class="about-card">
          <div class="about-heading">
            <div>
              <p class="eyebrow">ABOUT</p>
              <h2 id="about-title">Windows 影音下載工具</h2>
            </div>
            <button id="about-close" class="icon-button" type="button" aria-label="關閉關於視窗">關閉</button>
          </div>
          <div id="about-description" class="about-content">
            <p>作者：Y.J. Wang</p>
            <p>本工具以 Tauri 2、Rust 與 Vanilla TypeScript 建置；yt-dlp、FFmpeg、FFprobe sidecar 需通過固定 SHA-256 manifest 驗證。</p>
            <p>下載僅在精確允許的 HTTPS host、啟動前 DNS 驗證與本機處理邊界內進行，不使用遙測。</p>
            <p>小鴨影音與歐樂影院僅代表指定公開頁面的條件式驗證；站台變更時會安全拒絕，並非整站保證。超過 20 分鐘的完整基準目前尚未評估。</p>
          </div>
        </div>
      </dialog>
      <dialog id="history-clear-dialog" aria-labelledby="history-clear-title" aria-describedby="history-clear-description">
        <div class="about-card confirm-card">
          <div class="about-heading">
            <div>
              <p class="eyebrow">LOCAL HISTORY</p>
              <h2 id="history-clear-title">清除全部下載紀錄</h2>
            </div>
          </div>
          <p id="history-clear-description" class="about-content">這只會清除本機 SQLite 下載紀錄，不會刪除媒體檔，也不會清除工作狀態卡。</p>
          <div class="dialog-actions">
            <button id="history-clear-cancel" class="secondary" type="button">取消</button>
            <button id="history-clear-confirm" class="danger" type="button" ${historyBusy || busy ? "disabled" : ""}>清除全部歷史</button>
          </div>
        </div>
      </dialog>
    </section>`;
  renderRows();
  renderJobs();
  attachHandlers();
  restoreConsoleViewport();
  if (aboutWasOpen) {
    const dialog = document.querySelector<HTMLDialogElement>("#about-dialog");
    const trigger = document.querySelector<HTMLButtonElement>("#about");
    if (dialog && trigger) {
      aboutTrigger = trigger;
      dialog.showModal();
      document.querySelector<HTMLButtonElement>("#about-close")?.focus();
    }
  }
  if (historyClearWasOpen) {
    const dialog = document.querySelector<HTMLDialogElement>("#history-clear-dialog");
    const trigger = document.querySelector<HTMLButtonElement>("#history-clear");
    if (dialog && trigger) {
      historyClearTrigger = trigger;
      dialog.showModal();
      document.querySelector<HTMLButtonElement>("#history-clear-confirm")?.focus();
    }
  }
}

function renderRows(): void {
  const container = document.querySelector<HTMLDivElement>("#rows");
  if (!container) return;
  container.innerHTML = rows
    .map(
      (row, index) => `
      <div class="download-row" data-index="${index}">
        <label>平台
          <select data-field="platform" aria-label="第 ${index + 1} 筆平台" ${busy ? "disabled" : ""}>
            ${(["youtube", "youtube-music", "little-duck", "olevod", "facebook"] as Platform[])
              .map((platform) => `<option value="${platform}" ${platform === row.platform ? "selected" : ""}>${platformName(platform)}</option>`)
              .join("")}
          </select>
        </label>
        <label class="url-field">HTTPS URL
          <input data-field="url" type="url" value="${escapeAttribute(row.url)}" placeholder="https://…" autocomplete="off" spellcheck="false" ${busy ? "disabled" : ""} />
        </label>
        <label>模式
          <select data-field="mode" aria-label="第 ${index + 1} 筆模式" ${busy ? "disabled" : ""}>
            <option value="video" ${row.mode === "video" ? "selected" : ""}>Video</option>
            <option value="audio" ${row.mode === "audio" ? "selected" : ""}>Audio（MP3）</option>
          </select>
        </label>
        <button class="icon-button" data-remove="${index}" aria-label="移除第 ${index + 1} 筆" ${rows.length <= 1 || busy ? "disabled" : ""}>移除</button>
      </div>`,
    )
    .join("");
}

function renderJobs(): void {
  updateJobsBadge();
  const container = document.querySelector<HTMLDivElement>("#jobs");
  if (!container) return;
  if (!statuses.length) {
    container.innerHTML = '<p class="empty">尚未建立下載工作。</p>';
    return;
  }
  container.innerHTML = statuses
    .map((job) => {
      const active = job.state === "queued" || job.state === "running";
      const stateClass = jobStateClass(job.state);
      const progressPercent = Number.isFinite(job.progress_percent)
        ? Math.max(0, Math.min(100, job.progress_percent))
        : 0;
      return `
      <article class="job-card ${active ? "job-active" : "job-terminal"}">
        <div class="job-title"><span>${platformName(job.platform)} · ${modeName(job.mode)}</span><strong class="job-state ${stateClass}">${stateName(job.state)}</strong></div>
        <div class="job-url${active ? "" : " job-url-terminal"}" title="${escapeAttribute(job.url)}">${escapeHtml(job.url)}</div>
        ${active
          ? `<div class="progress-track" role="progressbar" aria-label="${escapeAttribute(stateName(job.state))}" aria-valuemin="0" aria-valuemax="100" aria-valuenow="${progressPercent}"><span style="width:${progressPercent}%"></span></div>
        <div class="job-meta"><span>${progressPercent.toFixed(1)}%</span><span>${formatSpeed(job.speed_bps)}</span><span>${formatEta(job.eta_seconds)}</span></div>`
          : `<div class="terminal-meta"><span>狀態：${escapeHtml(stateName(job.state))}</span>${renderTerminalTiming(job.id)}</div>`}
        ${job.output_path ? `<div class="success">輸出：${escapeHtml(job.output_path)}</div>` : ""}
        ${job.error ? `<div class="error">${escapeHtml(job.error)}</div>` : ""}
        ${active
          ? `<div class="job-actions"><button type="button" class="cancel-one" data-cancel="${escapeAttribute(job.id)}" aria-label="取消此工作">取消此工作</button></div>`
          : `<div class="job-actions"><button type="button" class="remove-finished" data-remove-finished="${escapeAttribute(job.id)}" aria-label="移除 ${escapeAttribute(platformName(job.platform))} 的已結束狀態">移除此狀態</button></div>`}
      </article>`;
    })
    .join("");
}

function updateJobsBadge(): void {
  const badge = document.querySelector<HTMLElement>("#jobs-badge");
  if (!badge) return;
  const count = badgeCount(statuses.length);
  badge.textContent = count;
  badge.setAttribute("aria-label", `追蹤工作數 ${count}`);
}

function renderTerminalTiming(jobId: string): string {
  if (historyData.warning || !Array.isArray(historyData.records)) {
    return "<span>時間紀錄同步中</span>";
  }
  const record = historyData.records.find((item) => item.job_id === jobId);
  if (!record) return "<span>時間紀錄同步中</span>";
  return `<span>結束：${escapeHtml(formatTimestamp(record.finished_at_ms))}</span><span>耗時：${escapeHtml(formatElapsed(record.elapsed_ms))}</span>`;
}

function renderHistoryRecords(): string {
  const records = Array.isArray(historyData.records) ? historyData.records.slice(0, 100) : [];
  if (!records.length) {
    return '<p class="empty">目前沒有下載紀錄。</p>';
  }
  return records
    .map((record) => {
      const state = record.terminal_status === "completed" || record.terminal_status === "failed" || record.terminal_status === "cancelled"
        ? record.terminal_status
        : "failed";
      return `
        <article class="history-record">
          <div class="history-record-heading">
            <strong>${escapeHtml(record.probe_title || "未提供標題")}</strong>
            <span class="history-state ${state}">${escapeHtml(historyStateName(record.terminal_status))}</span>
          </div>
          <div class="history-record-grid">
            <span>平台：${escapeHtml(platformName(record.platform))}</span>
            <span>模式：${escapeHtml(modeName(record.mode))}</span>
            <span class="history-source">來源：${escapeHtml(record.source_link)}</span>
            <span>開始：${escapeHtml(formatTimestamp(record.started_at_ms))}</span>
            <span>結束：${escapeHtml(formatTimestamp(record.finished_at_ms))}</span>
            <span>耗時：${escapeHtml(formatElapsed(record.elapsed_ms))}</span>
            ${record.output_filename ? `<span>檔案：${escapeHtml(record.output_filename)}</span>` : ""}
            ${record.error_code ? `<span>錯誤代碼：${escapeHtml(record.error_code)}</span>` : ""}
            ${record.error_summary ? `<span class="error">${escapeHtml(record.error_summary)}</span>` : ""}
          </div>
        </article>`;
    })
    .join("");
}

function historyStateName(state: string): string {
  return {
    completed: "完成",
    failed: "失敗",
    cancelled: "已取消",
  }[state] ?? "未知";
}

function safeCount(value: number): number {
  return Number.isFinite(value) && value >= 0 ? Math.floor(value) : 0;
}

function formatElapsed(value: number): string {
  const milliseconds = safeCount(value);
  const seconds = Math.floor(milliseconds / 1000);
  if (seconds < 60) return `${seconds} 秒`;
  const minutes = Math.floor(seconds / 60);
  const remainingSeconds = seconds % 60;
  if (minutes < 60) return `${minutes} 分 ${remainingSeconds} 秒`;
  const hours = Math.floor(minutes / 60);
  return `${hours} 小時 ${minutes % 60} 分 ${remainingSeconds} 秒`;
}

function formatTimestamp(value: number): string {
  if (!Number.isFinite(value) || value <= 0) return "—";
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return "—";
  return date.toLocaleString("zh-TW", { dateStyle: "medium", timeStyle: "short" });
}

function attachHandlers(): void {
  const tabs = document.querySelector<HTMLDivElement>(".console-tabs");
  tabs?.addEventListener("click", (event) => {
    if (!(event.target instanceof Element)) return;
    const tab = event.target.closest<HTMLButtonElement>("[role=\"tab\"]");
    if (!tab || !isConsoleTab(tab.dataset.consoleTab)) return;
    setConsoleTab(tab.dataset.consoleTab, true);
  });
  tabs?.addEventListener("keydown", (event) => {
    if (!(event.target instanceof HTMLButtonElement) || event.target.getAttribute("role") !== "tab") return;
    const tabButtons = Array.from(tabs.querySelectorAll<HTMLButtonElement>("[role=\"tab\"]"));
    const currentIndex = tabButtons.indexOf(event.target);
    if (currentIndex < 0) return;
    let nextIndex = currentIndex;
    if (event.key === "ArrowRight" || event.key === "ArrowDown") nextIndex = (currentIndex + 1) % tabButtons.length;
    if (event.key === "ArrowLeft" || event.key === "ArrowUp") nextIndex = (currentIndex - 1 + tabButtons.length) % tabButtons.length;
    if (event.key === "Home") nextIndex = 0;
    if (event.key === "End") nextIndex = tabButtons.length - 1;
    if (nextIndex === currentIndex) return;
    event.preventDefault();
    const nextTab = tabButtons[nextIndex];
    if (!nextTab) return;
    if (isConsoleTab(nextTab.dataset.consoleTab)) setConsoleTab(nextTab.dataset.consoleTab, true);
  });
  document.querySelector<HTMLButtonElement>("#about")?.addEventListener("click", () => {
    const trigger = document.querySelector<HTMLButtonElement>("#about");
    const dialog = document.querySelector<HTMLDialogElement>("#about-dialog");
    if (!trigger || !dialog) return;
    aboutTrigger = trigger;
    if (!dialog.open) dialog.showModal();
    document.querySelector<HTMLButtonElement>("#about-close")?.focus();
  });
  const aboutDialog = document.querySelector<HTMLDialogElement>("#about-dialog");
  aboutDialog?.addEventListener("click", (event) => {
    if (event.target === aboutDialog) aboutDialog.close();
  });
  aboutDialog?.addEventListener("cancel", (event) => {
    event.preventDefault();
    aboutDialog.close();
  });
  aboutDialog?.addEventListener("close", () => {
    aboutTrigger?.focus();
    aboutTrigger = null;
  });
  document.querySelector<HTMLButtonElement>("#about-close")?.addEventListener("click", () => {
    aboutDialog?.close();
  });
  document.querySelector<HTMLButtonElement>("#choose-output")?.addEventListener("click", () => void chooseOutputDirectory());
  document.querySelector<HTMLButtonElement>("#clear-finished")?.addEventListener("click", () => void clearFinishedStatuses());
  document.querySelector<HTMLButtonElement>("#add")?.addEventListener("click", () => {
    if (!busy && rows.length < MAX_ROWS) {
      rows.push(newRow());
      render();
    }
  });
  document.querySelector<HTMLButtonElement>("#start")?.addEventListener("click", () => void startDownloads());
  document.querySelector<HTMLButtonElement>("#probe")?.addEventListener("click", () => void probeFirst());
  document.querySelector<HTMLButtonElement>("#cancel-all")?.addEventListener("click", () => void cancelAll());
  const historyDialog = document.querySelector<HTMLDialogElement>("#history-clear-dialog");
  historyDialog?.addEventListener("click", (event) => {
    if (event.target === historyDialog) historyDialog.close();
  });
  historyDialog?.addEventListener("cancel", (event) => {
    event.preventDefault();
    historyDialog.close();
  });
  historyDialog?.addEventListener("close", () => {
    historyClearTrigger?.focus();
    historyClearTrigger = null;
  });
  document.querySelector<HTMLButtonElement>("#history-refresh")?.addEventListener("click", () => void refreshHistory());
  document.querySelector<HTMLButtonElement>("#history-clear")?.addEventListener("click", () => openHistoryClearDialog());
  document.querySelector<HTMLButtonElement>("#history-clear-cancel")?.addEventListener("click", () => historyDialog?.close());
  document.querySelector<HTMLButtonElement>("#history-clear-confirm")?.addEventListener("click", () => void confirmClearHistory());
  document.querySelectorAll<HTMLButtonElement>("[data-remove]").forEach((button) => {
    button.addEventListener("click", () => {
      const index = Number(button.dataset.remove);
      if (!busy && Number.isInteger(index) && rows.length > 1) {
        rows.splice(index, 1);
        render();
      }
    });
  });
  document.querySelectorAll<HTMLElement>("[data-field]").forEach((element) => {
    element.addEventListener("change", () => {
      if (busy) return;
      const row = element.closest<HTMLElement>("[data-index]");
      if (!row) return;
      const index = Number(row.dataset.index);
      const field = element.dataset.field;
      const value = (element as HTMLInputElement | HTMLSelectElement).value;
      if (!Number.isInteger(index) || !field) return;
      const target = rows[index];
      if (!target) return;
      if (field === "platform" || field === "mode" || field === "url") {
        if (field === "platform") target.platform = value as Platform;
        if (field === "mode") target.mode = value as Mode;
        if (field === "url") target.url = value;
      }
    });
  });
  document.querySelector<HTMLDivElement>("#jobs")?.addEventListener("click", (event) => {
    if (!(event.target instanceof Element)) return;
    const cancelButton = event.target.closest<HTMLButtonElement>("[data-cancel]");
    if (cancelButton) {
      void cancelOne(cancelButton.dataset.cancel ?? "");
      return;
    }
    const removeButton = event.target.closest<HTMLButtonElement>("[data-remove-finished]");
    if (removeButton) void removeFinishedStatus(removeButton.dataset.removeFinished ?? "");
  });
}

async function chooseOutputDirectory(): Promise<void> {
  if (busy) return;
  busy = true;
  outputNotice = "正在開啟 Windows 資料夾選擇器…";
  outputNoticeIsError = false;
  render();
  try {
    outputDirectory = await invoke<OutputDirectoryInfo>("pick_output_directory");
    outputNotice = outputDirectory.warning ?? "輸出目錄設定已更新。";
    outputNoticeIsError = false;
  } catch (error) {
    outputNotice = `輸出目錄未變更：${String(error)}`;
    outputNoticeIsError = true;
  } finally {
    busy = false;
    render();
  }
}

async function startDownloads(): Promise<void> {
  if (busy) return;
  const submittedDrafts = rows.map(cloneRequest);
  busy = true;
  render();
  try {
    const accepted = await invoke<DownloadStatus[]>("start_downloads", { requests: submittedDrafts });
    statuses = mergeStatuses(statuses, accepted);
    rows = [newRow()];
    setMessage("工作已加入排程；啟動前會再次執行 DNS 與 sidecar 驗證。", false);
  } catch (error) {
    setMessage(String(error), true);
  } finally {
    busy = false;
    render();
  }
}

async function probeFirst(): Promise<void> {
  if (busy) return;
  const row = rows[0];
  if (!row || !row.url.trim()) {
    setMessage("請先輸入第一筆 HTTPS URL。", true);
    return;
  }
  busy = true;
  render();
  try {
    const result = await invoke<{ title: string | null; duration_seconds: number | null; extractor: string | null }>("probe", { request: cloneRequest(row) });
    const duration = result.duration_seconds === null ? "未知長度" : `${Math.round(result.duration_seconds)} 秒`;
    setMessage(`Probe 成功：${result.title ?? "未提供標題"}（${duration}）`, false);
  } catch (error) {
    setMessage(`Probe 失敗：${String(error)}`, true);
  } finally {
    busy = false;
    render();
  }
}

async function cancelOne(id: string): Promise<void> {
  if (!id) return;
  try {
    await invoke("cancel_download", { id });
    await refreshJobs();
  } catch (error) {
    setMessage(String(error), true);
  }
}

async function cancelAll(): Promise<void> {
  try {
    await invoke("cancel_all_downloads");
    await refreshJobs();
  } catch (error) {
    setMessage(String(error), true);
  }
}

async function removeFinishedStatus(id: string): Promise<void> {
  if (!id || busy) return;
  busy = true;
  render();
  try {
    await invoke("remove_finished_download", { id });
    await refreshJobs();
    setMessage("已移除工作狀態卡；媒體檔與下載紀錄均未刪除。", false);
  } catch (error) {
    setMessage(String(error), true);
  } finally {
    busy = false;
    render();
  }
}

async function clearFinishedStatuses(): Promise<void> {
  if (busy) return;
  busy = true;
  render();
  try {
    const removed = await invoke<number>("clear_finished_downloads");
    await refreshJobs();
    setMessage(`已清除 ${safeCount(removed)} 筆已結束狀態卡；媒體檔與下載紀錄均未刪除。`, false);
  } catch (error) {
    setMessage(String(error), true);
  } finally {
    busy = false;
    render();
  }
}

function openHistoryClearDialog(): void {
  if (historyBusy || busy) return;
  const trigger = document.querySelector<HTMLButtonElement>("#history-clear");
  const dialog = document.querySelector<HTMLDialogElement>("#history-clear-dialog");
  if (!trigger || !dialog) return;
  historyClearTrigger = trigger;
  if (!dialog.open) dialog.showModal();
  document.querySelector<HTMLButtonElement>("#history-clear-confirm")?.focus();
}

async function confirmClearHistory(): Promise<void> {
  if (historyBusy) return;
  historyBusy = true;
  render();
  try {
    const result = await invoke<HistoryClearResult>("clear_download_history");
    if (result.warning) {
      historyNotice = result.warning;
      historyNoticeIsError = true;
    } else {
      historyNotice = `已清除 ${safeCount(result.removed)} 筆下載紀錄；媒體檔與工作狀態卡未受影響。`;
      historyNoticeIsError = false;
    }
    await refreshHistory(true);
    document.querySelector<HTMLDialogElement>("#history-clear-dialog")?.close();
  } catch {
    historyNotice = "下載紀錄未清除；媒體檔與工作狀態卡未受影響。";
    historyNoticeIsError = true;
  } finally {
    historyBusy = false;
    render();
  }
}

async function refreshJobs(): Promise<void> {
  try {
    statuses = await invoke<DownloadStatus[]>("list_downloads");
    renderJobs();
  } catch {
    // 啟動前端預覽模式時沒有 Tauri IPC，保持空狀態即可。
  }
}

async function refreshHistory(preserveNotice = false): Promise<void> {
  if (historyRefreshInFlight) return;
  historyRefreshInFlight = true;
  render();
  try {
    historyData = await invoke<HistoryList>("list_download_history");
    if (!preserveNotice) {
      historyNotice = historyData.warning ?? "";
      historyNoticeIsError = false;
    }
  } catch (error) {
    historyData = {
      records: [],
      stats: { total: 0, completed: 0, failed: 0, cancelled: 0, cumulative_elapsed_ms: 0 },
      warning: "下載紀錄目前無法使用；下載與已完成媒體不受影響",
      limit: 100,
    };
    if (!preserveNotice) {
      historyNotice = "下載紀錄未載入；下載與已完成媒體不受影響。";
      historyNoticeIsError = true;
    }
  } finally {
    historyRefreshInFlight = false;
    render();
  }
}

async function refreshSidecar(): Promise<void> {
  try {
    sidecarStatus = await invoke<SidecarStatus>("sidecar_status");
  } catch {
    sidecarStatus = { ready: false, message: "僅 Tauri 執行環境可用" };
  }
  applySidecarStatus();
}

async function refreshOutputDirectory(): Promise<void> {
  try {
    outputDirectory = await invoke<OutputDirectoryInfo>("get_output_directory");
    outputNotice = outputDirectory.warning ?? "";
    outputNoticeIsError = false;
  } catch (error) {
    outputDirectory = null;
    outputNotice = `無法取得輸出目錄：${String(error)}`;
    outputNoticeIsError = true;
  }
  render();
}

function applySidecarStatus(): void {
  const pill = document.querySelector<HTMLDivElement>("#sidecar");
  if (!pill) return;
  pill.textContent = sidecarLabel();
  pill.classList.toggle("ready", sidecarStatus.ready);
  pill.title = sidecarStatus.message;
}

function sidecarLabel(): string {
  if (sidecarStatus.ready) return "sidecar 已驗證";
  return sidecarStatus.message === "sidecar 檢查中…" ? "sidecar 檢查中…" : "sidecar 未就緒";
}

function setMessage(text: string, isError: boolean): void {
  messageText = text;
  messageIsError = isError;
  const element = document.querySelector<HTMLOutputElement>("#message");
  if (element) {
    element.textContent = text;
    element.hidden = !text;
    element.classList.toggle("error", isError);
  }
}

function escapeHtml(value: string): string {
  return value.replace(/[&<>"']/g, (character) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#039;" })[character] ?? character);
}

function escapeAttribute(value: string): string {
  return escapeHtml(value);
}

render();
void refreshSidecar();
void refreshOutputDirectory();
void refreshHistory();
window.setInterval(() => void refreshJobs(), 700);
window.setInterval(() => void refreshHistory(), 5000);
