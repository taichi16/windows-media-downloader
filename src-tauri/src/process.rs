use std::{path::Path, sync::Arc, time::Duration};

use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    sync::oneshot,
};

use crate::error::AppError;

pub const MAX_CAPTURED_OUTPUT: usize = 64 * 1024;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Default, Clone)]
pub struct ProcessOutput {
    pub stdout: String,
    pub stderr: String,
}

pub fn command(program: &Path, args: &[String]) -> Result<Command, AppError> {
    if !program.is_absolute() {
        return Err(AppError::Security("sidecar 路徑必須是絕對路徑".to_string()));
    }
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        command.creation_flags(CREATE_NO_WINDOW);
    }
    Ok(command)
}

pub async fn collect_output<R: AsyncRead + Unpin>(mut reader: R) -> Result<String, AppError> {
    let mut captured = Vec::with_capacity(MAX_CAPTURED_OUTPUT);
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        append_bounded(&mut captured, &buffer[..read]);
    }
    Ok(String::from_utf8_lossy(&captured).into_owned())
}

pub fn append_bounded(captured: &mut Vec<u8>, chunk: &[u8]) {
    if chunk.len() >= MAX_CAPTURED_OUTPUT {
        captured.clear();
        captured.extend_from_slice(&chunk[chunk.len() - MAX_CAPTURED_OUTPUT..]);
        return;
    }
    let overflow = captured
        .len()
        .saturating_add(chunk.len())
        .saturating_sub(MAX_CAPTURED_OUTPUT);
    if overflow > 0 {
        captured.drain(..overflow);
    }
    captured.extend_from_slice(chunk);
}

/// 在 Windows 使用由可信 OS API 解析的絕對路徑呼叫 taskkill `/T`，一次終止 yt-dlp 及其
/// 目前子程序；其他平台保留 `Child::kill` 的可編譯退化路徑，方便離線測試核心模組。
pub fn terminate_process_tree(child: &mut Child) -> Result<(), AppError> {
    let Some(pid) = child.id() else {
        return Ok(());
    };
    #[cfg(windows)]
    {
        // 使用 GetSystemDirectoryW 解析的 Windows 內建 taskkill 絕對路徑與 /T 遞迴選項，直接終止
        // yt-dlp 及其目前的 ffmpeg 子樹；沒有 shell、PATH 或可控命令列。
        // 啟動到取消之間仍可能有極短競態，因此再以 Child::start_kill
        // 作為父程序退化保護，這項限制也記錄於 SECURITY 文件。
        if let Ok(taskkill) = taskkill_path() {
            let pid_text = pid.to_string();
            let mut taskkill_command = std::process::Command::new(taskkill);
            use std::os::windows::process::CommandExt;
            taskkill_command.creation_flags(CREATE_NO_WINDOW);
            let taskkill_result = taskkill_command
                .args(["/PID", pid_text.as_str(), "/T", "/F"])
                .status();
            if let Ok(status) = taskkill_result {
                if !status.success() {
                    // 即使 taskkill 回報程序已消失，也繼續等待/回收 Child。
                    let _ = child.start_kill();
                }
            } else {
                let _ = child.start_kill();
            }
        } else {
            let _ = child.start_kill();
        }
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
    }
    // taskkill 可能已先回收程序；忽略 start_kill 的「已不存在」錯誤，
    // 由 wait() 負責最後的 handle 回收。
    let _ = child.start_kill();
    Ok(())
}

/// 直接以 PID 終止整個 Windows process tree，供取消命令在 async 工作之外
/// 立即觸發。呼叫端只可傳入本程式自己的 child PID。
#[cfg(windows)]
pub fn terminate_pid_tree(pid: u32) -> Result<(), AppError> {
    let taskkill = taskkill_path()?;
    let pid_text = pid.to_string();
    use std::os::windows::process::CommandExt;
    let mut taskkill_command = std::process::Command::new(taskkill);
    taskkill_command.creation_flags(CREATE_NO_WINDOW);
    let status = taskkill_command
        .args(["/PID", pid_text.as_str(), "/T", "/F"])
        .status()
        .map_err(|error| AppError::Process(format!("無法啟動 taskkill：{error}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(AppError::Process("taskkill 未能終止下載程序樹".to_string()))
    }
}

#[cfg(windows)]
fn taskkill_path() -> Result<std::path::PathBuf, AppError> {
    use std::fs;
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

    let mut buffer = [0_u16; 32768];
    let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
    if length == 0 || length as usize >= buffer.len() {
        return Err(AppError::Process(
            "無法取得 Windows System32 目錄".to_string(),
        ));
    }
    let directory = String::from_utf16(&buffer[..length as usize])
        .map_err(|_| AppError::Process("Windows System32 路徑不是有效 UTF-16".to_string()))?;
    let path = system_executable_path(Path::new(&directory));
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| AppError::Process(format!("找不到可信 taskkill.exe：{error}")))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(AppError::Process(
            "taskkill.exe 不是可信的一般檔案".to_string(),
        ));
    }
    Ok(path)
}

fn system_executable_path(system_directory: &Path) -> std::path::PathBuf {
    system_directory.join("taskkill.exe")
}

/// 等待程序結束或取消訊號；輪詢間隔固定且很短，避免取消懸掛。
pub async fn wait_or_cancel(
    child: &mut Child,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    mut cancel_rx: oneshot::Receiver<()>,
) -> Result<std::process::ExitStatus, AppError> {
    tokio::select! {
        result = child.wait() => result.map_err(AppError::from),
        _ = &mut cancel_rx => {
            terminate_process_tree(child)?;
            child.wait().await.map_err(AppError::from)
        }
        _ = wait_for_atomic(cancelled) => {
            terminate_process_tree(child)?;
            child.wait().await.map_err(AppError::from)
        }
    }
}

async fn wait_for_atomic(flag: Arc<std::sync::atomic::AtomicBool>) {
    while !flag.load(std::sync::atomic::Ordering::Acquire) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::system_executable_path;
    use super::{append_bounded, MAX_CAPTURED_OUTPUT};
    use std::path::Path;

    #[test]
    fn taskkill_path_is_derived_from_trusted_system_directory() {
        assert_eq!(
            system_executable_path(Path::new(r"D:\Windows\System32")),
            Path::new(r"D:\Windows\System32\taskkill.exe")
        );
    }

    #[test]
    fn bounded_capture_keeps_tail_of_oversized_output() {
        let mut captured = Vec::new();
        append_bounded(&mut captured, b"prefix");
        let tail = vec![b'x'; MAX_CAPTURED_OUTPUT + 17];
        append_bounded(&mut captured, &tail);
        assert_eq!(captured.len(), MAX_CAPTURED_OUTPUT);
        assert_eq!(captured, tail[17..]);
    }
}
