//! docforge-cli 集成测试：格式转换批处理语义（多输入 / 目录递归 / --out-dir / chunks / 格式清单）。
//!
//! bin `docforge-cli` 的 `required-features = ["json"]`，因此测试需在
//! `--features json` 下运行：`cargo test --features json --test cli_convert`。
//!
//! Cargo.toml 增量（主管接线；加上后 default `cargo test` 会整体跳过本目标）：
//!
//! ```toml
//! [[test]]
//! name = "cli_convert"
//! path = "tests/cli_convert.rs"
//! required-features = ["json"]
//! ```
//!
//! 本文件同时以 `#![cfg(feature = "json")]` 自门控：在 [[test]] 接线生效前的
//! 过渡期，default `cargo test`（无 json feature）会把整个文件编译为空
//!（env! CARGO_BIN_EXE 不会被求值），保持全绿；`--features json` 下照常运行。
//! 临时文件放 temp_dir + pid 子目录，测试结束清理。

#![cfg(feature = "json")]

use std::path::PathBuf;
use std::process::Command;

/// 临时目录：temp_dir + pid 子目录（各测试互不干扰），先清同名残留。
fn tmp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("docforge_cli_{}_{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 起子进程跑 CLI，返回 (退出码, stdout, stderr)。
fn run_cli(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_docforge-cli"))
        .args(args)
        .output()
        .expect("启动 docforge-cli 失败");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// 单文件 → md → stdout：退出 0，stdout 含 Markdown 标题。
#[test]
fn single_file_to_md_stdout() {
    let dir = tmp_dir("single");
    let file = dir.join("note.md");
    std::fs::write(&file, "# 标题\n\n正文段落。\n").unwrap();
    let path = file.to_str().unwrap().to_string();

    let (code, stdout, stderr) = run_cli(&[&path, "--format", "md"]);

    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "退出码应为 0，stderr: {stderr}");
    assert!(stdout.contains("# "), "stdout 应含 Markdown 标题: {stdout}");
    assert!(stdout.contains("标题"), "stdout 应含标题文本: {stdout}");
    assert!(stdout.contains("正文段落"), "stdout 应含正文: {stdout}");
}

/// 目录批量 → --out-dir：3 文件含 1 个不支持扩展名 → out_dir 出现 2 个 .md，
/// 退出码 2（部分失败），stderr 有失败行与汇总。
#[test]
fn directory_batch_to_out_dir() {
    let dir = tmp_dir("batch");
    let out = tmp_dir("batch_out");
    std::fs::write(dir.join("a.md"), "# 甲\n\n第一段。\n").unwrap();
    std::fs::write(dir.join("b.md"), "# 乙\n\n第二段。\n").unwrap();
    std::fs::write(dir.join("c.xyz"), "不支持的扩展名内容").unwrap();

    let (code, stdout, stderr) = run_cli(&[
        dir.to_str().unwrap(),
        "--format",
        "md",
        "--out-dir",
        out.to_str().unwrap(),
    ]);

    let md_count = std::fs::read_dir(&out)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .map(|x| x == "md")
                .unwrap_or(false)
        })
        .count();
    let a_ok = std::fs::read_to_string(out.join("a.md"))
        .map(|s| s.contains("甲"))
        .unwrap_or(false);
    let b_ok = std::fs::read_to_string(out.join("b.md"))
        .map(|s| s.contains("乙"))
        .unwrap_or(false);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&out);

    assert_eq!(code, 2, "部分失败应退出 2，stderr: {stderr}");
    assert_eq!(md_count, 2, "out_dir 应恰好有 2 个 .md");
    assert!(a_ok && b_ok, "两个 md 输出内容应正确");
    assert!(stderr.contains("c.xyz"), "stderr 应有不支持扩展名的失败行: {stderr}");
    assert!(stderr.contains("成功 2 / 失败 1"), "stderr 应有汇总: {stderr}");
    assert!(stdout.is_empty(), "批量模式不应写 stdout: {stdout}");
}

/// --format chunks：stdout 首行为 JSONL，含 block_ids 数组与 c0 锚点。
#[test]
fn chunks_format_jsonl() {
    let dir = tmp_dir("chunks");
    let file = dir.join("doc.md");
    std::fs::write(&file, "# 第一章 夜行\n\n他提着灯走过长街。\n\n巷口的猫叫了一声。\n")
        .unwrap();
    let path = file.to_str().unwrap().to_string();

    let (code, stdout, stderr) = run_cli(&[&path, "--format", "chunks"]);

    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "退出码应为 0，stderr: {stderr}");
    let first = stdout.lines().next().unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(first)
        .unwrap_or_else(|e| panic!("首行应为 JSON（JSONL）: {e}\n{stdout}"));
    assert!(
        v.get("block_ids").map(|x| x.is_array()).unwrap_or(false),
        "首行应含 block_ids 数组: {v}"
    );
    assert_eq!(
        v.get("id").and_then(|x| x.as_str()),
        Some("c0"),
        "首块锚点应为 c0: {v}"
    );
    assert!(v.get("text").is_some() && v.get("chapter").is_some(), "{v}");
}

/// --list-formats：退出 0，输出含默认组、feature 门控组与恒开的 txt。
#[test]
fn list_formats_runs() {
    let (code, stdout, stderr) = run_cli(&["--list-formats"]);
    assert_eq!(code, 0, "退出码应为 0，stderr: {stderr}");
    assert!(stdout.contains("txt"), "应含恒开的 txt: {stdout}");
    assert!(stdout.contains("[默认]"), "应含默认组标题: {stdout}");
    assert!(stdout.contains("[feature 门控]"), "应含门控组标题: {stdout}");
}
