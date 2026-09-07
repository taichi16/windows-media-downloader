//! 固定資源路徑與 SHA-256 manifest 驗證。
//!
//! 儲存庫不攜帶 Windows 執行檔；使用者或發行流程必須把受信任來源的
//! `yt-dlp.exe`、`ffmpeg.exe`、`ffprobe.exe`（全部必要）放入 resources/bin，
//! 並在 resources 目錄提供 sidecar-manifest.json。任何缺少 manifest、
//! hash 格式錯誤或 hash 不符都會 fail closed。

use std::{
    collections::HashMap,
    fs,
    io::{BufReader, Read},
    path::{Path, PathBuf},
};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager};

use crate::error::AppError;

const MANIFEST_NAME: &str = "sidecar-manifest.json";
const BIN_DIRECTORY: &str = "bin";
const YTDLP_NAME: &str = "yt-dlp.exe";
const FFMPEG_NAME: &str = "ffmpeg.exe";
const FFPROBE_NAME: &str = "ffprobe.exe";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SidecarManifest {
    pub schema_version: u32,
    pub files: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct SidecarPaths {
    pub yt_dlp: PathBuf,
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

/// Tauri Windows 的 `resource_dir()` 指向可執行檔所在目錄；resources glob
/// 會在其下產生 `resources/`，因此所有 portable 路徑均從 exe 目錄明確組合。
pub fn resource_root_from_executable_dir(executable_dir: &Path) -> PathBuf {
    executable_dir.join("resources")
}

pub fn resource_root(app: &AppHandle) -> Result<PathBuf, AppError> {
    let executable_dir = app
        .path()
        .resource_dir()
        .map_err(|err| AppError::Sidecar(format!("無法取得執行檔目錄：{err}")))?;
    Ok(resource_root_from_executable_dir(&executable_dir))
}

pub fn load_manifest(root: &Path) -> Result<SidecarManifest, AppError> {
    let path = root.join(MANIFEST_NAME);
    let contents = fs::read_to_string(&path).map_err(|err| {
        AppError::Sidecar(format!("找不到 {MANIFEST_NAME}（必須 fail closed）：{err}"))
    })?;
    let manifest: SidecarManifest = serde_json::from_str(&contents)
        .map_err(|err| AppError::Sidecar(format!("manifest 格式錯誤：{err}")))?;
    if manifest.schema_version != 1 {
        return Err(AppError::Sidecar(
            "不支援的 manifest schemaVersion".to_string(),
        ));
    }
    for required in [YTDLP_NAME, FFMPEG_NAME, FFPROBE_NAME] {
        if !manifest.files.contains_key(required) {
            return Err(AppError::Sidecar(format!(
                "manifest 必須包含 {required} 的 SHA-256"
            )));
        }
    }
    for (name, hash) in &manifest.files {
        if !matches!(name.as_str(), YTDLP_NAME | FFMPEG_NAME | FFPROBE_NAME) {
            return Err(AppError::Sidecar(format!(
                "manifest 含有不允許的檔名：{name}"
            )));
        }
        if !is_sha256(hash) {
            return Err(AppError::Sidecar(format!(
                "{name} 的 SHA-256 必須是 64 個十六進位字元"
            )));
        }
    }
    Ok(manifest)
}

pub fn resolve_and_verify(root: &Path) -> Result<SidecarPaths, AppError> {
    let manifest = load_manifest(root)?;
    let bin_dir = root.join(BIN_DIRECTORY);
    reject_unlisted_executables(&bin_dir, &manifest)?;
    let yt_dlp = verify_entry(&bin_dir, YTDLP_NAME, &manifest)?;
    let ffmpeg = verify_entry(&bin_dir, FFMPEG_NAME, &manifest)?;
    let ffprobe = verify_entry(&bin_dir, FFPROBE_NAME, &manifest)?;
    Ok(SidecarPaths {
        yt_dlp,
        ffmpeg,
        ffprobe,
    })
}

pub fn resolve_and_verify_from_app(app: &AppHandle) -> Result<SidecarPaths, AppError> {
    resolve_and_verify(&resource_root(app)?)
}

fn verify_entry(
    bin_dir: &Path,
    name: &str,
    manifest: &SidecarManifest,
) -> Result<PathBuf, AppError> {
    let path = safe_bin_path(bin_dir, name)?;
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(AppError::Sidecar(format!("缺少必要 sidecar：{name}")));
    }
    if metadata.len() == 0 {
        return Err(AppError::Sidecar(format!("sidecar 不得為空檔案：{name}")));
    }
    verify_hash(
        &path,
        manifest.files.get(name).expect("required manifest entry"),
    )?;
    Ok(path)
}

fn reject_unlisted_executables(bin_dir: &Path, manifest: &SidecarManifest) -> Result<(), AppError> {
    let entries = fs::read_dir(bin_dir)
        .map_err(|err| AppError::Sidecar(format!("無法讀取 sidecar 目錄：{err}")))?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name
            .to_str()
            .ok_or_else(|| AppError::Sidecar("bin 中含無法驗證檔名的資源".to_string()))?;
        let file_type = entry.file_type()?;
        if name_str.eq_ignore_ascii_case(".gitkeep")
            && file_type.is_file()
            && !file_type.is_symlink()
        {
            continue;
        }
        if file_type.is_symlink()
            || !file_type.is_file()
            || !matches!(name_str, YTDLP_NAME | FFMPEG_NAME | FFPROBE_NAME)
            || !manifest.files.contains_key(name_str)
        {
            return Err(AppError::Sidecar(format!(
                "bin 中有未宣告或不允許的 sidecar 資源：{name_str}"
            )));
        }
    }
    Ok(())
}

