use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager as TauriManager};
use tokio::{
    io::AsyncRead,
    sync::{oneshot, OwnedSemaphorePermit, Semaphore},
};
use uuid::Uuid;

use crate::{
    error::AppError,
    history::HistoryStore,
    output_directory::{OutputDirectoryManager, OutputDirectorySnapshot},
    paths::DownloadPaths,
    platform::{self, ResolvedTarget, PLATFORM_USER_AGENT},
    process::{self, ProcessOutput},
    security::{validate_batch, validate_url_with_dns, DownloadMode, DownloadRequest},
    sidecar::{self, SidecarPaths},
};

const MAX_CONCURRENT: usize = 3;
const MAX_BATCH: usize = 5;
const MAX_TRACKED: usize = 50;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum JobState {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadStatus {
    pub id: String,
    pub platform: crate::security::Platform,
    pub mode: DownloadMode,
    pub url: String,
    pub state: JobState,
    pub progress_percent: f64,
    pub speed_bps: Option<f64>,
    pub eta_seconds: Option<u64>,
    pub output_path: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeData {
    pub title: Option<String>,
    pub duration_seconds: Option<f64>,
    pub extractor: Option<String>,
    pub webpage_url: Option<String>,
    pub availability: Option<String>,
    pub has_drm: Option<bool>,
}

const PROBE_PRINT_TEMPLATE: &str = r#"{"title":%(title|null)j,"duration":%(duration|null)j,"extractor":%(extractor|null)j,"webpage_url":%(webpage_url|null)j,"availability":%(availability|null)j,"has_drm":%(formats.:.{has_drm}|[])j}"#;

struct ManagerInner {
    jobs: Mutex<BTreeMap<String, DownloadStatus>>,
    cancel_tokens: Mutex<BTreeMap<String, Arc<AtomicBool>>>,
    process_ids: Mutex<BTreeMap<String, u32>>,
    job_contexts: Mutex<BTreeMap<String, JobContext>>,
    all_cancelled: AtomicBool,
    semaphore: Arc<Semaphore>,
    probe_semaphore: Arc<Semaphore>,
}

#[derive(Debug, Clone, Default)]
struct JobContext {
    started_at_ms: i64,
    probe_title: Option<String>,
}

#[derive(Debug, Clone)]
struct TerminalSnapshot {
    status: DownloadStatus,
    context: JobContext,
}

#[derive(Debug)]
struct ReservedJob {
    id: String,
    status: DownloadStatus,
    token: Arc<AtomicBool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RequestKey {
    platform: crate::security::Platform,
    url: String,
    mode: DownloadMode,
}

impl RequestKey {
    fn from_request(request: &DownloadRequest) -> Self {
        Self {
            platform: request.platform,
            url: request.url.clone(),
            mode: request.mode,
        }
    }

    fn from_status(status: &DownloadStatus) -> Self {
        Self {
            platform: status.platform,
            url: status.url.clone(),
            mode: status.mode,
        }
    }
}

#[derive(Clone)]
pub struct DownloadManager {
    inner: Arc<ManagerInner>,
}

impl Default for DownloadManager {
    fn default() -> Self {
        Self::new(MAX_CONCURRENT)
    }
}

impl DownloadManager {
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            inner: Arc::new(ManagerInner {
                jobs: Mutex::new(BTreeMap::new()),
                cancel_tokens: Mutex::new(BTreeMap::new()),
                process_ids: Mutex::new(BTreeMap::new()),
                job_contexts: Mutex::new(BTreeMap::new()),
                all_cancelled: AtomicBool::new(false),
                semaphore: Arc::new(Semaphore::new(max_concurrent.clamp(1, MAX_CONCURRENT))),
                probe_semaphore: Arc::new(Semaphore::new(1)),
            }),
        }
    }

    pub async fn start_batch(
        &self,
        app: AppHandle,
        history: HistoryStore,
        requests: Vec<DownloadRequest>,
    ) -> Result<Vec<DownloadStatus>, AppError> {
        // 新批次在任何 DNS／sidecar 前置檢查前清除全域旗標；既有工作的
        // 個別 token 不會被清除。若檢查期間再次 cancel-all，旗標會保持 true。
        self.reset_for_new_batch();
        let requests = requests
            .into_iter()
            .map(|mut request| {
                request.url =
                    crate::security::validate_url_syntax(request.platform, &request.url)?.url;
                Ok(request)
            })
            .collect::<Result<Vec<_>, AppError>>()?;
        validate_batch(&requests)?;
        if requests.len() > MAX_BATCH {
            return Err(AppError::InvalidInput("單批最多 5 筆下載工作".to_string()));
        }
        // DNS 驗證在程序建立前完成，避免把未驗證的主機交給 sidecar。
        for request in &requests {
            validate_url_with_dns(request.platform, &request.url)?;
        }
        let output_snapshot = app.state::<OutputDirectoryManager>().snapshot(&app)?;
        let _sidecars = sidecar::resolve_and_verify_from_app(&app)?;
        // resolver 已要求並驗證 yt-dlp、ffmpeg、ffprobe 三個 sidecar；
        // 因此 yt-dlp 不會退回 PATH 尋找未驗證工具。

        let reservations = self.reserve_jobs(&requests)?;
        let statuses = reservations
            .iter()
            .map(|reservation| reservation.status.clone())
            .collect::<Vec<_>>();
        for (request, reservation) in requests.into_iter().zip(reservations) {
            let ReservedJob { id, token, .. } = reservation;
            self.inner
                .cancel_tokens
                .lock()
                .map_err(lock_error)?
                .insert(id.clone(), token.clone());

            let manager = self.clone();
            let app_handle = app.clone();
            let output_snapshot = output_snapshot.clone();
            let history = history.clone();
            tauri::async_runtime::spawn(async move {
                manager
                    .run_job(app_handle, history, id, request, token, output_snapshot)
                    .await;
            });
        }
        Ok(statuses)
    }

    fn reserve_jobs(&self, requests: &[DownloadRequest]) -> Result<Vec<ReservedJob>, AppError> {
        let mut jobs = self.inner.jobs.lock().map_err(lock_error)?;
        let request_keys = requests
            .iter()
            .map(RequestKey::from_request)
            .collect::<Vec<_>>();
        for (index, key) in request_keys.iter().enumerate() {
            if request_keys[..index].contains(key) {
                return Err(AppError::InvalidInput(
                    "同一批不可重複提交相同平台、網址與模式的工作".to_string(),
                ));
            }
        }
        let active_keys = jobs
            .values()
            .filter(|status| matches!(status.state, JobState::Queued | JobState::Running))
            .map(RequestKey::from_status)
            .collect::<Vec<_>>();
        if request_keys.iter().any(|key| active_keys.contains(key)) {
            return Err(AppError::InvalidInput(
                "相同平台、網址與模式的工作已在排隊或執行中".to_string(),
            ));
        }
        let active = jobs
            .values()
            .filter(|status| matches!(status.state, JobState::Queued | JobState::Running))
            .count();
        if active.saturating_add(requests.len()) > MAX_BATCH {
            return Err(AppError::InvalidInput(
                "目前最多同時排程 5 筆工作；請等待既有工作完成或取消".to_string(),
            ));
        }

        if jobs.len().saturating_add(requests.len()) > MAX_TRACKED {
            jobs.retain(|_, status| matches!(status.state, JobState::Queued | JobState::Running));
        }

        // Context insertion is part of reservation, under the same status
        // lock, so queued cancellation cannot race before its timestamp is
        // available to the history recorder.
        let mut contexts = self.inner.job_contexts.lock().map_err(lock_error)?;
        let mut reservations = Vec::with_capacity(requests.len());
        for request in requests {
            let id = Uuid::new_v4().to_string();
            let status = DownloadStatus {
                id: id.clone(),
                platform: request.platform,
                mode: request.mode,
                url: request.url.clone(),
                state: JobState::Queued,
                progress_percent: 0.0,
                speed_bps: None,
                eta_seconds: None,
                output_path: None,
                error: None,
            };
            let token = Arc::new(AtomicBool::new(false));
            let context = JobContext {
                started_at_ms: now_ms(),
                probe_title: None,
            };
            jobs.insert(id.clone(), status.clone());
            contexts.insert(id.clone(), context.clone());
            reservations.push(ReservedJob { id, status, token });
        }
        Ok(reservations)
    }

    fn reset_for_new_batch(&self) {
        self.inner.all_cancelled.store(false, Ordering::Release);
    }

    pub fn statuses(&self) -> Result<Vec<DownloadStatus>, AppError> {
        Ok(self
            .inner
            .jobs
            .lock()
            .map_err(lock_error)?
            .values()
            .cloned()
            .collect())
    }

    pub fn has_active_jobs(&self) -> bool {
        self.inner
            .jobs
            .lock()
            .map(|jobs| {
                jobs.values()
                    .any(|status| matches!(status.state, JobState::Queued | JobState::Running))
            })
            .unwrap_or(true)
    }

    pub async fn probe(
        &self,
        app: AppHandle,
        request: crate::security::DownloadRequest,
    ) -> Result<ProbeData, AppError> {
        validate_url_with_dns(request.platform, &request.url)?;
        let target = platform::resolve_target(&request).await?;
        let paths = sidecar::resolve_and_verify_from_app(&app)?;
        let probe_id = Uuid::new_v4().to_string();
        self.run_probe_with_sidecar(Some(&probe_id), &paths.yt_dlp, &request, &target, None)
            .await
    }

    async fn run_probe_with_sidecar(
        &self,
        job_id: Option<&str>,
        yt_dlp: &Path,
        request: &DownloadRequest,
        target: &ResolvedTarget,
        cancellation: Option<Arc<AtomicBool>>,
    ) -> Result<ProbeData, AppError> {
        let _probe_permit = self
            .inner
            .probe_semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| AppError::Process("probe 排程器已關閉".to_string()))?;
        let args = build_probe_args(request, target)?;
        let mut child = process::command(yt_dlp, &args)?.spawn()?;
        if let (Some(job_id), Some(pid)) = (job_id, child.id()) {
            self.inner
                .process_ids
                .lock()
                .map_err(lock_error)?
                .insert(job_id.to_string(), pid);
        }
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                self.remove_process_id(job_id);
                return Err(AppError::Process("無法取得 probe stdout".to_string()));
            }
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                self.remove_process_id(job_id);
                return Err(AppError::Process("無法取得 probe stderr".to_string()));
            }
        };
        let stdout_task = tauri::async_runtime::spawn(process::collect_output(stdout));
        let stderr_task = tauri::async_runtime::spawn(process::collect_output(stderr));
        let wait_result = if let Some(cancellation) = cancellation {
            let (_cancel_sender, cancel_receiver) = oneshot::channel();
            tokio::time::timeout(
                Duration::from_secs(45),
                process::wait_or_cancel(&mut child, cancellation, cancel_receiver),
            )
            .await
        } else {
            tokio::time::timeout(Duration::from_secs(45), async {
                child.wait().await.map_err(AppError::from)
            })
            .await
        };
        let status = match wait_result {
            Ok(Ok(status)) => status,
            Ok(Err(error)) => {
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                self.remove_process_id(job_id);
                return Err(error);
            }
            Err(_) => {
                // timeout 後明確終止並 wait 回收 probe，避免 yt-dlp 留在背景。
                let _ = process::terminate_process_tree(&mut child);
                let _ = child.wait().await;
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                self.remove_process_id(job_id);
                return Err(AppError::Process("probe 逾時".to_string()));
            }
        };
        self.remove_process_id(job_id);
        let stdout = stdout_task
            .await
            .map_err(|error| AppError::Process(error.to_string()))??;
        let stderr = stderr_task
            .await
            .map_err(|error| AppError::Process(error.to_string()))??;
        if !status.success() {
            return Err(AppError::Process(format!(
                "probe 結束碼 {:?}：{}",
                status.code(),
                tail(&stderr, 2000)
            )));
        }
        parse_probe_metadata(&stdout)
    }

    fn remove_process_id(&self, job_id: Option<&str>) {
        if let Some(job_id) = job_id {
            if let Ok(mut process_ids) = self.inner.process_ids.lock() {
                process_ids.remove(job_id);
            }
        }
    }

    pub fn cancel(&self, id: &str, history: &HistoryStore) -> Result<(), AppError> {
        let token = self
            .inner
            .cancel_tokens
            .lock()
            .map_err(lock_error)?
            .get(id)
            .cloned()
            .ok_or_else(|| AppError::InvalidInput("找不到工作識別碼".to_string()))?;
        token.store(true, Ordering::Release);
        if let Some(pid) = self
            .inner
            .process_ids
            .lock()
            .map_err(lock_error)?
            .get(id)
            .copied()
        {
            #[cfg(windows)]
            let _ = process::terminate_pid_tree(pid);
        }
        let snapshot = {
            let mut jobs = self.inner.jobs.lock().map_err(lock_error)?;
            if let Some(status) = jobs.get_mut(id) {
                if matches!(status.state, JobState::Queued) {
                    status.state = JobState::Cancelled;
                    Some(self.terminal_snapshot_locked(status))
                } else {
                    None
                }
            } else {
                None
            }
        };
        if let Some(snapshot) = snapshot {
            self.record_snapshot(history, snapshot);
        }
        Ok(())
    }

    pub fn cancel_all(&self, history: &HistoryStore) -> Result<(), AppError> {
        self.inner.all_cancelled.store(true, Ordering::Release);
        for token in self
            .inner
            .cancel_tokens
            .lock()
            .map_err(lock_error)?
            .values()
        {
            token.store(true, Ordering::Release);
        }
        let process_ids = {
            let process_ids = self.inner.process_ids.lock().map_err(lock_error)?;
            process_ids_for_cancellation(&process_ids)
        };
        for pid in process_ids {
            #[cfg(windows)]
            let _ = process::terminate_pid_tree(pid);
        }
        let snapshots = {
            let mut jobs = self.inner.jobs.lock().map_err(lock_error)?;
            let mut snapshots = Vec::new();
            for status in jobs.values_mut() {
                if matches!(status.state, JobState::Queued) {
                    status.state = JobState::Cancelled;
                    snapshots.push(self.terminal_snapshot_locked(status));
                }
            }
            snapshots
        };
        for snapshot in snapshots {
            self.record_snapshot(history, snapshot);
        }
        Ok(())
    }

    /// Shutdown-only path.  Unlike the user-facing cancel-all command, this
    /// terminalizes every queued/running status after signalling all saved
    /// process trees.  Status/context snapshots are cloned while the jobs
    /// mutex is held, then history is written after unlocking.  Repeated
    /// shutdown notifications are safe because history insertion is keyed by
    /// job id and terminal states are not regressed.
    pub fn shutdown(&self, history: &HistoryStore) -> Result<(), AppError> {
        self.inner.all_cancelled.store(true, Ordering::Release);
        for token in self
            .inner
            .cancel_tokens
            .lock()
            .map_err(lock_error)?
            .values()
        {
            token.store(true, Ordering::Release);
        }
        let process_ids = {
            let process_ids = self.inner.process_ids.lock().map_err(lock_error)?;
            process_ids_for_cancellation(&process_ids)
        };
        for pid in process_ids {
            #[cfg(windows)]
            let _ = process::terminate_pid_tree(pid);
        }
        let snapshots = {
            let mut jobs = self.inner.jobs.lock().map_err(lock_error)?;
            let mut snapshots = Vec::new();
            for status in jobs.values_mut() {
                if matches!(status.state, JobState::Queued | JobState::Running) {
                    status.state = JobState::Cancelled;
                    snapshots.push(self.terminal_snapshot_locked(status));
                }
            }
            snapshots
        };
        for snapshot in snapshots {
            self.record_snapshot(history, snapshot);
        }
        Ok(())
    }

    /// Remove a terminal status card without touching its media output or the
    /// local history database.  The state check and removal share one lock so
    /// a concurrent cancellation/terminal transition cannot remove an active
    /// job accidentally.
    pub fn remove_finished_download(&self, id: &str) -> Result<(), AppError> {
        let mut jobs = self.inner.jobs.lock().map_err(lock_error)?;
        let Some(status) = jobs.get(id) else {
            return Err(AppError::InvalidInput("找不到工作識別碼".to_string()));
        };
        if matches!(status.state, JobState::Queued | JobState::Running) {
            return Err(AppError::InvalidInput(
                "工作仍在排隊或執行中，請先取消工作".to_string(),
            ));
        }
        jobs.remove(id);
        drop(jobs);
        self.remove_context(id);
        Ok(())
    }

    /// Remove all terminal status cards atomically with respect to state
    /// transitions.  Active jobs remain queued/running and are not cancelled.
    pub fn clear_finished_downloads(&self) -> Result<usize, AppError> {
        let mut jobs = self.inner.jobs.lock().map_err(lock_error)?;
        let removed_ids = jobs
            .iter()
            .filter(|(_, status)| {
                matches!(
                    status.state,
                    JobState::Completed | JobState::Failed | JobState::Cancelled
                )
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        jobs.retain(|_, status| matches!(status.state, JobState::Queued | JobState::Running));
        drop(jobs);
        if let Ok(mut contexts) = self.inner.job_contexts.lock() {
            for id in &removed_ids {
                contexts.remove(id);
            }
        }
        Ok(removed_ids.len())
    }

    async fn run_job(
        &self,
        app: AppHandle,
        history: HistoryStore,
        id: String,
        request: DownloadRequest,
        token: Arc<AtomicBool>,
        output_snapshot: OutputDirectorySnapshot,
    ) {
        let permit = match self.inner.semaphore.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => {
                self.finish_failed(&id, "下載排程器已關閉", &history);
                self.cleanup_job(&id);
                return;
            }
        };
        if self.is_cancelled(&token) {
            self.finish_state(&id, JobState::Cancelled, &history);
            self.cleanup_job(&id);
            drop(permit);
            return;
        }
        self.set_state(&id, JobState::Running);

        let result = self
            .run_job_inner(&app, &id, &request, &token, permit, &output_snapshot)
            .await;
        match result {
            Ok(path) if self.is_cancelled(&token) => {
                let _ = fs::remove_file(path);
                self.finish_state(&id, JobState::Cancelled, &history);
            }
            Ok(path) => match output_snapshot.user_visible_path(&path) {
                Ok(display_path) => self.finish_completed(&id, display_path, &history),
                Err(error) => self.finish_failed(&id, &error.to_string(), &history),
            },
            Err(AppError::Cancelled) => self.finish_state(&id, JobState::Cancelled, &history),
            Err(_) if self.is_cancelled(&token) => {
                self.finish_state(&id, JobState::Cancelled, &history)
            }
            Err(error) => self.finish_failed(&id, &error.to_string(), &history),
        }
        self.cleanup_job(&id);
    }

    async fn run_job_inner(
        &self,
        app: &AppHandle,
        id: &str,
        request: &DownloadRequest,
        token: &Arc<AtomicBool>,
        _permit: OwnedSemaphorePermit,
        output_snapshot: &OutputDirectorySnapshot,
    ) -> Result<PathBuf, AppError> {
        // semaphore 取得後再解析 DNS，避免長時間排隊期間的 DNS 變更繞過啟動前檢查。
        validate_url_with_dns(request.platform, &request.url)?;
        let sidecars = sidecar::resolve_and_verify_from_app(app)?;
        let paths = output_snapshot.download_paths_for_job()?;
        let job_dir = paths.job_dir(id)?;
        let result = async {
            let target = platform::resolve_target(request).await?;
            let probe = self
                .run_probe_with_sidecar(
                    Some(id),
                    &sidecars.yt_dlp,
                    request,
                    &target,
                    Some(token.clone()),
                )
                .await?;
            self.set_probe_title(id, probe.title);
            self.run_download_process(id, request, &sidecars, &target, token, &paths, &job_dir)
                .await
        }
        .await;
        if result.is_err() {
            let _ = fs::remove_dir_all(&job_dir);
        }
        result
    }

    // The arguments are independently security-scoped (request, verified target,
    // sidecars, cancellation and fixed paths); keep this boundary explicit.
    #[allow(clippy::too_many_arguments)]
    async fn run_download_process(
        &self,
        id: &str,
        request: &DownloadRequest,
        sidecars: &SidecarPaths,
        target: &ResolvedTarget,
        token: &Arc<AtomicBool>,
        paths: &DownloadPaths,
        job_dir: &Path,
    ) -> Result<PathBuf, AppError> {
        let args = build_ytdlp_args(request, job_dir, sidecars, target)?;
        let mut command = process::command(&sidecars.yt_dlp, &args)?;
        let mut child = command
            .spawn()
            .map_err(|error| AppError::Process(format!("無法啟動 yt-dlp：{error}")))?;
        if let Some(pid) = child.id() {
            self.inner
                .process_ids
                .lock()
                .map_err(lock_error)?
                .insert(id.to_string(), pid);
        }

        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                self.remove_process_id(Some(id));
                return Err(AppError::Process("無法取得 yt-dlp stdout".to_string()));
            }
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                self.remove_process_id(Some(id));
                return Err(AppError::Process("無法取得 yt-dlp stderr".to_string()));
            }
        };
        let progress_manager = self.clone();
        let stdout_job_id = id.to_string();
        let stdout_task = tauri::async_runtime::spawn(async move {
            progress_manager.collect_stream(stdout_job_id, stdout).await
        });
        let progress_manager = self.clone();
        let stderr_job_id = id.to_string();
        let stderr_task = tauri::async_runtime::spawn(async move {
            progress_manager.collect_stream(stderr_job_id, stderr).await
        });
        let (_cancel_sender, cancel_receiver) = oneshot::channel();
        let status =
            match process::wait_or_cancel(&mut child, self.cancel_flag(id, token), cancel_receiver)
                .await
            {
                Ok(status) => status,
                Err(error) => {
                    self.remove_process_id(Some(id));
                    let _ = stdout_task.await;
                    let _ = stderr_task.await;
                    return Err(error);
                }
            };
        self.remove_process_id(Some(id));
        let output = ProcessOutput {
            stdout: stdout_task
                .await
                .map_err(|error| AppError::Process(error.to_string()))??,
            stderr: stderr_task
                .await
                .map_err(|error| AppError::Process(error.to_string()))??,
        };
        if self.is_cancelled(token) {
            return Err(AppError::Cancelled);
        }
        if !status.success() {
            return Err(AppError::Process(format!(
                "yt-dlp 結束碼 {:?}：{}",
                status.code(),
                tail(&output.stderr, 2000)
            )));
        }
        let source = paths.find_completed_file(id)?;
        verify_media_with_ffprobe(self, id, &sidecars.ffprobe, &source, token).await?;
        paths.finalize(id)
    }

    async fn collect_stream<R: AsyncRead + Unpin>(
        &self,
        id: String,
        mut reader: R,
    ) -> Result<String, AppError> {
        use tokio::io::AsyncReadExt;
        let mut captured = Vec::with_capacity(process::MAX_CAPTURED_OUTPUT);
        let mut buffer = [0_u8; 8192];
        loop {
            let read = reader.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            let text = String::from_utf8_lossy(&buffer[..read]);
            if let Some(update) = parse_progress(&text) {
                self.update_progress(&id, update);
            }
            process::append_bounded(&mut captured, &buffer[..read]);
        }
        Ok(String::from_utf8_lossy(&captured).into_owned())
    }

    fn cancel_flag(&self, id: &str, token: &Arc<AtomicBool>) -> Arc<AtomicBool> {
        if self.inner.all_cancelled.load(Ordering::Acquire) {
            token.store(true, Ordering::Release);
        }
        let _ = id;
        token.clone()
    }

    fn is_cancelled(&self, token: &Arc<AtomicBool>) -> bool {
        token.load(Ordering::Acquire) || self.inner.all_cancelled.load(Ordering::Acquire)
    }

    fn set_state(&self, id: &str, state: JobState) {
        if let Ok(mut jobs) = self.inner.jobs.lock() {
            if let Some(status) = jobs.get_mut(id) {
                status.state = state;
            }
        }
    }

    fn finish_completed(&self, id: &str, output_path: String, history: &HistoryStore) {
        let snapshot = if let Ok(mut jobs) = self.inner.jobs.lock() {
            if let Some(status) = jobs.get_mut(id) {
                status.state = JobState::Completed;
                status.progress_percent = 100.0;
                status.output_path = Some(output_path);
                Some(self.terminal_snapshot_locked(status))
            } else {
                None
            }
        } else {
            None
        };
        if let Some(snapshot) = snapshot {
            self.record_snapshot(history, snapshot);
        }
    }

    fn finish_failed(&self, id: &str, message: &str, history: &HistoryStore) {
        let snapshot = if let Ok(mut jobs) = self.inner.jobs.lock() {
            if let Some(status) = jobs.get_mut(id) {
                status.state = JobState::Failed;
                status.error = Some(message.chars().take(2000).collect());
                Some(self.terminal_snapshot_locked(status))
            } else {
                None
            }
        } else {
            None
        };
        if let Some(snapshot) = snapshot {
            self.record_snapshot(history, snapshot);
        }
    }

    fn finish_state(&self, id: &str, state: JobState, history: &HistoryStore) {
        let snapshot = if let Ok(mut jobs) = self.inner.jobs.lock() {
            if let Some(status) = jobs.get_mut(id) {
                status.state = state;
                Some(self.terminal_snapshot_locked(status))
            } else {
                None
            }
        } else {
            None
        };
        if let Some(snapshot) = snapshot {
            self.record_snapshot(history, snapshot);
        }
    }

    /// Clone the terminal status and its reservation context while the jobs
    /// lock is held.  The SQLite write itself happens after releasing all
    /// manager locks so a slow/locked history database cannot stall status
    /// transitions, cancellation, or card cleanup.
    fn terminal_snapshot_locked(&self, status: &DownloadStatus) -> TerminalSnapshot {
        let context = self
            .inner
            .job_contexts
            .lock()
            .ok()
            .and_then(|contexts| contexts.get(&status.id).cloned())
            .unwrap_or_default();
        TerminalSnapshot {
            status: status.clone(),
            context,
        }
    }

    fn record_snapshot(&self, history: &HistoryStore, snapshot: TerminalSnapshot) {
        history.record_terminal(
            &snapshot.status,
            snapshot.context.probe_title.as_deref(),
            snapshot.context.started_at_ms,
        );
    }

    fn set_probe_title(&self, id: &str, title: Option<String>) {
        if let Ok(mut contexts) = self.inner.job_contexts.lock() {
            if let Some(context) = contexts.get_mut(id) {
                context.probe_title = title;
            }
        }
    }

    fn remove_context(&self, id: &str) {
        if let Ok(mut contexts) = self.inner.job_contexts.lock() {
            contexts.remove(id);
        }
    }

    fn update_progress(&self, id: &str, update: ProgressUpdate) {
        if let Ok(mut jobs) = self.inner.jobs.lock() {
            if let Some(status) = jobs.get_mut(id) {
                if let Some(value) = update.percent {
                    status.progress_percent = value.clamp(0.0, 100.0);
                }
                if let Some(value) = update.speed_bps {
                    status.speed_bps = Some(value);
                }
                if let Some(value) = update.eta_seconds {
                    status.eta_seconds = Some(value);
                }
            }
        }
    }

    fn cleanup_job(&self, id: &str) {
        if let Ok(mut tokens) = self.inner.cancel_tokens.lock() {
            tokens.remove(id);
        }
        if let Ok(mut process_ids) = self.inner.process_ids.lock() {
            process_ids.remove(id);
        }
        self.remove_context(id);
    }
}

