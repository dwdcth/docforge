// 吸收自 docparser-office 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：crate 引用重写（M3 与 xlsx 一并启用）。
//! `merge_cell_policy`：xlsx 合并单元格的三态处置（默认 `mark`）。
//!
//! * `mark` —— 只在表格块 `meta["merges"]` 里标注区域，行数据原样（历史行为）；
//! * `expand` —— 把合并区左上角的值铺满整个区域，行数据稠密成按列号对齐的矩形；
//! * `ignore` —— 不标注也不展开：`meta["merges"]` 恒为空数组。
//!
//! 单元格/区域引用（`A1`、`A1:B2`、`$A$1:$B$2`）的解析也放这里，
//! xlsx 后端只负责「拿到文本后怎么摆」。

use std::str::FromStr;

/// 合并单元格策略（对应 Python `policy.merge_cell_policy`，默认 `mark`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MergeCellPolicy {
    /// 只标注（`meta["merges"]`），不改行数据
    #[default]
    Mark,
    /// 左上角的值铺满合并区，行稠密成按列号对齐
    Expand,
    /// 既不标注也不展开（`meta["merges"] = []`）
    Ignore,
}

impl MergeCellPolicy {
    /// 策略名（CLI / 日志 / Python `policy` 取值一致）
    pub fn as_str(self) -> &'static str {
        match self {
            MergeCellPolicy::Mark => "mark",
            MergeCellPolicy::Expand => "expand",
            MergeCellPolicy::Ignore => "ignore",
        }
    }
}

impl FromStr for MergeCellPolicy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "mark" => Ok(MergeCellPolicy::Mark),
            "expand" => Ok(MergeCellPolicy::Expand),
            "ignore" => Ok(MergeCellPolicy::Ignore),
            other => Err(format!(
                "unknown merge_cell_policy `{other}`（可选 mark / expand / ignore）"
            )),
        }
    }
}

/// 单元格引用 → `(0-based 行, 0-based 列)`；`$A$1` 的绝对符号忽略。
///
/// 只认「字母 + 数字」两种成分（`$` 跳过），其它字符（空格、`:`、越界符号）
/// 一律返回 `None`，交由调用方忽略这条引用——合并标注是附加信息，不该拖垮整表。
pub fn parse_cell_ref(r: &str) -> Option<(usize, usize)> {
    let mut col = 0usize;
    let mut letters = 0usize;
    let mut row = 0usize;
    let mut digits = 0usize;
    for c in r.chars() {
        match c {
            '$' => {}
            _ if c.is_ascii_alphabetic() && digits == 0 => {
                col = col * 26 + (c.to_ascii_uppercase() as u8 - b'A' + 1) as usize;
                letters += 1;
            }
            _ if c.is_ascii_digit() => {
                row = row
                    .checked_mul(10)?
                    .checked_add((c as u8 - b'0') as usize)?;
                digits += 1;
            }
            _ => return None,
        }
    }
    // 「A」「1」这类半边引用不合法；行号 0 也不存在（Excel 从 1 起）
    if letters == 0 || digits == 0 || row == 0 {
        return None;
    }
    Some((row - 1, col - 1))
}

/// 合并区引用 → `((r1, c1), (r2, c2))`，已规范化为左上/右下（0-based，闭区间）。
///
/// `mergeCell` 必然是 `起:止` 形式；单单元格（没有 `:`）不算合并区，返回 `None`。
pub fn parse_merge_ref(r: &str) -> Option<((usize, usize), (usize, usize))> {
    let (a, b) = r.split_once(':')?;
    let (ra, ca) = parse_cell_ref(a)?;
    let (rb, cb) = parse_cell_ref(b)?;
    Some(((ra.min(rb), ca.min(cb)), (ra.max(rb), ca.max(cb))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_names_and_default() {
        assert_eq!(MergeCellPolicy::default(), MergeCellPolicy::Mark);
        assert_eq!(
            "expand".parse::<MergeCellPolicy>().unwrap(),
            MergeCellPolicy::Expand
        );
        assert_eq!(
            " IGNORE ".parse::<MergeCellPolicy>().unwrap(),
            MergeCellPolicy::Ignore
        );
        assert_eq!(
            "".parse::<MergeCellPolicy>().unwrap(),
            MergeCellPolicy::Mark
        );
        assert!("spread".parse::<MergeCellPolicy>().is_err());
        assert_eq!(MergeCellPolicy::Mark.as_str(), "mark");
    }

    #[test]
    fn cell_ref_parsing() {
        assert_eq!(parse_cell_ref("A1"), Some((0, 0)));
        assert_eq!(parse_cell_ref("B2"), Some((1, 1)));
        assert_eq!(parse_cell_ref("AA10"), Some((9, 26)));
        // 绝对引用：绝对符号忽略
        assert_eq!(parse_cell_ref("$C$7"), Some((6, 2)));
        // 不合法：半边引用 / 行号 0 / 混入其它字符
        assert_eq!(parse_cell_ref("A"), None);
        assert_eq!(parse_cell_ref("1"), None);
        assert_eq!(parse_cell_ref("A0"), None);
        assert_eq!(parse_cell_ref("A1 B2"), None);
        assert_eq!(parse_cell_ref(""), None);
    }

    #[test]
    fn merge_ref_normalizes_corners() {
        assert_eq!(parse_merge_ref("A1:B2"), Some(((0, 0), (1, 1))));
        // 反向给出（D4:B2）也归到左上/右下
        assert_eq!(parse_merge_ref("D4:B2"), Some(((1, 1), (3, 3))));
        assert_eq!(parse_merge_ref("$A$1:$A$3"), Some(((0, 0), (2, 0))));
        // 单单元格不是合并区；坏引用只丢这一条
        assert_eq!(parse_merge_ref("A1"), None);
        assert_eq!(parse_merge_ref("A1:"), None);
    }
}
