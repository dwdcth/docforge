//! docforge-cli 集成测试：全格式文档转换（仅输出 GFM 与 Google OKF v0.3）。

#![cfg(feature = "cli")]

use std::path::PathBuf;
use std::process::Command;

fn tmp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("docforge_cli_{}_{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

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

/// 默认输出格式为 GFM：单文件直接输出到 stdout，退出码 0。
#[test]
fn single_file_defaults_to_gfm_stdout() {
    let dir = tmp_dir("default_gfm");
    let file = dir.join("spec.md");
    std::fs::write(&file, "# 规格\n\n| 列A | 列B |\n|---|---|\n| 1 | 2 |\n").unwrap();
    let path = file.to_str().unwrap().to_string();

    let (code, stdout, stderr) = run_cli(&[&path]);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("# 规格"), "{stdout}");
    assert!(stdout.contains("| 列A | 列B |"), "{stdout}");
}

/// 目录批量 → GFM `--out-dir`：3 文件含 1 个不支持扩展名 → 退出码 2（部分失败）。
#[test]
fn directory_batch_to_gfm_out_dir() {
    let dir = tmp_dir("batch");
    let out = tmp_dir("batch_out");
    std::fs::write(dir.join("a.md"), "# 甲\n\n第一段。\n").unwrap();
    std::fs::write(dir.join("b.md"), "# 乙\n\n第二段。\n").unwrap();
    std::fs::write(dir.join("c.xyz"), "不支持的扩展名内容").unwrap();

    let (code, stdout, stderr) = run_cli(&[
        dir.to_str().unwrap(),
        "--format",
        "gfm",
        "--out-dir",
        out.to_str().unwrap(),
    ]);

    let md_count = std::fs::read_dir(&out)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "md").unwrap_or(false))
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
    assert!(a_ok && b_ok, "两个 GFM 输出内容应正确");
    assert!(stderr.contains("c.xyz"), "stderr 应有不支持扩展名的失败行: {stderr}");
    assert!(stderr.contains("成功 2 / 失败 1"), "stderr 应有汇总: {stderr}");
    assert!(stdout.is_empty(), "批量模式不应写 stdout: {stdout}");
}