#[derive(Debug, Default)]
struct ProgressUpdate {
    percent: Option<f64>,
    speed_bps: Option<f64>,
    eta_seconds: Option<u64>,
}

pub fn build_ytdlp_args(
    request: &DownloadRequest,
    job_dir: &Path,
    sidecars: &SidecarPaths,
    target: &ResolvedTarget,
) -> Result<Vec<String>, AppError> {
    if target.platform() != request.platform {
        return Err(AppError::Security(
            "verified target 平台與下載請求不一致".to_string(),
        ));
    }
    if !job_dir.is_absolute() {
        return Err(AppError::Security("暫存路徑必須是絕對路徑".to_string()));
    }
    let ffmpeg = &sidecars.ffmpeg;
    let output_template = job_dir.join("download.%(ext)s");
    let mut args = vec![
        "--ignore-config".to_string(),
        "--no-plugin-dirs".to_string(),
        "--no-js-runtimes".to_string(),
        "--no-remote-components".to_string(),
        "--compat-options".to_string(),
        "no-certifi".to_string(),
        "--no-playlist".to_string(),
        "--newline".to_string(),
        "--progress".to_string(),
        "--restrict-filenames".to_string(),
        "--windows-filenames".to_string(),
        "--ffmpeg-location".to_string(),
        ffmpeg
            .parent()
            .ok_or_else(|| AppError::Sidecar("ffmpeg 路徑缺少父目錄".to_string()))?
            .to_string_lossy()
            .into_owned(),
        "--output".to_string(),
        output_template.to_string_lossy().into_owned(),
    ];
    append_platform_headers(&mut args, request.platform);
    append_platform_download_options(&mut args, request.platform);
    if matches!(request.mode, DownloadMode::Audio) {
        args.extend([
            "--extract-audio".to_string(),
            "--audio-format".to_string(),
            "mp3".to_string(),
            "--audio-quality".to_string(),
            "0".to_string(),
        ]);
    } else {
        args.extend([
            "--format".to_string(),
            "bestvideo*+bestaudio/best".to_string(),
            "--merge-output-format".to_string(),
            "mp4".to_string(),
        ]);
    }
    // `--` 使 URL 永遠是位置參數；URL 也已在 security 模組通過 HTTPS/host 檢查。
    args.push("--".to_string());
    args.push(target.url().to_string());
    Ok(args)
}

