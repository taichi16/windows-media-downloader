//! Rust-owned output directory configuration and Windows path safety checks.
//!
//! The renderer never supplies an output path.  This module owns the persisted
//! setting, validates it before a batch is reserved, and exposes a second
//! validation step immediately before a job directory is created.  There is
//! still an unavoidable same-user TOCTOU window between validation and the
//! filesystem operation; every creation step therefore repeats the reparse and
//! directory checks and fails closed when the path changes.

use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};
use uuid::Uuid;

use crate::{error::AppError, paths::DownloadPaths};

#[cfg(windows)]
use std::os::windows::{ffi::OsStrExt, ffi::OsStringExt};

#[cfg(windows)]
use windows_sys::Win32::{
    Storage::FileSystem::{
        GetDriveTypeW, GetFileAttributesW, MoveFileExW, FILE_ATTRIBUTE_REPARSE_POINT,
        INVALID_FILE_ATTRIBUTES, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    },
    System::SystemInformation::GetWindowsDirectoryW,
};

const CONFIG_FILE_NAME: &str = "output-directory.json";
const CONFIG_SCHEMA_VERSION: u32 = 1;
const DEFAULT_OUTPUT_DIRECTORY: &str = "Windows Media Downloader";
const CONFIG_MAX_BYTES: u64 = 16 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct OutputDirectoryInfo {
    pub path: String,
    pub is_default: bool,
    pub warning: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OutputDirectorySnapshot {
    root: PathBuf,
    is_default: bool,
    forbidden_roots: Vec<PathBuf>,
}

impl OutputDirectorySnapshot {
    /// Revalidate the same root after a job waited in the queue.
    pub fn revalidate_for_job(&self) -> Result<(), AppError> {
        let allow_create = self.is_default;
        let validated = validate_root_with_options(
            &self.root,
            &PathContext {
                forbidden_roots: self.forbidden_roots.clone(),
            },
            allow_create,
            true,
        )?;
        if validated != self.root {
            return Err(AppError::Security(
                "輸出目錄在工作啟動前已變更，為安全起見停止工作".to_string(),
            ));
        }
        Ok(())
    }

    /// Revalidate and construct DownloadPaths.  A custom directory is never
    /// recreated after it disappears; only the built-in default may be made
    /// again.
    pub fn download_paths_for_job(&self) -> Result<DownloadPaths, AppError> {
        self.revalidate_for_job()?;
        if self.is_default {
            DownloadPaths::new(self.root.clone())
        } else {
            DownloadPaths::new_existing(self.root.clone())
        }
    }

    /// Validate a finalized file against this batch's canonical root before
    /// exposing a user-visible path in `DownloadStatus`.
    pub fn user_visible_path(&self, final_path: &Path) -> Result<String, AppError> {
        self.revalidate_for_job()?;
        let metadata = fs::symlink_metadata(final_path)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() || is_reparse_point(final_path)?
        {
            return Err(AppError::Security(
                "完成檔必須是輸出根下的非 reparse 一般檔案".to_string(),
            ));
        }
        let canonical = fs::canonicalize(final_path)?;
        if !path_is_within(&canonical, &self.root) {
            return Err(AppError::Security(
                "完成檔不在本批驗證的輸出根內".to_string(),
            ));
        }
        let display_path = user_root_path(&canonical)?;
        let path = display_path.to_str().ok_or_else(|| {
            AppError::Security("完成檔路徑含有無法安全顯示的非 Unicode 字元".to_string())
        })?;
        Ok(path.to_string())
    }

    fn info(&self, warning: Option<String>) -> Result<OutputDirectoryInfo, AppError> {
        let display_path = user_root_path(&self.root)?;
        let path = display_path.to_str().ok_or_else(|| {
            AppError::Security("輸出目錄含有無法安全顯示的非 Unicode 字元".to_string())
        })?;
        Ok(OutputDirectoryInfo {
            path: path.to_string(),
            is_default: self.is_default,
            warning,
        })
    }
}

#[derive(Clone, Default)]
pub struct OutputDirectoryManager {
    gate: Arc<Mutex<()>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct StoredConfig {
    schema_version: u32,
    custom_root: Option<String>,
}

impl Default for StoredConfig {
    fn default() -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION,
            custom_root: None,
        }
    }
}