/// 非 gfm/okf 输出格式应被拒绝（退出码 1）。
#[test]
fn rejects_non_gfm_or_okf_format() {
    let dir = tmp_dir("reject_fmt");
    let file = dir.join("note.md");
    std::fs::write(&file, "# 标题\n").unwrap();
    let path = file.to_str().unwrap().to_string();

    for fmt in ["json", "md", "text", "units", "chunks"] {
        let (code, _stdout, stderr) = run_cli(&[&path, "--format", fmt]);
        assert_eq!(code, 1, "格式 {fmt} 应拒绝，stderr: {stderr}");
        assert!(stderr.contains("gfm | okf"), "stderr: {stderr}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// --help / --version / --list-formats 均以退出码 0 正常输出。
#[test]
fn meta_flags_work() {
    let (code, stdout, stderr) = run_cli(&["--help"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("gfm|okf"), "{stdout}");
    assert!(stdout.contains("--ocr"), "{stdout}");
    assert!(stdout.contains("--ocr-mirror"), "{stdout}");

    let (code, stdout, stderr) = run_cli(&["--version"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("docforge-cli "), "{stdout}");

    let (code, stdout, stderr) = run_cli(&["--list-formats"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    for expected in [
        "docx", "doc", "ppt", "xls", "xlsb", "odt", "pdf", "epub", "ppocr", "indra", "gfm", "okf",
    ] {
        assert!(stdout.contains(expected), "缺少 {expected}: {stdout}");
    }
}

/// 非法 --ocr / --ocr-mirror 值返回退出码 1；数字文档搭配 --ocr ppocr/indra 不触发模型下载。
#[test]
fn ocr_flags_validation_and_lazy_digital_docs() {
    let dir = tmp_dir("ocr_flags");
    let file = dir.join("note.md");
    std::fs::write(&file, "# 标题\n\n纯数字正文。\n").unwrap();
    let path = file.to_str().unwrap().to_string();

    let (code, _stdout, stderr) = run_cli(&[&path, "--ocr", "unknown-engine"]);
    assert_eq!(code, 1, "stderr: {stderr}");
    assert!(stderr.contains("ppocr"), "{stderr}");

    let (code, _stdout, stderr) = run_cli(&[&path, "--ocr-mirror", "mars"]);
    assert_eq!(code, 1, "stderr: {stderr}");
    assert!(stderr.contains("auto | cn | global"), "{stderr}");

    // 懒加载验证：纯数字文档即便显式指定 --ocr ppocr / --ocr indra + 不存在的离线目录也零开销成功
    let fake_models = dir.join("nonexistent_models");
    for mode in ["ppocr", "ppocr-tiny", "indra", "off"] {
        let (code, stdout, stderr) = run_cli(&[
            &path,
            "--ocr",
            mode,
            "--ocr-model-dir",
            fake_models.to_str().unwrap(),
            "--ocr-mirror",
            "cn",
        ]);
        assert_eq!(code, 0, "mode={mode} stderr: {stderr}");
        assert!(stdout.contains("纯数字正文"), "mode={mode} stdout: {stdout}");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// --format okf 目录批量：生成标准 OKF v0.3 Bundle（根 index.md 含 okf_version，
/// 子目录 index.md 无 frontmatter，保留名 index.md 自动避让为 index-doc.md）。
#[test]
fn directory_batch_to_okf_bundle() {
    let dir = tmp_dir("okf_in");
    let sub = dir.join("guides");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(dir.join("index.md"), "# 首页概览\n\n这是顶层说明文档。\n").unwrap();
    std::fs::write(sub.join("deploy.md"), "# 部署手册\n\n按步骤完成服务上线。\n").unwrap();

    let out = tmp_dir("okf_out");
    let (code, _stdout, stderr) = run_cli(&[
        dir.to_str().unwrap(),
        "--format",
        "okf",
        "--okf-type",
        "Playbook",
        "--okf-title",
        "Ops Knowledge",
        "--out-dir",
        out.to_str().unwrap(),
    ]);

    let root_idx = std::fs::read_to_string(out.join("index.md")).unwrap_or_default();
    let sub_idx = std::fs::read_to_string(out.join("guides/index.md")).unwrap_or_default();
    let renamed = std::fs::read_to_string(out.join("index-doc.md")).unwrap_or_default();
    let deploy = std::fs::read_to_string(out.join("guides/deploy.md")).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&out);

    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(root_idx.starts_with("---\nokf_version: \"0.3\"\n"), "{root_idx}");
    assert!(root_idx.contains("title: \"Ops Knowledge\""), "{root_idx}");
    assert!(root_idx.contains("index-doc.md"), "{root_idx}");
    assert!(root_idx.contains("guides/"), "{root_idx}");
    assert!(!sub_idx.starts_with("---"), "子目录 index.md 不可带 frontmatter: {sub_idx}");
    assert!(sub_idx.contains("deploy.md"), "{sub_idx}");
    assert!(renamed.contains("type: \"Playbook\""), "{renamed}");
    assert!(deploy.contains("type: \"Playbook\""), "{deploy}");
    assert!(deploy.contains("generated: { by: \"docforge/"), "{deploy}");
}

/// --format okf 图片资产落盘：含图片时写出 assets/*.png 并在正文链接。
#[test]
fn okf_extracts_image_assets_to_disk() {
    let dir = tmp_dir("okf_img_in");
    let out = tmp_dir("okf_img_out");
    let png_bytes: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8,
        0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D, 0xB0, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];
    let img = dir.join("diagram.png");
    std::fs::write(&img, png_bytes).unwrap();

    let (code, _stdout, stderr) = run_cli(&[
        img.to_str().unwrap(),
        "--format",
        "okf",
        "--ocr",
        "off",
        "--out-dir",
        out.to_str().unwrap(),
    ]);

    let md = std::fs::read_to_string(out.join("diagram.md")).unwrap_or_default();
    let asset = out.join("assets/diagram-p1-img1.png");
    let asset_ok = std::fs::read(&asset).map(|b| b == png_bytes).unwrap_or(false);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&out);

    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(asset_ok, "图片资产应原样落盘到 assets/");
    assert!(
        md.contains("./assets/diagram-p1-img1.png"),
        "正文应使用显式相对路径链接图片资产: {md}"
    );
    assert!(md.contains("x-docforge-assets: 1"), "frontmatter 应记录资产数: {md}");
    assert!(
        stderr.contains("纯扫描版") && stderr.contains("--ocr ppocr"),
        "关闭 OCR 处理纯图文档应在 stderr 提示启用 OCR: {stderr}"
    );
}

/// --format gfm 落盘输出（-o）同样自动提取图片资产到相对 assets/ 并用 `./assets/...` 链接。
#[test]
fn gfm_to_file_extracts_relative_image_assets() {
    let dir = tmp_dir("gfm_img_in");
    let out = tmp_dir("gfm_img_out");
    let png_bytes: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8,
        0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D, 0xB0, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];
    let img = dir.join("chart.png");
    std::fs::write(&img, png_bytes).unwrap();
    let out_md = out.join("chart.md");

    let (code, _stdout, stderr) = run_cli(&[
        img.to_str().unwrap(),
        "--format",
        "gfm",
        "--ocr",
        "off",
        "-o",
        out_md.to_str().unwrap(),
    ]);

    let md = std::fs::read_to_string(&out_md).unwrap_or_default();
    let asset = out.join("assets/chart-p1-img1.png");
    let asset_ok = std::fs::read(&asset).map(|b| b == png_bytes).unwrap_or(false);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&out);

    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(asset_ok, "GFM 落盘模式也应将图片写入相对 assets/");
    assert!(
        md.contains("![image](./assets/chart-p1-img1.png)"),
        "GFM 正文应使用 ./assets/... 相对路径链接图片: {md}"
    );
}

/// 纯扫描版 PDF 在 `--ocr off` 下应在 stderr 明确提示启用 OCR，纯文本文件不误报。
#[test]
fn scanned_pdf_with_ocr_off_hints_enabling_ocr() {
    let dir = tmp_dir("scanned_pdf_hint");
    let pdf_path = dir.join("scan_only.pdf");
    let mut doc = oxidize_pdf::Document::new();
    let page = oxidize_pdf::Page::a4();
    doc.add_page(page);
    doc.save(&pdf_path).unwrap();

    let (code, _stdout, stderr) = run_cli(&[pdf_path.to_str().unwrap(), "--ocr", "off"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        stderr.contains("纯扫描版") && stderr.contains("--ocr ppocr") && stderr.contains("--ocr indra"),
        "纯扫描版 PDF 应在 stderr 提示启用 OCR: {stderr}"
    );

    // 正常文本文件关闭 OCR 不应误报扫描版提示
    let md_path = dir.join("normal.md");
    std::fs::write(&md_path, "# 正常文档\n\n含文字层正文。\n").unwrap();
    let (code2, _stdout2, stderr2) = run_cli(&[md_path.to_str().unwrap(), "--ocr", "off"]);
    assert_eq!(code2, 0, "stderr: {stderr2}");
    assert!(!stderr2.contains("纯扫描版"), "正常文档不应误报: {stderr2}");

    let _ = std::fs::remove_dir_all(&dir);
}