pub fn build_probe_args(
    request: &DownloadRequest,
    target: &ResolvedTarget,
) -> Result<Vec<String>, AppError> {
    // Probe 明確使用 simulate/metadata，絕不建立輸出檔；不接受前端附加參數。
    if target.platform() != request.platform {
        return Err(AppError::Security(
            "verified target 平台與 probe 請求不一致".to_string(),
        ));
    }
    let mut args = vec![
        "--ignore-config".to_string(),
        "--no-plugin-dirs".to_string(),
        "--no-js-runtimes".to_string(),
        "--no-remote-components".to_string(),
        "--compat-options".to_string(),
        "no-certifi".to_string(),
        "--no-playlist".to_string(),
        "--simulate".to_string(),
        "--skip-download".to_string(),
        "--print".to_string(),
        PROBE_PRINT_TEMPLATE.to_string(),
        "--no-warnings".to_string(),
        "--no-progress".to_string(),
    ];
    append_platform_headers(&mut args, request.platform);
    args.extend(["--".to_string(), target.url().to_string()]);
    Ok(args)
}

fn append_platform_headers(args: &mut Vec<String>, platform: crate::security::Platform) {
    if matches!(
        platform,
        crate::security::Platform::LittleDuck | crate::security::Platform::Olevod
    ) {
        args.extend([
            "--add-headers".to_string(),
            format!("User-Agent:{PLATFORM_USER_AGENT}"),
        ]);
    }
}