struct LoadedConfig {
    config: StoredConfig,
    warning: Option<String>,
}

#[derive(Debug, Clone)]
struct PathContext {
    forbidden_roots: Vec<PathBuf>,
}

impl OutputDirectoryManager {
    pub fn snapshot(&self, app: &AppHandle) -> Result<OutputDirectorySnapshot, AppError> {
        let _guard = self.gate.lock().map_err(lock_error)?;
        resolve_snapshot(app)
    }

    pub fn info(&self, app: &AppHandle) -> Result<OutputDirectoryInfo, AppError> {
        let _guard = self.gate.lock().map_err(lock_error)?;
        let (snapshot, warning) = resolve_snapshot_with_warning(app)?;
        snapshot.info(warning)
    }

    /// Open a native folder picker.  The selected path is validated and only
    /// then persisted.  A cancelled picker never writes the configuration.
    pub fn pick_folder<F>(
        &self,
        app: &AppHandle,
        has_active_jobs: F,
    ) -> Result<OutputDirectoryInfo, AppError>
    where
        F: Fn() -> bool,
    {
        if has_active_jobs() {
            return Err(AppError::InvalidInput(
                "有排隊中或執行中的工作時不可變更輸出目錄".to_string(),
            ));
        }

        use tauri_plugin_dialog::DialogExt;
        let picked = app.dialog().file().blocking_pick_folder();
        let Some(file_path) = picked else {
            let mut info = self.info(app)?;
            info.warning = Some("已取消選擇，輸出目錄設定未變更".to_string());
            return Ok(info);
        };
        let path = file_path
            .into_path()
            .map_err(|error| AppError::InvalidInput(format!("無法取得選取的資料夾：{error}")))?;

        // Recheck after the potentially long-lived native dialog closes.
        if has_active_jobs() {
            return Err(AppError::InvalidInput(
                "有排隊中或執行中的工作時不可變更輸出目錄".to_string(),
            ));
        }
        let _guard = self.gate.lock().map_err(lock_error)?;
        // Recheck after taking the settings lock as well.  The dialog and the
        // first check can span a concurrent batch reservation.
        if has_active_jobs() {
            return Err(AppError::InvalidInput(
                "有排隊中或執行中的工作時不可變更輸出目錄".to_string(),
            ));
        }
        let context = PathContext::from_app(app)?;
        let canonical = validate_root(&path, &context, false)?;
        let config_path = config_path(app)?;
        let custom_path = user_root_path(&canonical)?;
        let custom_root = custom_path.to_str().ok_or_else(|| {
            AppError::Security("輸出目錄含有無法安全保存的非 Unicode 字元".to_string())
        })?;
        let config = StoredConfig {
            schema_version: CONFIG_SCHEMA_VERSION,
            custom_root: Some(custom_root.to_string()),
        };
        persist_config(&config_path, &config)?;
        OutputDirectorySnapshot {
            root: canonical,
            is_default: false,
            forbidden_roots: context.forbidden_roots,
        }
        .info(None)
    }
}

fn resolve_snapshot(app: &AppHandle) -> Result<OutputDirectorySnapshot, AppError> {
    resolve_snapshot_with_warning(app).map(|(snapshot, _)| snapshot)
}

fn resolve_snapshot_with_warning(
    app: &AppHandle,
) -> Result<(OutputDirectorySnapshot, Option<String>), AppError> {
    let context = PathContext::from_app(app)?;
    let default_path = app
        .path()
        .download_dir()
        .map_err(|error| AppError::Internal(format!("無法取得 Downloads 目錄：{error}")))?
        .join(DEFAULT_OUTPUT_DIRECTORY);
    let loaded = load_config(&config_path(app)?)?;
    let (root, is_default, root_warning) = resolve_root_choice(
        loaded.config.custom_root.as_deref(),
        &default_path,
        &context,
    )?;
    let warning = root_warning.or(loaded.warning);
    Ok((
        OutputDirectorySnapshot {
            root,
            is_default,
            forbidden_roots: context.forbidden_roots,
        },
        warning,
    ))
}

