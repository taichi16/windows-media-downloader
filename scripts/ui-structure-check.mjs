import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";

const mainPath = fileURLToPath(new URL("../src/main.ts", import.meta.url));
const stylesPath = fileURLToPath(new URL("../src/styles.css", import.meta.url));
const logoPath = fileURLToPath(new URL("../public/logo.svg", import.meta.url));
const [main, styles, logo] = await Promise.all([readFile(mainPath, "utf8"), readFile(stylesPath, "utf8"), readFile(logoPath, "utf8")]);

const checks = [
  [main.includes('role="tablist"'), "控制台有 tablist"],
  [main.includes('role="tab"') && main.includes('aria-controls="jobs-panel"') && main.includes('aria-controls="history-panel"'), "兩個 panel 具備 tab/aria-controls"],
  [main.includes('aria-selected="${activeConsoleTab === "jobs"}"') && main.includes('tabindex="${activeConsoleTab === "jobs" ? "0" : "-1"}"'), "tab 使用 active 狀態與 roving tabindex"],
  [main.includes("captureConsoleViewport()") && main.includes("restoreConsoleViewport()"), "render 前後保存清單 scrollTop"],
  [main.includes('event.target.closest<HTMLButtonElement>') && main.includes('data-console-tab'), "tab 使用事件委派"],
  [main.includes('id="jobs-badge"') && main.includes("function updateJobsBadge()") && main.includes("updateJobsBadge();"), "refreshJobs 路徑以 textContent 更新 jobs badge"],
  [main.includes("visibleScrollTop") && main.includes("closest<HTMLElement>('[role=\"tabpanel\"]')?.hidden"), "scrollTop 捕捉忽略 hidden tabpanel"],
  [main.includes('src="/logo.svg"') && main.includes('aria-hidden="true"'), "header 使用 decorative 本機 SVG logo"],
  [!/<script\b|<foreignObject\b|\b(?:xlink:)?href\s*=/.test(logo) && logo.includes("linearGradient") && logo.includes("viewBox="), "SVG logo 無 script/foreignObject/external href"],
  [main.includes('messageText ? "" : "hidden"') && main.includes("element.hidden = !text"), "空 message 以 hidden 移除高度"],
  [main.includes('id="output-path"') && main.includes('title="${escapeAttribute(visibleOutputPath)}"') && main.includes('aria-label="完成檔目錄：${escapeAttribute(visibleOutputPath)}"') && styles.includes("text-overflow: ellipsis"), "輸出路徑有 ellipsis 與完整可存取值"],
  [styles.includes("input, select { min-height: 44px; font-size: 16px; }"), "輸入與選單維持 16px 字級及 44px 觸控高度"],
  [styles.includes(".compact { min-height: 44px; }"), "hero/About compact 按鈕維持 44px 觸控高度"],
  [main.includes('data-cancel="${escapeAttribute(job.id)}"') && main.includes('data-remove-finished="${escapeAttribute(job.id)}"'), "active/terminal 工作操作仍由 data 屬性委派"],
  [main.includes('class="job-card ${active ? "job-active" : "job-terminal"}"'), "工作卡區分 active 與 compact terminal"],
  [main.includes("function renderTerminalTiming(jobId: string)") && main.includes("item.job_id === jobId") && main.includes("formatTimestamp(record.finished_at_ms)") && main.includes("時間紀錄同步中"), "terminal 卡片使用歷史時間且缺資料時不杜撰"],
  [styles.includes(".list-viewport") && styles.includes("min-height: 0") && styles.includes("overflow-y: auto") && styles.includes("height: clamp"), "清單 viewport 有 bounded scroll"],
  [styles.includes("@media (prefers-reduced-motion: reduce)"), "具備 reduced-motion 規則"],
];

for (const [condition, description] of checks) {
  assert.ok(condition, `UI 結構檢查失敗：${description}`);
}

console.log(`UI 結構 smoke check 通過（${checks.length} 項，未啟動網路或下載）。`);