fn append_platform_download_options(args: &mut Vec<String>, platform: crate::security::Platform) {
    if matches!(
        platform,
        crate::security::Platform::LittleDuck | crate::security::Platform::Olevod
    ) {
        args.extend(["--concurrent-fragments".to_string(), "4".to_string()]);
    }
}

/// 解析固定 `--print` JSON template 的最小 metadata；不接受完整 info JSON，
/// 以免標題/描述/格式清單等大型或惡意欄位耗盡 IPC buffer。
pub fn parse_probe_metadata(stdout: &str) -> Result<ProbeData, AppError> {
    if stdout.len() >= process::MAX_CAPTURED_OUTPUT {
        return Err(AppError::Process(
            "probe metadata 超過 64 KiB 限制，已拒絕".to_string(),
        ));
    }
    let json = match serde_json::from_str::<serde_json::Value>(stdout.trim()) {
        Ok(value) => value,
        Err(_) => stdout
            .lines()
            .rev()
            .find_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .ok_or_else(|| AppError::Process("probe 未回傳有效最小 metadata JSON".to_string()))?,
    };
    if !json.is_object() {
        return Err(AppError::Process(
            "probe metadata 必須是 JSON object".to_string(),
        ));
    }
    let availability = json
        .get("availability")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let has_drm = json.get("has_drm").and_then(normalize_drm_marker);
    if has_drm == Some(true) {
        return Err(AppError::Security("內容標記為 DRM，拒絕下載".to_string()));
    }
    match availability.as_deref() {
        None | Some("public" | "unlisted") => {}
        Some(_) => {
            return Err(AppError::Security(
                "內容不是明確 public/unlisted，可能需要登入、付費或不可用，拒絕下載".to_string(),
            ));
        }
    }
    Ok(ProbeData {
        title: json
            .get("title")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        duration_seconds: json.get("duration").and_then(serde_json::Value::as_f64),
        extractor: json
            .get("extractor")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        webpage_url: json
            .get("webpage_url")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        availability,
        has_drm,
    })
}

