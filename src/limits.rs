// 设计与安全上限参考 firecrawl/anydoc 0.2.5（MIT，`src/package/limits.rs`），
// 见 THIRD_PARTY_NOTICES.md。
//! 统一资源安全限额（防解压炸弹、病态嵌套、单元格展开放大）。
//!
//! 这些阈值是针对恶意/畸形输入的硬闸——真实办公与电子书文档通常低几个量级。
//! 触及上限统一返回 [`ErrorCode::ResourceLimit`](crate::error::ErrorCode::ResourceLimit)，
//! 不走回退链（引擎视为确定性终止码）。

use std::io::Read;

use crate::error::{DocError, Result};

/// 单个容器条目解压后字节上限：128 MiB。
pub const MAX_ENTRY_BYTES: u64 = 128 * 1024 * 1024;

/// 单个容器累计解压字节上限：512 MiB。
pub const MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;

/// 单个容器条目数上限。
pub const MAX_ENTRY_COUNT: usize = 100_000;

/// XML 元素最大嵌套深度。
pub const MAX_XML_DEPTH: usize = 256;

/// 单份 XML 部件最大节点数（元素 + 文本节点）。
pub const MAX_XML_NODES: usize = 2_000_000;

/// 单张表格最大网格槽位数（400 万格）。
pub const MAX_GRID_SLOTS: u64 = 4_000_000;

/// 重复展开（如 ODF `number-*-repeated`）产生的有内容单元格数上限。
pub const MAX_EXPANSION: u64 = 4_000_000;

/// 重复展开复制文本总字节上限：64 MiB。
pub const MAX_EXPANSION_TEXT_BYTES: u64 = 64 * 1024 * 1024;

/// 内嵌资源（图片等）单文档保留字节上限：128 MiB。
pub const MAX_ASSET_TOTAL_BYTES: usize = 128 * 1024 * 1024;

/// 二进制记录容器（老版 PPT/OLE 记录流）最大嵌套深度。
pub const MAX_RECORD_DEPTH: usize = 64;

/// 二进制记录流最大遍历记录总数。
pub const MAX_RECORDS: u64 = 16_000_000;

/// 校验容器条目数是否超过 [`MAX_ENTRY_COUNT`]。
pub fn check_entry_count(count: usize, label: &str) -> Result<()> {
    if count > MAX_ENTRY_COUNT {
        return Err(DocError::resource_limit(format!(
            "{label} 条目数 {count} 超过安全上限 ({MAX_ENTRY_COUNT})"
        )));
    }
    Ok(())
}

/// 有界读取：最多读取 `max_bytes` 字节；若底层流产出超过 `max_bytes` 字节
/// （例如 ZIP 解压炸弹），立即中止并返回 [`DocError::resource_limit`]。
pub fn read_bounded<R: Read>(reader: R, max_bytes: u64, label: &str) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    let cap = max_bytes.saturating_add(1);
    reader
        .take(cap)
        .read_to_end(&mut buf)
        .map_err(|e| DocError::corrupt(format!("读取 {label} 失败: {e}")))?;
    if buf.len() as u64 > max_bytes {
        return Err(DocError::resource_limit(format!(
            "{label} 解压大小超过安全上限 ({max_bytes} 字节)"
        )));
    }
    Ok(buf)
}

/// 跨条目累计字节预算（防“大量中等条目叠加吃爆内存”）。
#[derive(Debug, Clone, Copy)]
pub struct ByteBudget {
    used: u64,
    limit: u64,
}

impl ByteBudget {
    pub fn new(limit: u64) -> Self {
        Self { used: 0, limit }
    }

    /// 容器默认累计预算（[`MAX_TOTAL_BYTES`]）。
    pub fn archive_default() -> Self {
        Self::new(MAX_TOTAL_BYTES)
    }

    /// 剩余可用字节数。
    pub fn remaining(&self) -> u64 {
        self.limit.saturating_sub(self.used)
    }

    /// 记账 `bytes` 字节；超额返回 [`DocError::resource_limit`]。
    pub fn consume(&mut self, bytes: u64, label: &str) -> Result<()> {
        let next = self.used.saturating_add(bytes);
        if next > self.limit {
            return Err(DocError::resource_limit(format!(
                "{label} 累计解压字节超过安全上限 ({} 字节)",
                self.limit
            )));
        }
        self.used = next;
        Ok(())
    }

    /// 结合单条目上限与剩余总预算执行有界读取，并在成功后自动记账。
    pub fn read_entry<R: Read>(&mut self, reader: R, label: &str) -> Result<Vec<u8>> {
        let cap = MAX_ENTRY_BYTES.min(self.remaining());
        let data = read_bounded(reader, cap, label)?;
        self.consume(data.len() as u64, label)?;
        Ok(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;

    #[test]
    fn read_bounded_accepts_exact_limit_and_rejects_overflow() {
        let exact = [b'a'; 16];
        let got = read_bounded(&exact[..], 16, "exact").unwrap();
        assert_eq!(got.len(), 16);

        let over = [b'a'; 17];
        let err = read_bounded(&over[..], 16, "over").unwrap_err();
        assert_eq!(err.code, ErrorCode::ResourceLimit);
        assert!(err.message.contains("over"), "{}", err.message);
    }

    #[test]
    fn check_entry_count_enforces_cap() {
        assert!(check_entry_count(MAX_ENTRY_COUNT, "ok").is_ok());
        let err = check_entry_count(MAX_ENTRY_COUNT + 1, "bomb").unwrap_err();
        assert_eq!(err.code, ErrorCode::ResourceLimit);
    }

    #[test]
    fn byte_budget_tracks_cumulative_usage() {
        let mut budget = ByteBudget::new(10);
        let a = budget.read_entry(&b"123456"[..], "a").unwrap();
        assert_eq!(a.len(), 6);
        assert_eq!(budget.remaining(), 4);

        let b = budget.read_entry(&b"7890"[..], "b").unwrap();
        assert_eq!(b.len(), 4);
        assert_eq!(budget.remaining(), 0);

        let err = budget.read_entry(&b"x"[..], "c").unwrap_err();
        assert_eq!(err.code, ErrorCode::ResourceLimit);
    }
}
