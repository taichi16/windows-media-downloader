use std::fmt;

use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("輸入無效：{0}")]
    InvalidInput(String),
    #[error("安全檢查失敗：{0}")]
    Security(String),
    #[error("sidecar 檢查失敗：{0}")]
    Sidecar(String),
    #[error("檔案系統錯誤：{0}")]
    Io(#[from] std::io::Error),
    #[error("JSON 錯誤：{0}")]
    Json(#[from] serde_json::Error),
    #[error("下載程序錯誤：{0}")]
    Process(String),
    #[error("下載已取消")]
    Cancelled,
    #[error("內部錯誤：{0}")]
    Internal(String),
}

impl Serialize for AppError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl From<url::ParseError> for AppError {
    fn from(value: url::ParseError) -> Self {
        Self::InvalidInput(format!("URL 無法解析：{value}"))
    }
}

impl fmt::Display for crate::security::Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