fn resolve_root_choice(
    custom_root: Option<&str>,
    default_path: &Path,
    context: &PathContext,
) -> Result<(PathBuf, bool, Option<String>), AppError> {
    if let Some(custom_root) = custom_root {
        let custom_path = PathBuf::from(custom_root);
        if let Ok(root) = validate_root(&custom_path, context, false) {
            return Ok((root, false, None));
        }
    }
    let root = validate_root(default_path, context, true).map_err(|error| {
        AppError::Security(format!(
            "自訂輸出目錄無法使用，且預設輸出目錄也不可用：{error}"
        ))
    })?;
    Ok((
        root,
        true,
        custom_root.map(|_| "自訂輸出目錄無法使用，已安全回退預設目錄".to_string()),
    ))
}

fn config_path(app: &AppHandle) -> Result<PathBuf, AppError> {
    Ok(app
        .path()
        .app_local_data_dir()
        .map_err(|error| AppError::Internal(format!("無法取得 app local data 目錄：{error}")))?
        .join(CONFIG_FILE_NAME))
}

fn load_config(path: &Path) -> Result<LoadedConfig, AppError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(LoadedConfig {
                config: StoredConfig::default(),
                warning: None,
            });
        }
        Err(_) => {
            return Ok(LoadedConfig {
                config: StoredConfig::default(),
                warning: Some("輸出目錄設定無法讀取，已安全回退預設目錄".to_string()),
            });
        }
    };
    if !metadata.is_file()
        || metadata.len() > CONFIG_MAX_BYTES
        || is_reparse_point(path).unwrap_or(true)
    {
        return Ok(LoadedConfig {
            config: StoredConfig::default(),
            warning: Some("輸出目錄設定檔類型或大小不安全，已安全回退預設目錄".to_string()),
        });
    }
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(_) => {
            return Ok(LoadedConfig {
                config: StoredConfig::default(),
                warning: Some("輸出目錄設定無法讀取，已安全回退預設目錄".to_string()),
            });
        }
    };
    if bytes.len() as u64 > CONFIG_MAX_BYTES {
        return Ok(LoadedConfig {
            config: StoredConfig::default(),
            warning: Some("輸出目錄設定檔超過大小限制，已安全回退預設目錄".to_string()),
        });
    }
    match serde_json::from_slice::<StoredConfig>(&bytes) {
        Ok(config) if config.schema_version == CONFIG_SCHEMA_VERSION => Ok(LoadedConfig {
            config,
            warning: None,
        }),
        _ => Ok(LoadedConfig {
            config: StoredConfig::default(),
            warning: Some("輸出目錄設定損壞或版本不支援，已安全回退預設目錄".to_string()),
        }),
    }
}