fn normalize_drm_marker(value: &serde_json::Value) -> Option<bool> {
    match value {
        serde_json::Value::Array(values) => {
            let mut saw_false = false;
            for value in values {
                match normalize_drm_marker(value) {
                    Some(true) => return Some(true),
                    Some(false) => saw_false = true,
                    None => {}
                }
            }
            saw_false.then_some(false)
        }
        serde_json::Value::Object(values) => values.get("has_drm").and_then(normalize_drm_marker),
        serde_json::Value::Bool(value) => Some(*value),
        serde_json::Value::String(value) if value.eq_ignore_ascii_case("true") => Some(true),
        serde_json::Value::String(value) if value.eq_ignore_ascii_case("maybe") => Some(true),
        serde_json::Value::String(value) if value.eq_ignore_ascii_case("false") => Some(false),
        _ => None,
    }
}

async fn verify_media_with_ffprobe(
    manager: &DownloadManager,
    id: &str,
    ffprobe: &Path,
    media: &Path,
    token: &Arc<AtomicBool>,
) -> Result<(), AppError> {
    let args = vec![
        "-v".to_string(),
        "error".to_string(),
        "-show_entries".to_string(),
        "format=duration".to_string(),
        "-of".to_string(),
        "default=noprint_wrappers=1:nokey=1".to_string(),
        media.to_string_lossy().into_owned(),
    ];
    let mut child = process::command(ffprobe, &args)?.spawn()?;
    if let Some(pid) = child.id() {
        manager
            .inner
            .process_ids
            .lock()
            .map_err(lock_error)?
            .insert(id.to_string(), pid);
    }
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            manager.remove_process_id(Some(id));
            return Err(AppError::Process("無法取得 ffprobe stdout".to_string()));
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            manager.remove_process_id(Some(id));
            return Err(AppError::Process("無法取得 ffprobe stderr".to_string()));
        }
    };
    let stdout_task = tauri::async_runtime::spawn(process::collect_output(stdout));
    let stderr_task = tauri::async_runtime::spawn(process::collect_output(stderr));
    let (_cancel_sender, cancel_receiver) = oneshot::channel();
    let status = match tokio::time::timeout(
        Duration::from_secs(30),
        process::wait_or_cancel(&mut child, manager.cancel_flag(id, token), cancel_receiver),
    )
    .await
    {
        Ok(result) => match result {
            Ok(status) => status,
            Err(error) => {
                manager.remove_process_id(Some(id));
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                return Err(error);
            }
        },
        Err(_) => {
            // timeout 後明確終止並 wait 回收 ffprobe，避免留下孤兒程序。
            let _ = process::terminate_process_tree(&mut child);
            let _ = child.wait().await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            manager.remove_process_id(Some(id));
            return Err(AppError::Process("ffprobe 驗證逾時".to_string()));
        }
    };
    manager.remove_process_id(Some(id));
    let output = ProcessOutput {
        stdout: stdout_task
            .await
            .map_err(|err| AppError::Process(err.to_string()))??,
        stderr: stderr_task
            .await
            .map_err(|err| AppError::Process(err.to_string()))??,
    };
    if manager.is_cancelled(token) {
        return Err(AppError::Cancelled);
    }
    let duration = parse_positive_duration(&output.stdout);
    if !status.success() || duration.is_none() {
        return Err(AppError::Process(format!(
            "ffprobe 驗證失敗：{}",
            tail(&output.stderr, 1000)
        )));
    }
    Ok(())
}

fn parse_positive_duration(value: &str) -> Option<f64> {
    value
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|duration| duration.is_finite() && *duration > 0.0)
}

fn parse_progress(text: &str) -> Option<ProgressUpdate> {
    let line = text.lines().last().unwrap_or(text);
    let mut update = ProgressUpdate::default();
    for token in line.split_whitespace() {
        if let Some(percent) = token.strip_suffix('%') {
            if let Ok(value) = percent.parse::<f64>() {
                if value.is_finite() {
                    update.percent = Some(value);
                }
            }
        }
        if let Some(speed) = token.strip_suffix("/s") {
            update.speed_bps = parse_rate(speed);
        }
    }
    if let Some(index) = line.find("ETA ") {
        update.eta_seconds = parse_eta(&line[index + 4..]);
    }
    if update.percent.is_some() || update.speed_bps.is_some() || update.eta_seconds.is_some() {
        Some(update)
    } else {
        None
    }
}

fn parse_rate(value: &str) -> Option<f64> {
    let (number, multiplier) = if let Some(value) = value.strip_suffix("KiB") {
        (value, 1024.0)
    } else if let Some(value) = value.strip_suffix("MiB") {
        (value, 1024.0 * 1024.0)
    } else if let Some(value) = value.strip_suffix("GiB") {
        (value, 1024.0 * 1024.0 * 1024.0)
    } else if let Some(value) = value.strip_suffix('K') {
        (value, 1000.0)
    } else if let Some(value) = value.strip_suffix('M') {
        (value, 1_000_000.0)
    } else {
        (value, 1.0)
    };
    let value = number.parse::<f64>().ok()? * multiplier;
    (value.is_finite() && value >= 0.0).then_some(value)
}

fn parse_eta(value: &str) -> Option<u64> {
    let value = value.split_whitespace().next()?;
    let mut pieces = value.split(':').rev();
    let seconds = pieces.next()?.parse::<u64>().ok()?;
    let minutes = pieces.next().unwrap_or("0").parse::<u64>().ok()?;
    let hours = pieces.next().unwrap_or("0").parse::<u64>().ok()?;
    Some(hours.saturating_mul(3600) + minutes.saturating_mul(60) + seconds)
}

