mod error;
mod history;
mod manager;
mod output_directory;
mod paths;
mod platform;
mod process;
mod security;
mod sidecar;

use history::{HistoryClearResult, HistoryList, HistoryStore};
use manager::{DownloadManager, DownloadStatus, ProbeData};
use output_directory::{OutputDirectoryInfo, OutputDirectoryManager};
use security::{validate_url_syntax, DownloadRequest, Platform, ValidatedUrl};
use serde::Serialize;
use tauri::{Manager, State};

#[tauri::command]
fn validate_url(platform: Platform, url: String) -> Result<ValidatedUrl, String> {
    validate_url_syntax(platform, &url).map_err(|error| error.to_string())
}

#[tauri::command]
async fn start_downloads(
    app: tauri::AppHandle,
    manager: State<'_, DownloadManager>,
    history: State<'_, HistoryStore>,
    requests: Vec<DownloadRequest>,
) -> Result<Vec<DownloadStatus>, String> {
    let history = history.inner().clone();
    manager
        .start_batch(app, history, requests)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn probe(
    app: tauri::AppHandle,
    manager: State<'_, DownloadManager>,
    request: DownloadRequest,
) -> Result<ProbeData, String> {
    manager
        .probe(app, request)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn list_downloads(manager: State<'_, DownloadManager>) -> Result<Vec<DownloadStatus>, String> {
    manager.statuses().map_err(|error| error.to_string())
}

#[tauri::command]
fn cancel_download(
    id: String,
    manager: State<'_, DownloadManager>,
    history: State<'_, HistoryStore>,
) -> Result<(), String> {
    manager
        .cancel(&id, &history)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn cancel_all_downloads(
    manager: State<'_, DownloadManager>,
    history: State<'_, HistoryStore>,
) -> Result<(), String> {
    manager
        .cancel_all(&history)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn remove_finished_download(id: String, manager: State<'_, DownloadManager>) -> Result<(), String> {
    manager
        .remove_finished_download(&id)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn clear_finished_downloads(manager: State<'_, DownloadManager>) -> Result<usize, String> {
    manager
        .clear_finished_downloads()
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn list_download_history(history: State<'_, HistoryStore>) -> HistoryList {
    history.list()
}

#[tauri::command]
fn clear_download_history(history: State<'_, HistoryStore>) -> HistoryClearResult {
    history.clear()
}

#[tauri::command]
fn get_output_directory(
    app: tauri::AppHandle,
    output: State<'_, OutputDirectoryManager>,
) -> Result<OutputDirectoryInfo, String> {
    output.info(&app).map_err(|error| error.to_string())
}

#[tauri::command]
fn pick_output_directory(
    app: tauri::AppHandle,
    output: State<'_, OutputDirectoryManager>,
    manager: State<'_, DownloadManager>,
) -> Result<OutputDirectoryInfo, String> {
    output
        .pick_folder(&app, || manager.has_active_jobs())
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn sidecar_status(app: tauri::AppHandle) -> SidecarStatus {
    match sidecar::resolve_and_verify_from_app(&app) {
        Ok(_paths) => SidecarStatus {
            ready: true,
            message: "已驗證 yt-dlp、ffmpeg、ffprobe".to_string(),
        },
        Err(error) => SidecarStatus {
            ready: false,
            message: error.to_string(),
        },
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SidecarStatus {
    pub ready: bool,
    pub message: String,
}

#[tauri::command]
fn app_capabilities() -> AppCapabilities {
    AppCapabilities {
        max_batch: 5,
        max_concurrent: 3,
        supports_probe: true,
        supports_subtitles: false,
        supports_cross_restart_resume: false,
        supports_auto_update: false,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AppCapabilities {
    pub max_batch: usize,
    pub max_concurrent: usize,
    pub supports_probe: bool,
    pub supports_subtitles: bool,
    pub supports_cross_restart_resume: bool,
    pub supports_auto_update: bool,
}

pub fn run() {
    let app = tauri::Builder::default()
        .manage(DownloadManager::default())
        .manage(HistoryStore::default())
        .manage(OutputDirectoryManager::default())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            app.state::<HistoryStore>().initialize(app.handle());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            validate_url,
            probe,
            start_downloads,
            list_downloads,
            cancel_download,
            cancel_all_downloads,
            remove_finished_download,
            clear_finished_downloads,
            list_download_history,
            clear_download_history,
            get_output_directory,
            pick_output_directory,
            sidecar_status,
            app_capabilities
        ])
        .build(tauri::generate_context!())
        .expect("啟動 Tauri 應用程式失敗");
    app.run(|app_handle, event| {
        if matches!(event, tauri::RunEvent::ExitRequested { .. }) {
            // App Exit 使用 shutdown-only 路徑：先終止程序樹，再把所有
            // queued/running 狀態固定記為 cancelled；一般 UI cancel-all
            // 仍等待各 running child 真正結束後才轉終態。
            let _ = app_handle
                .state::<DownloadManager>()
                .shutdown(&app_handle.state::<HistoryStore>());
        }
    });
}