fn persist_config(path: &Path, config: &StoredConfig) -> Result<(), AppError> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Internal("輸出目錄設定缺少父目錄".to_string()))?;
    // Check before and after creating the app-local directory.  In
    // particular, do not let create_dir_all follow an app-local junction to
    // an external location.
    reject_reparse_ancestors(parent)?;
    fs::create_dir_all(parent)?;
    reject_reparse_ancestors(parent)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file() || is_reparse_point(path)? {
                return Err(AppError::Security(
                    "輸出目錄設定檔必須是一般檔案且不可是 reparse point".to_string(),
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(AppError::Io(error)),
    }
    let bytes = serde_json::to_vec_pretty(config)?;
    atomic_write(path, &bytes)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), AppError> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Internal("原子設定寫入缺少父目錄".to_string()))?;
    let temp = parent.join(format!(".{CONFIG_FILE_NAME}.{}.tmp", Uuid::new_v4()));
    let result = (|| -> Result<(), AppError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        replace_file(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<(), AppError> {
    let source = wide_path(source);
    let destination = wide_path(destination);
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        return Err(AppError::Io(io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> Result<(), AppError> {
    fs::rename(source, destination)?;
    Ok(())
}

impl PathContext {
    fn from_app(app: &AppHandle) -> Result<Self, AppError> {
        let mut roots = Vec::new();
        let executable = std::env::current_exe()
            .map_err(|error| AppError::Internal(format!("無法取得 executable 目錄：{error}")))?;
        if let Some(parent) = executable.parent() {
            roots.push(parent.to_path_buf());
        }
        roots.push(
            app.path()
                .resource_dir()
                .map_err(|error| AppError::Internal(format!("無法取得 resources 目錄：{error}")))?,
        );
        if let Some(windows) = windows_directory() {
            roots.push(windows.clone());
            if let Some(drive) = drive_root(&windows) {
                roots.push(drive.join("Program Files"));
                roots.push(drive.join("Program Files (x86)"));
                roots.push(drive.join("ProgramData"));
            }
        }
        for variable in ["ProgramFiles", "ProgramFiles(x86)", "ProgramData"] {
            if let Some(path) = std::env::var_os(variable).map(PathBuf::from) {
                roots.push(path);
            }
        }
        let mut forbidden_roots = Vec::new();
        for root in roots {
            if let Ok(canonical) = fs::canonicalize(root) {
                if !forbidden_roots.contains(&canonical) {
                    forbidden_roots.push(canonical);
                }
            }
        }
        Ok(Self { forbidden_roots })
    }
}

fn validate_root(
    path: &Path,
    context: &PathContext,
    allow_create: bool,
) -> Result<PathBuf, AppError> {
    // `canonicalize` on Windows commonly returns a verbatim *disk* path
    // (`\\?\\C:\\...`).  Treat it as an internal form only when it is
    // already the canonical spelling of the existing path.  UNC, device and
    // verbatim-UNC forms never satisfy `is_verbatim_disk_path` and remain
    // rejected by `reject_path_shape`.
    let allow_internal_verbatim_disk = is_verbatim_disk_path(path)
        && fs::canonicalize(path)
            .map(|canonical| path_compare_key(&canonical) == path_compare_key(path))
            .unwrap_or(false);
    validate_root_with_options(path, context, allow_create, allow_internal_verbatim_disk)
}

fn validate_root_with_options(
    path: &Path,
    context: &PathContext,
    allow_create: bool,
    allow_internal_verbatim_disk: bool,
) -> Result<PathBuf, AppError> {
    reject_path_shape(path, allow_internal_verbatim_disk)?;
    reject_network_drive(path)?;
    reject_reparse_ancestors(path)?;

    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_dir() || is_reparse_point(path)? {
                return Err(AppError::Security(
                    "輸出目錄必須是非 reparse 目錄".to_string(),
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound && allow_create => {
            fs::create_dir_all(path)?;
        }
        Err(error) => {
            return Err(AppError::Security(format!(
                "輸出目錄不存在或無法存取：{error}"
            )));
        }
    }

    reject_reparse_ancestors(path)?;
    let canonical = fs::canonicalize(path)?;
    if is_drive_root(&canonical) {
        return Err(AppError::Security("不可選擇磁碟根目錄".to_string()));
    }
    if context
        .forbidden_roots
        .iter()
        .any(|forbidden| path_is_within(&canonical, forbidden))
    {
        return Err(AppError::Security(
            "不可選擇 Windows、Program Files、ProgramData、程式或 resources 目錄".to_string(),
        ));
    }
    writable_probe(&canonical)?;
    Ok(canonical)
}

fn reject_path_shape(path: &Path, allow_internal_verbatim_disk: bool) -> Result<(), AppError> {
    let internal_verbatim_disk = allow_internal_verbatim_disk && is_verbatim_disk_path(path);
    if path.as_os_str().is_empty() || (!path.is_absolute() && !internal_verbatim_disk) {
        return Err(AppError::Security("輸出目錄必須是絕對路徑".to_string()));
    }
    let text = path.to_string_lossy();
    if text.starts_with("//")
        || text.starts_with(r"\\.")
        || (text.starts_with(r"\\") && !internal_verbatim_disk)
        || (text.starts_with(r"\\?\UNC") && !internal_verbatim_disk)
    {
        return Err(AppError::Security(
            "不可使用 UNC 或 verbatim UNC 路徑".to_string(),
        ));
    }
    Ok(())
}

fn reject_network_drive(path: &Path) -> Result<(), AppError> {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        let drive_text = text.strip_prefix("\\\\?\\").unwrap_or(&text);
        if drive_text
            .get(..3)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("unc"))
        {
            return Err(AppError::Security("不可使用 UNC 路徑".to_string()));
        }
        let mut chars = drive_text.chars();
        let drive = chars.next();
        if drive.is_none() || chars.next() != Some(':') {
            return Err(AppError::Security(
                "輸出目錄缺少 Windows 磁碟代號".to_string(),
            ));
        }
        let root = format!("{}:\\", drive.unwrap());
        let root = wide_path(Path::new(&root));
        let drive_type = unsafe { GetDriveTypeW(root.as_ptr()) };
        if drive_type == windows_sys::Win32::System::WindowsProgramming::DRIVE_REMOTE {
            return Err(AppError::Security(
                "不可使用 mapped network drive".to_string(),
            ));
        }
        if drive_type == windows_sys::Win32::System::WindowsProgramming::DRIVE_NO_ROOT_DIR {
            return Err(AppError::Security("Windows 磁碟代號不存在".to_string()));
        }
    }
    Ok(())
}

fn reject_reparse_ancestors(path: &Path) -> Result<(), AppError> {
    let mut current = Some(path);
    while let Some(candidate) = current {
        match fs::symlink_metadata(candidate) {
            Ok(_) if is_reparse_point(candidate)? => {
                return Err(AppError::Security(
                    "輸出目錄或其祖先不可是 symlink、junction 或 reparse point".to_string(),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(AppError::Io(error)),
        }
        let parent = candidate.parent();
        current = parent.filter(|value| *value != candidate);
    }
    Ok(())
}

fn is_reparse_point(path: &Path) -> io::Result<bool> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(true);
    }
    #[cfg(windows)]
    {
        let wide = wide_path(path);
        let attributes = unsafe { GetFileAttributesW(wide.as_ptr()) };
        if attributes == INVALID_FILE_ATTRIBUTES {
            return Err(io::Error::last_os_error());
        }
        Ok(attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0)
    }
    #[cfg(not(windows))]
    {
        Ok(false)
    }
}

fn writable_probe(root: &Path) -> Result<(), AppError> {
    let probe = root.join(format!(".wmd-write-probe-{}.tmp", Uuid::new_v4()));
    let result = (|| -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)?;
        file.write_all(b"wmd")?;
        file.sync_all()?;
        Ok(())
    })();
    let cleanup = fs::remove_file(&probe);
    match result {
        Err(error) => {
            let _ = cleanup;
            Err(AppError::Security(format!("輸出目錄不可寫入：{error}")))
        }
        Ok(()) => {
            cleanup.map_err(|error| AppError::Security(format!("無法清理輸出目錄測試檔：{error}")))
        }
    }
}

fn is_drive_root(path: &Path) -> bool {
    let key = path_compare_key(path);
    key.len() == 3 && key.as_bytes()[1] == b':' && key.ends_with('\\')
}

fn is_verbatim_disk_path(path: &Path) -> bool {
    let text = path.to_string_lossy();
    text.len() >= 7
        && text[..4].eq_ignore_ascii_case("\\\\?\\")
        && text.as_bytes()[4].is_ascii_alphabetic()
        && text.as_bytes()[5] == b':'
        && matches!(text.as_bytes()[6], b'\\' | b'/')
}

/// Convert a validated internal canonical disk path to the ordinary Windows
/// spelling used in the config file and UI.  This deliberately handles only
/// `\\?\\<drive>:\\...`; UNC, verbatim UNC and device paths never enter the
/// conversion branch and are rejected rather than normalized.
fn user_root_path(canonical: &Path) -> Result<PathBuf, AppError> {
    let text = canonical.to_str().ok_or_else(|| {
        AppError::Security("輸出目錄含有無法安全保存的非 Unicode 字元".to_string())
    })?;
    if is_verbatim_disk_path(canonical) {
        let ordinary = text
            .strip_prefix("\\\\?\\")
            .ok_or_else(|| AppError::Security("無法安全轉換輸出目錄路徑".to_string()))?;
        let ordinary = PathBuf::from(ordinary);
        let ordinary_canonical = fs::canonicalize(&ordinary).map_err(|error| {
            AppError::Security(format!("輸出目錄 canonical 路徑無法安全轉換：{error}"))
        })?;
        if path_compare_key(&ordinary_canonical) != path_compare_key(canonical) {
            return Err(AppError::Security(
                "輸出目錄 canonical 路徑不一致，拒絕保存".to_string(),
            ));
        }
        return Ok(ordinary);
    }
    if text.starts_with("\\\\") || text.starts_with("//") {
        return Err(AppError::Security(
            "不可將 UNC 或 device 路徑轉為輸出目錄".to_string(),
        ));
    }
    Ok(canonical.to_path_buf())
}

fn path_compare_key(path: &Path) -> String {
    let mut text = path
        .to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase();
    if is_verbatim_disk_path(path) {
        text = text[4..].to_string();
    }
    while text.len() > 3 && text.ends_with('\\') {
        text.pop();
    }
    text
}

fn path_is_within(candidate: &Path, parent: &Path) -> bool {
    let candidate = path_compare_key(candidate);
    let parent = path_compare_key(parent);
    candidate == parent
        || candidate
            .strip_prefix(&parent)
            .is_some_and(|suffix| suffix.starts_with('\\'))
}

fn drive_root(path: &Path) -> Option<PathBuf> {
    let text = path.to_string_lossy();
    let mut chars = text.chars();
    let drive = chars.next()?;
    (chars.next() == Some(':')).then(|| PathBuf::from(format!("{drive}:\\")))
}

#[cfg(windows)]
fn windows_directory() -> Option<PathBuf> {
    let mut buffer = vec![0_u16; 260];
    loop {
        let length = unsafe { GetWindowsDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
        if length == 0 {
            return None;
        }
        if (length as usize) < buffer.len() {
            return Some(PathBuf::from(OsString::from_wide(
                &buffer[..length as usize],
            )));
        }
        buffer.resize(buffer.len().saturating_mul(2), 0);
    }
}

#[cfg(not(windows))]
fn windows_directory() -> Option<PathBuf> {
    None
}

#[cfg(windows)]
fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn lock_error<T>(_: std::sync::PoisonError<T>) -> AppError {
    AppError::Internal("輸出目錄設定鎖已損壞".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("wmd-output-{label}-{}", Uuid::new_v4()))
    }

    #[test]
    fn stored_config_defaults_and_roundtrips_atomically() {
        let root = temp_root("config");
        fs::create_dir_all(&root).expect("temp root");
        let path = root.join(CONFIG_FILE_NAME);
        let config = StoredConfig {
            schema_version: CONFIG_SCHEMA_VERSION,
            custom_root: Some(r"C:\Downloads\Media".to_string()),
        };
        persist_config(&path, &config).expect("persist config");
        assert_eq!(load_config(&path).expect("load config").config, config);
        let replacement = StoredConfig {
            schema_version: CONFIG_SCHEMA_VERSION,
            custom_root: Some(r"C:\Downloads\Replacement".to_string()),
        };
        persist_config(&path, &replacement).expect("replace config");
        assert_eq!(load_config(&path).expect("load config").config, replacement);
        assert!(!root.read_dir().expect("read temp root").any(|entry| entry
            .expect("entry")
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn corrupted_config_falls_back_without_panic() {
        let root = temp_root("corrupt");
        fs::create_dir_all(&root).expect("temp root");
        let path = root.join(CONFIG_FILE_NAME);
        fs::write(&path, b"not-json").expect("write corrupt config");
        let loaded = load_config(&path).expect("load config");
        assert!(loaded.config.custom_root.is_none());
        assert!(loaded.warning.is_some());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn path_shape_rejects_unc_and_drive_root() {
        assert!(reject_path_shape(Path::new(r"\\server\share"), false).is_err());
        assert!(reject_path_shape(Path::new(r"\\?\C:\folder"), false).is_err());
        assert!(reject_path_shape(Path::new(r"\\.\C:\folder"), true).is_err());
        assert!(reject_path_shape(Path::new(r"\\?\UNC\server\share"), true).is_err());
        assert!(is_drive_root(Path::new("C:\\")));
        assert!(!is_drive_root(Path::new(r"C:\folder")));
    }

    #[test]
    fn missing_custom_root_falls_back_without_creating_it() {
        let root = temp_root("fallback");
        let default = root.join("default");
        let missing = root.join("missing-custom");
        fs::create_dir_all(&default).expect("default");
        let context = PathContext {
            forbidden_roots: Vec::new(),
        };
        let (resolved, is_default, warning) = resolve_root_choice(
            Some(missing.to_str().expect("unicode path")),
            &default,
            &context,
        )
        .expect("fallback");
        assert!(is_default);
        assert!(warning.is_some());
        assert_eq!(
            resolved,
            fs::canonicalize(&default).expect("canonical default")
        );
        assert!(!missing.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn non_directory_and_forbidden_descendant_are_rejected() {
        let root = temp_root("bounds");
        fs::create_dir_all(&root).expect("temp root");
        let file = root.join("file");
        fs::write(&file, b"not a directory").expect("file");
        let context = PathContext {
            forbidden_roots: Vec::new(),
        };
        assert!(validate_root(&file, &context, false).is_err());

        let protected = root.join("protected");
        let child = protected.join("child");
        fs::create_dir_all(&child).expect("protected child");
        let context = PathContext {
            forbidden_roots: vec![fs::canonicalize(&protected).expect("canonical protected")],
        };
        assert!(validate_root(&child, &context, false).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn oversized_or_non_file_config_falls_back() {
        let root = temp_root("config-boundary");
        fs::create_dir_all(&root).expect("temp root");
        let path = root.join(CONFIG_FILE_NAME);
        fs::write(&path, vec![b'x'; CONFIG_MAX_BYTES as usize + 1]).expect("oversized config");
        let loaded = load_config(&path).expect("load oversized config");
        assert!(loaded.config.custom_root.is_none());
        assert!(loaded.warning.is_some());
        fs::remove_file(&path).expect("remove oversized config");
        fs::create_dir(&path).expect("config directory");
        let loaded = load_config(&path).expect("load directory config");
        assert!(loaded.config.custom_root.is_none());
        assert!(loaded.warning.is_some());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unknown_config_fields_fall_back() {
        let root = temp_root("config-unknown");
        fs::create_dir_all(&root).expect("temp root");
        let path = root.join(CONFIG_FILE_NAME);
        fs::write(
            &path,
            br#"{"schema_version":1,"custom_root":null,"unexpected":true}"#,
        )
        .expect("write config");
        let loaded = load_config(&path).expect("load config");
        assert!(loaded.config.custom_root.is_none());
        assert!(loaded.warning.is_some());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn canonical_verbatim_disk_path_revalidates() {
        let root = temp_root("canonical");
        fs::create_dir_all(&root).expect("temp root");
        let context = PathContext {
            forbidden_roots: Vec::new(),
        };
        let validated = validate_root(&root, &context, false).expect("validate normal path");
        let again = validate_root(&validated, &context, false).expect("validate canonical path");
        assert_eq!(again, validated);
        assert!(is_verbatim_disk_path(&validated));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn canonical_root_persists_as_ordinary_path_across_restart() {
        let root = temp_root("persisted-canonical");
        fs::create_dir_all(&root).expect("temp root");
        let context = PathContext {
            forbidden_roots: Vec::new(),
        };
        let canonical = validate_root(&root, &context, false).expect("validate root");
        let ordinary = user_root_path(&canonical).expect("ordinary root");
        assert!(!ordinary.to_string_lossy().starts_with("\\\\?\\"));

        let config_path = root.join(CONFIG_FILE_NAME);
        let config = StoredConfig {
            schema_version: CONFIG_SCHEMA_VERSION,
            custom_root: Some(ordinary.to_str().expect("ordinary unicode").to_string()),
        };
        persist_config(&config_path, &config).expect("persist ordinary root");
        let loaded = load_config(&config_path).expect("load ordinary root");
        let loaded_path = PathBuf::from(loaded.config.custom_root.as_deref().expect("root"));
        let revalidated = validate_root(&loaded_path, &context, false).expect("revalidate root");
        assert_eq!(revalidated, canonical);

        let default = root.join("different-default");
        fs::create_dir_all(&default).expect("different default");
        let (resolved, is_default, warning) =
            resolve_root_choice(loaded.config.custom_root.as_deref(), &default, &context)
                .expect("resolve persisted root");
        assert_eq!(resolved, canonical);
        assert!(!is_default);
        assert!(warning.is_none());

        let snapshot = OutputDirectorySnapshot {
            root: canonical,
            is_default: false,
            forbidden_roots: Vec::new(),
        };
        let info = snapshot.info(None).expect("display root");
        assert!(!info.path.starts_with("\\\\?\\"));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn finalized_output_display_is_root_bound_and_not_verbatim() {
        let root = temp_root("final-display");
        fs::create_dir_all(&root).expect("temp root");
        let context = PathContext {
            forbidden_roots: Vec::new(),
        };
        let canonical_root = validate_root(&root, &context, false).expect("validate root");
        let final_path = canonical_root.join("completed.mp4");
        fs::write(&final_path, b"fixture").expect("completed output");
        let snapshot = OutputDirectorySnapshot {
            root: canonical_root.clone(),
            is_default: false,
            forbidden_roots: Vec::new(),
        };
        let display = snapshot
            .user_visible_path(&final_path)
            .expect("display completed output");
        assert!(!display.starts_with("\\\\?\\"));
        assert_eq!(
            PathBuf::from(&display),
            user_root_path(&fs::canonicalize(final_path).expect("canonical output"))
                .expect("ordinary output")
        );

        let outside = root
            .parent()
            .expect("temp parent")
            .join(format!("wmd-outside-{}.mp4", Uuid::new_v4()));
        fs::write(&outside, b"outside").expect("outside output");
        assert!(snapshot.user_visible_path(&outside).is_err());
        let _ = fs::remove_file(outside);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn reparse_directory_is_rejected_when_symlink_is_available() {
        use std::os::windows::fs::symlink_dir;

        let root = temp_root("reparse");
        let target = root.join("target");
        let link = root.join("link");
        fs::create_dir_all(&target).expect("target");
        if symlink_dir(&target, &link).is_err() {
            let _ = fs::remove_dir_all(root);
            return;
        }
        let context = PathContext {
            forbidden_roots: Vec::new(),
        };
        assert!(validate_root(&link, &context, false).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn reparse_config_and_ancestor_are_rejected_when_supported() {
        use std::os::windows::fs::{symlink_dir, symlink_file};

        let root = temp_root("config-reparse");
        let target_dir = root.join("target-dir");
        let linked_dir = root.join("linked-dir");
        fs::create_dir_all(&target_dir).expect("target dir");
        if symlink_dir(&target_dir, &linked_dir).is_err() {
            let _ = fs::remove_dir_all(root);
            return;
        }
        let ancestor_config = linked_dir.join(CONFIG_FILE_NAME);
        let config = StoredConfig::default();
        assert!(persist_config(&ancestor_config, &config).is_err());

        let target_file = root.join("target.json");
        fs::write(&target_file, br#"{"schema_version":1,"custom_root":null}"#)
            .expect("target config");
        let linked_file = root.join(CONFIG_FILE_NAME);
        if symlink_file(&target_file, &linked_file).is_ok() {
            let loaded = load_config(&linked_file).expect("load linked config");
            assert!(loaded.warning.is_some());
            assert!(persist_config(&linked_file, &config).is_err());
        }
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn mapped_drive_is_rejected_when_one_is_present() {
        let mut found = false;
        for drive in b'A'..=b'Z' {
            let root = format!("{}:\\", drive as char);
            let wide = wide_path(Path::new(&root));
            let drive_type = unsafe { GetDriveTypeW(wide.as_ptr()) };
            if drive_type == windows_sys::Win32::System::WindowsProgramming::DRIVE_REMOTE {
                found = true;
                assert!(
                    reject_network_drive(Path::new(&format!("{}:\\folder", drive as char)))
                        .is_err()
                );
                break;
            }
        }
        // CI commonly has no mapped drives; the branch is intentionally
        // conditional so the test does not depend on machine configuration.
        let _ = found;
    }

    #[test]
    fn writable_probe_cleans_up_and_missing_custom_root_is_not_created() {
        let root = temp_root("writable");
        fs::create_dir_all(&root).expect("temp root");
        writable_probe(&root).expect("write probe");
        assert!(!root.read_dir().expect("read temp root").any(|entry| entry
            .expect("entry")
            .file_name()
            .to_string_lossy()
            .contains("wmd-write-probe")));
        let missing = root.join("missing-custom");
        let context = PathContext {
            forbidden_roots: Vec::new(),
        };
        assert!(validate_root(&missing, &context, false).is_err());
        assert!(!missing.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn config_schema_mismatch_falls_back() {
        let root = temp_root("schema");
        fs::create_dir_all(&root).expect("temp root");
        let path = root.join(CONFIG_FILE_NAME);
        fs::write(&path, br#"{"schema_version":99,"custom_root":"C:\\bad"}"#)
            .expect("write config");
        let loaded = load_config(&path).expect("load config");
        assert!(loaded.config.custom_root.is_none());
        assert!(loaded.warning.is_some());
        let _ = fs::remove_dir_all(root);
    }
}