fn tail(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars().rev().take(max_chars).collect::<Vec<_>>();
    chars.reverse();
    chars.into_iter().collect()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn lock_error<T>(_: std::sync::PoisonError<T>) -> AppError {
    AppError::Internal("工作狀態鎖已損壞".to_string())
}

fn process_ids_for_cancellation(process_ids: &BTreeMap<String, u32>) -> Vec<u32> {
    process_ids.values().copied().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::{DownloadMode, Platform};

    fn request(url: &str) -> DownloadRequest {
        DownloadRequest {
            platform: Platform::Youtube,
            url: url.to_string(),
            mode: DownloadMode::Video,
        }
    }

    fn sidecars() -> SidecarPaths {
        SidecarPaths {
            yt_dlp: PathBuf::from(r"C:\bundle\yt-dlp.exe"),
            ffmpeg: PathBuf::from(r"C:\bundle\ffmpeg.exe"),
            ffprobe: PathBuf::from(r"C:\bundle\ffprobe.exe"),
        }
    }

    fn target(url: &str) -> ResolvedTarget {
        crate::platform::target_for_test(Platform::Youtube, url)
    }

    fn request_for(platform: Platform, url: &str) -> DownloadRequest {
        DownloadRequest {
            platform,
            url: url.to_string(),
            mode: DownloadMode::Video,
        }
    }

    fn target_for(platform: Platform, url: &str) -> ResolvedTarget {
        crate::platform::target_for_test(platform, url)
    }

    fn status_for(id: &str, state: JobState) -> DownloadStatus {
        DownloadStatus {
            id: id.to_string(),
            platform: Platform::Youtube,
            mode: DownloadMode::Video,
            url: format!("https://www.youtube.com/watch?v={id}"),
            state,
            progress_percent: 0.0,
            speed_bps: None,
            eta_seconds: None,
            output_path: None,
            error: None,
        }
    }

    #[test]
    fn remove_finished_download_rejects_active_and_keeps_media_untouched() {
        let manager = DownloadManager::new(1);
        {
            let mut jobs = manager.inner.jobs.lock().expect("jobs lock");
            jobs.insert(
                "running".to_string(),
                status_for("running", JobState::Running),
            );
            jobs.insert(
                "finished".to_string(),
                DownloadStatus {
                    output_path: Some("C:\\Downloads\\finished.mp4".to_string()),
                    ..status_for("finished", JobState::Completed)
                },
            );
        }
        assert!(manager.remove_finished_download("running").is_err());
        manager
            .remove_finished_download("finished")
            .expect("terminal card can be removed");
        let jobs = manager.inner.jobs.lock().expect("jobs lock");
        assert!(jobs.contains_key("running"));
        assert!(!jobs.contains_key("finished"));
    }

    #[test]
    fn clear_finished_downloads_retains_active_jobs_and_returns_count() {
        let manager = DownloadManager::new(1);
        {
            let mut jobs = manager.inner.jobs.lock().expect("jobs lock");
            jobs.insert("queued".to_string(), status_for("queued", JobState::Queued));
            jobs.insert(
                "completed".to_string(),
                status_for("completed", JobState::Completed),
            );
            jobs.insert("failed".to_string(), status_for("failed", JobState::Failed));
            jobs.insert(
                "cancelled".to_string(),
                status_for("cancelled", JobState::Cancelled),
            );
        }
        assert_eq!(manager.clear_finished_downloads().expect("clear cards"), 3);
        let jobs = manager.inner.jobs.lock().expect("jobs lock");
        assert_eq!(jobs.len(), 1);
        assert!(jobs.contains_key("queued"));
    }

    #[test]
    fn concurrent_terminal_card_removal_is_atomic() {
        let manager = DownloadManager::new(1);
        manager.inner.jobs.lock().expect("jobs lock").insert(
            "terminal".to_string(),
            status_for("terminal", JobState::Failed),
        );
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let first_manager = manager.clone();
        let first_barrier = barrier.clone();
        let first = std::thread::spawn(move || {
            first_barrier.wait();
            first_manager.remove_finished_download("terminal").is_ok()
        });
        let second_manager = manager.clone();
        let second = std::thread::spawn(move || {
            barrier.wait();
            second_manager.remove_finished_download("terminal").is_ok()
        });
        let successes = [
            first.join().expect("first removal"),
            second.join().expect("second removal"),
        ]
        .into_iter()
        .filter(|success| *success)
        .count();
        assert_eq!(successes, 1);
        assert!(manager.inner.jobs.lock().expect("jobs lock").is_empty());
    }

    #[test]
    fn queued_cancel_records_history_without_waiting_for_semaphore() {
        let history_path =
            std::env::temp_dir().join(format!("wmd-manager-history-{}.sqlite3", Uuid::new_v4()));
        let history = HistoryStore::for_path(&history_path);
        let manager = DownloadManager::new(1);
        let reservation = manager
            .reserve_jobs(&[request("https://www.youtube.com/watch?v=queued-cancel")])
            .expect("reserve queued job")
            .pop()
            .expect("reservation");
        manager
            .inner
            .cancel_tokens
            .lock()
            .expect("token lock")
            .insert(reservation.id.clone(), reservation.token);
        manager
            .cancel(&reservation.id, &history)
            .expect("cancel queued job");
        let listed = history.list();
        assert_eq!(listed.stats.cancelled, 1);
        assert_eq!(listed.records[0].terminal_status, "cancelled");
        let _ = std::fs::remove_file(history_path);
    }

    #[test]
    fn locked_history_write_does_not_hold_jobs_mutex() {
        let history_path = std::env::temp_dir().join(format!(
            "wmd-manager-history-lock-{}.sqlite3",
            Uuid::new_v4()
        ));
        let history = HistoryStore::for_path(&history_path);
        let locker = rusqlite::Connection::open(&history_path).expect("history locker");
        locker
            .execute_batch("BEGIN EXCLUSIVE")
            .expect("exclusive history lock");
        let manager = DownloadManager::new(1);
        manager.inner.jobs.lock().expect("jobs lock").insert(
            "blocked-history".to_string(),
            status_for("blocked-history", JobState::Queued),
        );
        manager
            .inner
            .job_contexts
            .lock()
            .expect("context lock")
            .insert(
                "blocked-history".to_string(),
                JobContext {
                    started_at_ms: now_ms(),
                    probe_title: None,
                },
            );

        let (finished_sender, finished_receiver) = std::sync::mpsc::channel();
        let worker_manager = manager.clone();
        let worker_history = history.clone();
        let worker = std::thread::spawn(move || {
            worker_manager.finish_failed("blocked-history", "download failed", &worker_history);
            finished_sender.send(()).expect("worker completion");
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut observed_terminal = false;
        while std::time::Instant::now() < deadline {
            if let Ok(jobs) = manager.inner.jobs.try_lock() {
                if jobs
                    .get("blocked-history")
                    .is_some_and(|status| status.state == JobState::Failed)
                {
                    observed_terminal = true;
                    break;
                }
            }
            std::thread::yield_now();
        }
        assert!(observed_terminal, "state update should not wait for SQLite");
        drop(locker);
        finished_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("history worker should finish after unlock");
        worker.join().expect("history worker");
        drop(history);
        let _ = std::fs::remove_file(history_path);
    }

    #[test]
    fn shutdown_terminalizes_active_jobs_and_persists_idempotently() {
        let history_path =
            std::env::temp_dir().join(format!("wmd-manager-shutdown-{}.sqlite3", Uuid::new_v4()));
        let history = HistoryStore::for_path(&history_path);
        let manager = DownloadManager::new(1);
        {
            let mut jobs = manager.inner.jobs.lock().expect("jobs lock");
            jobs.insert(
                "shutdown-queued".to_string(),
                status_for("shutdown-queued", JobState::Queued),
            );
            jobs.insert(
                "shutdown-running".to_string(),
                status_for("shutdown-running", JobState::Running),
            );
        }
        {
            let mut contexts = manager.inner.job_contexts.lock().expect("context lock");
            contexts.insert(
                "shutdown-queued".to_string(),
                JobContext {
                    started_at_ms: now_ms(),
                    probe_title: Some("queued title".to_string()),
                },
            );
            contexts.insert(
                "shutdown-running".to_string(),
                JobContext {
                    started_at_ms: now_ms(),
                    probe_title: Some("running title".to_string()),
                },
            );
        }
        manager.shutdown(&history).expect("first shutdown");
        assert!(manager
            .statuses()
            .expect("statuses")
            .iter()
            .all(|status| status.state == JobState::Cancelled));
        let first = history.list();
        assert_eq!(first.stats.total, 2);
        assert_eq!(first.stats.cancelled, 2);

        manager.shutdown(&history).expect("repeated shutdown");
        let second = history.list();
        assert_eq!(second.stats.total, 2);
        assert_eq!(second.stats.cancelled, 2);
        drop(history);
        let _ = std::fs::remove_file(history_path);
    }

    #[test]
    fn reservation_rejects_active_plus_new_over_five_without_inserting() {
        let manager = DownloadManager::new(1);
        {
            let mut jobs = manager.inner.jobs.lock().expect("jobs lock");
            for index in 0..4 {
                let id = format!("active-{index}");
                jobs.insert(
                    id.clone(),
                    DownloadStatus {
                        id,
                        platform: Platform::Youtube,
                        mode: DownloadMode::Video,
                        url: "https://www.youtube.com/watch?v=x".to_string(),
                        state: JobState::Running,
                        progress_percent: 0.0,
                        speed_bps: None,
                        eta_seconds: None,
                        output_path: None,
                        error: None,
                    },
                );
            }
        }
        let requests = vec![
            request("https://www.youtube.com/watch?v=a"),
            request("https://www.youtube.com/watch?v=b"),
        ];
        let error = manager
            .reserve_jobs(&requests)
            .expect_err("4 active plus 2 new jobs must exceed capacity");
        assert!(error.to_string().contains("最多同時排程 5 筆"));
        assert_eq!(manager.inner.jobs.lock().expect("jobs lock").len(), 4);
    }

    #[test]
    fn reservation_rejects_duplicate_keys_before_any_insert() {
        let manager = DownloadManager::new(1);
        let sentinel = "https://www.youtube.com/watch?v=sentinel-duplicate-secret";
        let first = request(sentinel);
        let second = first.clone();
        let error = manager
            .reserve_jobs(&[first, second])
            .expect_err("duplicate batch must be rejected");
        assert!(!error.to_string().contains(sentinel));
        assert!(manager.inner.jobs.lock().expect("jobs lock").is_empty());
    }

    #[test]
    fn reservation_rejects_duplicate_active_key_but_allows_terminal_history() {
        let manager = DownloadManager::new(1);
        let sentinel = "https://www.youtube.com/watch?v=sentinel-active-secret";
        let active = request(sentinel);
        manager
            .reserve_jobs(std::slice::from_ref(&active))
            .expect("reserve active");
        let error = manager
            .reserve_jobs(std::slice::from_ref(&active))
            .expect_err("active duplicate must be rejected");
        assert!(!error.to_string().contains(sentinel));

        let terminal_manager = DownloadManager::new(1);
        let terminal = request("https://www.youtube.com/watch?v=terminal");
        {
            let mut jobs = terminal_manager.inner.jobs.lock().expect("jobs lock");
            for (index, state) in [JobState::Completed, JobState::Failed, JobState::Cancelled]
                .into_iter()
                .enumerate()
            {
                jobs.insert(
                    format!("terminal-{index}"),
                    DownloadStatus {
                        id: format!("terminal-{index}"),
                        platform: terminal.platform,
                        mode: terminal.mode,
                        url: terminal.url.clone(),
                        state,
                        progress_percent: 100.0,
                        speed_bps: None,
                        eta_seconds: None,
                        output_path: Some("C:\\Downloads\\terminal.mp4".to_string()),
                        error: None,
                    },
                );
            }
        }
        assert!(terminal_manager
            .reserve_jobs(std::slice::from_ref(&terminal))
            .is_ok());
    }

    #[test]
    fn same_url_with_different_modes_can_coexist() {
        let manager = DownloadManager::new(1);
        let url = "https://www.youtube.com/watch?v=mode-split";
        let video = request(url);
        let audio = DownloadRequest {
            platform: video.platform,
            url: video.url.clone(),
            mode: DownloadMode::Audio,
        };
        assert!(manager.reserve_jobs(&[video, audio]).is_ok());
        assert_eq!(manager.inner.jobs.lock().expect("jobs lock").len(), 2);
    }

    #[test]
    fn normalized_fragment_urls_share_the_same_reservation_key() {
        let first_url = crate::security::validate_url_syntax(
            Platform::Youtube,
            "https://WWW.YouTube.com/watch?v=normalized#first",
        )
        .expect("first URL")
        .url;
        let second_url = crate::security::validate_url_syntax(
            Platform::Youtube,
            "  https://www.youtube.com/watch?v=normalized#second  ",
        )
        .expect("second URL")
        .url;
        assert_eq!(first_url, second_url);

        let manager = DownloadManager::new(1);
        let first = request(&first_url);
        manager
            .reserve_jobs(std::slice::from_ref(&first))
            .expect("first normalized job");
        let second = request(&second_url);
        assert!(manager.reserve_jobs(std::slice::from_ref(&second)).is_err());
    }

    #[test]
    fn concurrent_reservations_allow_only_one_duplicate_key() {
        let manager = DownloadManager::new(1);
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let request = request("https://www.youtube.com/watch?v=concurrent");
        let first_manager = manager.clone();
        let first_barrier = barrier.clone();
        let first_request = request.clone();
        let first = std::thread::spawn(move || {
            first_barrier.wait();
            first_manager
                .reserve_jobs(std::slice::from_ref(&first_request))
                .is_ok()
        });
        let second_manager = manager.clone();
        let second_barrier = barrier;
        let second = std::thread::spawn(move || {
            second_barrier.wait();
            second_manager
                .reserve_jobs(std::slice::from_ref(&request))
                .is_ok()
        });
        let accepted = [
            first.join().expect("first thread"),
            second.join().expect("second thread"),
        ]
        .into_iter()
        .filter(|accepted| *accepted)
        .count();
        assert_eq!(accepted, 1);
        assert_eq!(manager.inner.jobs.lock().expect("jobs lock").len(), 1);
    }

    #[test]
    fn new_batch_reset_does_not_clear_existing_cancel_tokens() {
        let manager = DownloadManager::new(1);
        let token = Arc::new(AtomicBool::new(true));
        manager
            .inner
            .cancel_tokens
            .lock()
            .expect("token lock")
            .insert("existing".to_string(), token.clone());
        manager.inner.all_cancelled.store(true, Ordering::Release);
        manager.reset_for_new_batch();
        assert!(!manager.inner.all_cancelled.load(Ordering::Acquire));
        assert!(token.load(Ordering::Acquire));
    }

    #[test]
    fn cancellation_collects_all_saved_process_ids() {
        let ids = BTreeMap::from([(String::from("a"), 10_u32), (String::from("b"), 20_u32)]);
        let mut actual = process_ids_for_cancellation(&ids);
        actual.sort_unstable();
        assert_eq!(actual, vec![10, 20]);
    }

    #[test]
    fn arguments_are_fixed_and_url_is_last_position() {
        let args = build_ytdlp_args(
            &request("https://www.youtube.com/watch?v=normal"),
            Path::new(r"C:\downloads\.incomplete\12345678-1234-1234-1234-123456789012"),
            &sidecars(),
            &target("https://www.youtube.com/watch?v=normal"),
        )
        .expect("args");
        assert!(args.contains(&"--ignore-config".to_string()));
        assert!(args.contains(&"--no-plugin-dirs".to_string()));
        assert!(args.contains(&"--no-js-runtimes".to_string()));
        assert!(args.contains(&"--no-remote-components".to_string()));
        assert!(args.contains(&"--compat-options".to_string()));
        assert!(args.contains(&"no-certifi".to_string()));
        assert!(args.contains(&"--no-playlist".to_string()));
        assert_eq!(
            args.last().map(String::as_str),
            Some("https://www.youtube.com/watch?v=normal")
        );
        assert!(!args
            .iter()
            .any(|arg| arg.contains("--exec") || arg.contains("--config")));
    }

    #[test]
    fn arguments_use_normalized_url_after_syntax_validation() {
        let args = build_ytdlp_args(
            &request("  https://www.youtube.com/watch?v=normal  "),
            Path::new(r"C:\downloads\.incomplete\12345678-1234-1234-1234-123456789012"),
            &sidecars(),
            &target("https://www.youtube.com/watch?v=normal"),
        )
        .expect("args");
        assert_eq!(
            args.last().map(String::as_str),
            Some("https://www.youtube.com/watch?v=normal")
        );
    }

    #[test]
    fn hls_platform_args_use_fixed_user_agent_without_impersonation() {
        for (platform, source_url, target_url) in [
            (
                Platform::LittleDuck,
                "https://play.777tv.ai/v/1",
                "https://v2.adfg8.vip/live/master.m3u8",
            ),
            (
                Platform::Olevod,
                "https://olevod.com/v/1",
                "https://europe.olemovienews.com/live/master.m3u8",
            ),
        ] {
            let request = request_for(platform, source_url);
            let target = target_for(platform, target_url);
            let download = build_ytdlp_args(
                &request,
                Path::new(r"C:\downloads\.incomplete\12345678-1234-1234-1234-123456789012"),
                &sidecars(),
                &target,
            )
            .expect("HLS download args");
            let probe = build_probe_args(&request, &target).expect("HLS probe args");
            assert!(download
                .windows(2)
                .any(|pair| { pair[0] == "--concurrent-fragments" && pair[1] == "4" }));
            assert!(!probe.iter().any(|arg| arg == "--concurrent-fragments"));
            for args in [download, probe] {
                assert!(args.windows(2).any(|pair| {
                    pair[0] == "--add-headers"
                        && pair[1] == format!("User-Agent:{PLATFORM_USER_AGENT}")
                }));
                assert!(!args.iter().any(|arg| arg.contains("impersonate")));
            }
        }

        let youtube_request = request("https://www.youtube.com/watch?v=normal");
        let youtube_target = target("https://www.youtube.com/watch?v=normal");
        let youtube_download = build_ytdlp_args(
            &youtube_request,
            Path::new(r"C:\downloads\.incomplete\12345678-1234-1234-1234-123456789012"),
            &sidecars(),
            &youtube_target,
        )
        .expect("YouTube download args");
        let youtube_probe = build_probe_args(&youtube_request, &youtube_target).expect("probe");
        assert!(!youtube_download
            .iter()
            .any(|arg| arg == "--concurrent-fragments"));
        assert!(!youtube_probe.iter().any(|arg| arg == "--add-headers"));
        assert!(!youtube_probe
            .iter()
            .any(|arg| arg == "--concurrent-fragments"));
    }

    #[test]
    fn parser_extracts_progress() {
        let update = parse_progress("[download]  42.5% of 10.00MiB at 1.50MiB/s ETA 00:07")
            .expect("progress");
        assert_eq!(update.percent, Some(42.5));
        assert_eq!(update.eta_seconds, Some(7));
        assert_eq!(update.speed_bps, Some(1.5 * 1024.0 * 1024.0));
    }

    #[test]
    fn parser_rejects_nonfinite_and_negative_speed_values() {
        assert!(parse_progress("[download] NaN% at Infinity/s").is_none());
        assert!(parse_rate("NaN").is_none());
        assert!(parse_rate("Infinity").is_none());
        assert!(parse_rate("-1MiB").is_none());
        let update = parse_progress("[download] -5% at 1MiB/s").expect("finite percent");
        assert_eq!(update.percent, Some(-5.0));
        assert_eq!(update.speed_bps, Some(1024.0 * 1024.0));
    }

    #[test]
    fn probe_uses_simulation_and_cannot_receive_extra_arguments() {
        let args = build_probe_args(
            &request("https://www.youtube.com/watch?v=normal"),
            &target("https://www.youtube.com/watch?v=normal"),
        )
        .expect("probe args");
        assert!(args.contains(&"--simulate".to_string()));
        assert!(args.contains(&"--skip-download".to_string()));
        assert!(args.contains(&"--no-plugin-dirs".to_string()));
        assert!(args.contains(&"--no-js-runtimes".to_string()));
        assert!(args.contains(&"--no-remote-components".to_string()));
        assert!(args.contains(&"--compat-options".to_string()));
        assert!(args.contains(&"no-certifi".to_string()));
        assert!(args.contains(&"--print".to_string()));
        assert!(args.iter().any(|arg| arg.contains("%(title|null)j")));
        assert!(args
            .iter()
            .any(|arg| arg.contains("%(formats.:.{has_drm}|[])j")));
        assert!(!args.contains(&"--dump-single-json".to_string()));
        assert_eq!(
            args.last().map(String::as_str),
            Some("https://www.youtube.com/watch?v=normal")
        );
        assert_eq!(args.len(), 15);
    }

    #[test]
    fn probe_metadata_is_minimal_and_rejects_drm_login_and_large_fields() {
        let valid = r#"{"title":"safe","duration":12.5,"extractor":"fixture","webpage_url":"https://www.youtube.com/watch?v=x","availability":"public","has_drm":false}"#;
        let parsed = parse_probe_metadata(valid).expect("valid metadata");
        assert_eq!(parsed.title.as_deref(), Some("safe"));
        assert_eq!(parsed.duration_seconds, Some(12.5));
        assert!(parse_probe_metadata(
            r#"{"title":"x","availability":"login_required","has_drm":false}"#
        )
        .is_err());
        assert!(
            parse_probe_metadata(r#"{"title":"x","availability":"public","has_drm":true}"#)
                .is_err()
        );
        assert!(
            parse_probe_metadata(r#"{"title":"x","availability":"public","has_drm":"maybe"}"#)
                .is_err()
        );
        assert!(parse_probe_metadata(
            r#"{"title":"x","availability":"public","has_drm":[false,null]}"#
        )
        .is_ok());
        assert!(parse_probe_metadata(
            r#"{"title":"x","availability":"public","has_drm":[{"has_drm":false},{"has_drm":"maybe"}]}"#
        )
        .is_err());
        assert!(parse_probe_metadata(
            r#"{"title":"x","availability":"public","has_drm":[{"has_drm":true}]}"#
        )
        .is_err());
        assert!(parse_probe_metadata(r#"{"title":"x","availability":"public"}"#).is_ok());
        assert!(parse_probe_metadata(r#"{"title":"x","has_drm":[false]}"#).is_ok());
        for availability in ["premium_only", "subscriber_only", "needs_auth", "private"] {
            let metadata =
                format!(r#"{{"title":"x","availability":"{availability}","has_drm":false}}"#);
            assert!(parse_probe_metadata(&metadata).is_err(), "{availability}");
        }
        assert!(
            parse_probe_metadata(r#"{"title":"x","availability":"mystery","has_drm":false}"#)
                .is_err()
        );
        let oversized = format!(
            r#"{{"title":"{}","availability":"public","has_drm":false}}"#,
            "x".repeat(process::MAX_CAPTURED_OUTPUT)
        );
        assert!(parse_probe_metadata(&oversized).is_err());
    }

    #[test]
    fn ffprobe_duration_requires_positive_finite_value() {
        assert_eq!(parse_positive_duration("12.5\n"), Some(12.5));
        assert!(parse_positive_duration("0").is_none());
        assert!(parse_positive_duration("-1").is_none());
        assert!(parse_positive_duration("NaN").is_none());
        assert!(parse_positive_duration("N/A").is_none());
    }
}
