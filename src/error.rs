// 吸收自 docparser-core 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：仅 crate 名引用替换。
//! 统一错误分类（与 Python `errors.py` / KB 错误语义对齐）。

use std::fmt;

/// 全局统一错误码。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    Encrypted,   // 加密 / 权限受限
    Corrupt,     // 文件损坏 / 后端链耗尽
    Unsupported, // 格式不支持 / 伪装
    Timeout,     // 单文件 / 单页超时
    NativeCrash, // 底层 native 崩溃
    Unknown,
}

impl ErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorCode::Encrypted => "encrypted",
            ErrorCode::Corrupt => "corrupt",
            ErrorCode::Unsupported => "unsupported",
            ErrorCode::Timeout => "timeout",
            ErrorCode::NativeCrash => "native_crash",
            ErrorCode::Unknown => "unknown",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 解析错误：统一码 + 可读描述 + 已尝试后端序列。
#[derive(Debug, Clone)]
pub struct DocError {
    pub code: ErrorCode,
    pub message: String,
    pub attempts: Vec<String>,
}

impl DocError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            attempts: Vec::new(),
        }
    }
    pub fn encrypted(m: impl Into<String>) -> Self {
        Self::new(ErrorCode::Encrypted, m)
    }
    pub fn corrupt(m: impl Into<String>) -> Self {
        Self::new(ErrorCode::Corrupt, m)
    }
    pub fn unsupported(m: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unsupported, m)
    }
    pub fn timeout(m: impl Into<String>) -> Self {
        Self::new(ErrorCode::Timeout, m)
    }
}

impl fmt::Display for DocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for DocError {}

pub type Result<T> = std::result::Result<T, DocError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_display_and_constructors() {
        assert_eq!(ErrorCode::Encrypted.as_str(), "encrypted");
        assert_eq!(DocError::encrypted("x").code, ErrorCode::Encrypted);
        assert_eq!(DocError::corrupt("x").code, ErrorCode::Corrupt);
        assert_eq!(DocError::unsupported("x").code, ErrorCode::Unsupported);
        assert_eq!(DocError::timeout("x").code, ErrorCode::Timeout);
        assert_eq!(format!("{}", DocError::corrupt("bad")), "[corrupt] bad");
    }

    #[test]
    fn error_is_send_and_boxable() {
        let e: Box<dyn std::error::Error + Send> = Box::new(DocError::corrupt("x"));
        assert!(e.to_string().contains("corrupt"));
    }
}
