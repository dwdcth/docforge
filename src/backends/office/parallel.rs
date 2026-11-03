// 吸收自 docparser-office 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：crate 引用重写（docparser_core→docforge、office 内部→super）。
//! 容器内并行：把「各个 sheet / 各个 slide」分块丢给线程，**保序**回收。
//!
//! 刻意不引 `rayon`：
//! 1. 并发粒度只有「部件」一级（通常 1–20 个），不需要工作窃取；
//! 2. 这是要发布的库，每多一个依赖就多一份下游审计面；
//! 3. 行为要可预测——线程数、切块方式、错误顺序都写死在这一个文件里。
//!
//! 语义约定：
//! - **顺序**：切的是连续块，块内顺序 + 块序拼接 = 原顺序，调用方无需再排；
//! - **错误**：按块序取第一个错误上抛，与串行时「先遇到先返回」一致；
//! - **panic**：降级成 `internal` 错误，不让库把 panic 漏给宿主进程。

use crate::error::{DocError, ErrorCode, Result};

/// 并行度上限：取 `available_parallelism`，可用 `DOCPARSER_JOBS` 覆盖（1 = 强制串行）。
///
/// 给环境变量是有意的：库被嵌进别人的服务里时，线程池该多大由调用方说了算，
/// 不该由我们猜。
fn jobs() -> usize {
    if let Ok(v) = std::env::var("DOCPARSER_JOBS") {
        if let Ok(n) = v.trim().parse::<usize>() {
            return n.max(1);
        }
    }
    std::thread::available_parallelism()
        .map(|v| v.get())
        .unwrap_or(1)
}

/// 按连续块并行映射，结果拼回原顺序。`f` 每次拿到一整块（便于块内复用同一个句柄，
/// 例如每块开一次 ZIP、块内多个 sheet 共用）。
///
/// 块内只需保证「输入顺序进 = 输出顺序出」，函数不做额外排序。
pub(crate) fn map_chunks_ordered<T, R, F>(items: Vec<T>, f: F) -> Result<Vec<R>>
where
    T: Send,
    R: Send,
    F: Fn(Vec<T>) -> Result<Vec<R>> + Sync,
{
    let n = items.len();
    let workers = jobs().min(n);
    // 串行兜底：条目太少（1 个）或强制单线程时，老老实实一遍走完
    if workers < 2 {
        return f(items);
    }

    // 连续切块：`chunk` 向上取整，最后一块可能短
    let chunk = n.div_ceil(workers);
    let mut chunks: Vec<Vec<T>> = Vec::with_capacity(workers);
    let mut rest = items;
    while !rest.is_empty() {
        let at = chunk.min(rest.len());
        let tail = rest.split_off(at);
        chunks.push(rest);
        rest = tail;
    }

    // `scope` 借用栈上数据，不需要 'static；`f` 只读共享
    let per_chunk: Vec<Result<Vec<R>>> = std::thread::scope(|scope| {
        let handles: Vec<_> = chunks.into_iter().map(|c| scope.spawn(|| f(c))).collect();
        handles
            .into_iter()
            .map(|h| {
                h.join().unwrap_or_else(|_| {
                    Err(DocError::new(
                        ErrorCode::Unknown,
                        "并行解析线程 panic（内部错误）",
                    ))
                })
            })
            .collect()
    });

    // 按块序收敛错误：块 0 出错就先返回块 0 的错
    let mut out = Vec::with_capacity(n);
    for part in per_chunk {
        out.extend(part?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_order_and_drops_nothing() {
        let items: Vec<usize> = (0..37).collect();
        let out = map_chunks_ordered(items, |chunk| {
            Ok(chunk.into_iter().map(|x| x * 2).collect())
        })
        .unwrap();
        let want: Vec<usize> = (0..37).map(|x| x * 2).collect();
        assert_eq!(out, want);
    }

    #[test]
    fn single_item_falls_back_to_serial() {
        let out = map_chunks_ordered(vec![7usize], Ok).unwrap();
        assert_eq!(out, vec![7]);
    }

    #[test]
    fn first_error_wins_in_chunk_order() {
        let items: Vec<usize> = (0..8).collect();
        let err = map_chunks_ordered(items, |chunk| {
            // 第一块就报错，后面块即使成功也不该被采纳
            if chunk.contains(&0) {
                return Err(DocError::corrupt("block0"));
            }
            Ok(chunk)
        })
        .unwrap_err();
        assert!(err.to_string().contains("block0"), "{err}");
    }
}