fn safe_bin_path(bin_dir: &Path, name: &str) -> Result<PathBuf, AppError> {
    if !matches!(name, YTDLP_NAME | FFMPEG_NAME | FFPROBE_NAME) {
        return Err(AppError::Sidecar("不允許的 sidecar 檔名".to_string()));
    }
    Ok(bin_dir.join(name))
}

fn verify_hash(path: &Path, expected: &str) -> Result<(), AppError> {
    let file = fs::File::open(path)
        .map_err(|err| AppError::Sidecar(format!("讀取 sidecar {} 失敗：{err}", path.display())))?;
    let mut reader = BufReader::with_capacity(1024 * 1024, file);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = reader.read(&mut buffer).map_err(|err| {
            AppError::Sidecar(format!("讀取 sidecar {} 失敗：{err}", path.display()))
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual = hex_lower(&hasher.finalize());
    if actual != expected.to_ascii_lowercase() {
        return Err(AppError::Sidecar(format!(
            "sidecar SHA-256 不符：{}",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("unknown")
        )));
    }
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.as_bytes().iter().all(u8::is_ascii_hexdigit)
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn manifest_rejects_placeholder_and_unknown_paths() {
        let dir = tempfile_dir();
        let manifest = dir.join(MANIFEST_NAME);
        fs::write(
            &manifest,
            r#"{"schemaVersion":1,"files":{"yt-dlp.exe":"REPLACE_WITH_64_HEX_SHA256"}}"#,
        )
        .expect("write manifest");
        assert!(load_manifest(&dir).is_err());
    }

    #[test]
    fn verifies_known_file_hash() {
        let dir = tempfile_dir();
        let bin = dir.join(BIN_DIRECTORY);
        fs::create_dir_all(&bin).expect("create bin");
        let mut hashes = HashMap::new();
        for name in [YTDLP_NAME, FFMPEG_NAME, FFPROBE_NAME] {
            let mut file = fs::File::create(bin.join(name)).expect("create sidecar");
            file.write_all(name.as_bytes()).expect("write sidecar");
            hashes.insert(name, hex_lower(&Sha256::digest(name.as_bytes())));
        }
        fs::write(
            dir.join(MANIFEST_NAME),
            serde_json::json!({ "schemaVersion": 1, "files": hashes }).to_string(),
        )
        .expect("write manifest");
        let resolved = resolve_and_verify(&dir).expect("valid sidecar");
        assert_eq!(resolved.yt_dlp, bin.join(YTDLP_NAME));
        assert_eq!(resolved.ffmpeg, bin.join(FFMPEG_NAME));
        assert_eq!(resolved.ffprobe, bin.join(FFPROBE_NAME));
    }

    #[test]
    fn resource_root_is_adjacent_to_executable_directory() {
        let executable_dir = PathBuf::from(r"C:\portable");
        assert_eq!(
            resource_root_from_executable_dir(&executable_dir),
            PathBuf::from(r"C:\portable\resources")
        );
    }

    #[test]
    fn rejects_unlisted_executable_or_library_in_bin() {
        let dir = tempfile_dir();
        let bin = dir.join(BIN_DIRECTORY);
        fs::create_dir_all(&bin).expect("create bin");
        for name in [YTDLP_NAME, FFMPEG_NAME, FFPROBE_NAME, "unexpected.exe"] {
            fs::write(bin.join(name), name.as_bytes()).expect("write sidecar");
        }
        let files = HashMap::from([
            (
                YTDLP_NAME.to_string(),
                hex_lower(&Sha256::digest(YTDLP_NAME.as_bytes())),
            ),
            (
                FFMPEG_NAME.to_string(),
                hex_lower(&Sha256::digest(FFMPEG_NAME.as_bytes())),
            ),
            (
                FFPROBE_NAME.to_string(),
                hex_lower(&Sha256::digest(FFPROBE_NAME.as_bytes())),
            ),
        ]);
        fs::write(
            dir.join(MANIFEST_NAME),
            serde_json::json!({ "schemaVersion": 1, "files": files }).to_string(),
        )
        .expect("write manifest");
        assert!(resolve_and_verify(&dir).is_err());
        fs::remove_file(bin.join("unexpected.exe")).expect("remove executable");
        fs::write(bin.join("unexpected.dll"), b"library").expect("write library");
        assert!(resolve_and_verify(&dir).is_err());
        fs::remove_file(bin.join("unexpected.dll")).expect("remove library");
        fs::create_dir(bin.join("unexpected-directory")).expect("create directory");
        assert!(resolve_and_verify(&dir).is_err());
    }

    fn tempfile_dir() -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("windows-downloader-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }
}
