use std::{
    fs, io,
    path::{Path, PathBuf},
};

use crate::error::AppError;

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    GetFileAttributesW, FILE_ATTRIBUTE_REPARSE_POINT, INVALID_FILE_ATTRIBUTES,
};

const ALLOWED_EXTENSIONS: &[&str] = &[
    "aac", "flac", "m4a", "mkv", "mov", "mp3", "mp4", "ogg", "opus", "wav", "webm",
];

#[derive(Debug, Clone)]
pub struct DownloadPaths {
    pub root: PathBuf,
    pub incomplete: PathBuf,
}

impl DownloadPaths {
    pub fn new(root: PathBuf) -> Result<Self, AppError> {
        Self::from_root(root, true)
    }

    /// Construct paths for a previously selected root without recreating it if
    /// the user removed it while a job was queued.
    pub fn new_existing(root: PathBuf) -> Result<Self, AppError> {
        Self::from_root(root, false)
    }

    fn from_root(root: PathBuf, allow_create: bool) -> Result<Self, AppError> {
        if allow_create {
            fs::create_dir_all(&root)?;
        }
        ensure_safe_directory(&root)?;
        let root = fs::canonicalize(root)?;
        ensure_safe_directory(&root)?;
        let incomplete = root.join(".incomplete");
        if !incomplete.exists() {
            fs::create_dir_all(&incomplete)?;
        }
        ensure_safe_directory(&incomplete)?;
        Ok(Self { root, incomplete })
    }

    pub fn job_dir(&self, job_id: &str) -> Result<PathBuf, AppError> {
        validate_job_id(job_id)?;
        ensure_safe_directory(&self.root)?;
        ensure_safe_directory(&self.incomplete)?;
        let path = self.incomplete.join(job_id);
        fs::create_dir_all(&path)?;
        ensure_safe_directory(&path)?;
        Ok(path)
    }

    pub fn output_path(&self, job_id: &str, extension: &str) -> Result<PathBuf, AppError> {
        validate_job_id(job_id)?;
        let extension = extension.trim_start_matches('.').to_ascii_lowercase();
        if !ALLOWED_EXTENSIONS.contains(&extension.as_str()) {
            return Err(AppError::Security("下載輸出副檔名不在允許清單".to_string()));
        }
        Ok(self.root.join(format!("{job_id}.{extension}")))
    }

    /// 只接受 yt-dlp 本次工作資料夾中產生的單一最終媒體檔。
    pub fn find_completed_file(&self, job_id: &str) -> Result<PathBuf, AppError> {
        let dir = self.job_dir(job_id)?;
        let mut candidates = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            // 使用 DirEntry::file_type／symlink_metadata，而非 metadata()；後者
            // 會跟隨 symlink/reparse point，可能讓輸出逃離固定暫存目錄。
            let file_type = entry.file_type()?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)?;
            let reparse = is_reparse_point(&path)?;
            if !file_type.is_file()
                || file_type.is_symlink()
                || !metadata.file_type().is_file()
                || metadata.file_type().is_symlink()
                || reparse
            {
                continue;
            }
            if metadata.len() == 0 {
                continue;
            }
            let Some(extension) = path.extension().and_then(|value| value.to_str()) else {
                continue;
            };
            if ALLOWED_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()) {
                candidates.push(path);
            }
        }
        if candidates.len() != 1 {
            return Err(AppError::Process(format!(
                "工作 {job_id} 應有一個完成檔，實際找到 {} 個",
                candidates.len()
            )));
        }
        Ok(candidates.remove(0))
    }

    pub fn finalize(&self, job_id: &str) -> Result<PathBuf, AppError> {
        let source = self.find_completed_file(job_id)?;
        // Repeat the directory checks immediately before moving the completed
        // file.  This narrows (but cannot eliminate) a same-user replacement
        // race between the earlier scan and finalization.
        ensure_safe_directory(&self.root)?;
        ensure_safe_directory(&self.incomplete)?;
        let job_dir = self.incomplete.join(job_id);
        ensure_safe_directory(&job_dir)?;
        let extension = source
            .extension()
            .and_then(|value| value.to_str())
            .ok_or_else(|| AppError::Process("完成檔缺少副檔名".to_string()))?;
        let destination = self.output_path(job_id, extension)?;
        if destination.exists() {
            return Err(AppError::Process(
                "輸出檔已存在，為避免覆寫而停止".to_string(),
            ));
        }
        fs::rename(&source, &destination)?;
        if job_dir.exists() {
            let _ = fs::remove_dir_all(job_dir);
        }
        Ok(destination)
    }
}

fn ensure_safe_directory(path: &Path) -> Result<(), AppError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| AppError::Security(format!("輸出目錄不存在或無法存取：{error}")))?;
    if !metadata.is_dir() || is_reparse_point(path).map_err(AppError::Io)? {
        return Err(AppError::Security(
            "輸出目錄及 .incomplete 不可是 symlink、junction 或 reparse point".to_string(),
        ));
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
        let wide = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
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

fn validate_job_id(value: &str) -> Result<(), AppError> {
    if value.len() != 36
        || !value
            .chars()
            .all(|character| character.is_ascii_hexdigit() || character == '-')
    {
        return Err(AppError::Internal("無效的內部工作識別碼".to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_output_under_fixed_root_and_rejects_traversal() {
        let root =
            std::env::temp_dir().join(format!("downloader-path-test-{}", uuid::Uuid::new_v4()));
        let paths = DownloadPaths::new(root.clone()).expect("paths");
        let id = uuid::Uuid::new_v4().to_string();
        let output = paths.output_path(&id, "mp4").expect("output");
        assert!(output.starts_with(&paths.root));
        assert!(paths.output_path("..\\escape", "mp4").is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_zero_byte_media_as_completed_output() {
        let root = std::env::temp_dir().join(format!(
            "downloader-zero-output-test-{}",
            uuid::Uuid::new_v4()
        ));
        let paths = DownloadPaths::new(root.clone()).expect("paths");
        let id = uuid::Uuid::new_v4().to_string();
        let job_dir = paths.job_dir(&id).expect("job dir");
        fs::write(job_dir.join("download.mp4"), []).expect("write zero output");
        assert!(paths.find_completed_file(&id).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn rejects_symlink_media_when_symlink_is_available() {
        use std::os::windows::fs::symlink_file;

        let root = std::env::temp_dir().join(format!(
            "downloader-symlink-output-test-{}",
            uuid::Uuid::new_v4()
        ));
        let paths = DownloadPaths::new(root.clone()).expect("paths");
        let id = uuid::Uuid::new_v4().to_string();
        let job_dir = paths.job_dir(&id).expect("job dir");
        let target = root.join("outside.mp4");
        fs::write(&target, b"outside").expect("target");
        let linked = job_dir.join("download.mp4");
        if symlink_file(&target, &linked).is_ok() {
            assert!(is_reparse_point(&linked).expect("reparse check"));
            assert!(paths.find_completed_file(&id).is_err());
        }
        let _ = fs::remove_dir_all(root);
    }
}
