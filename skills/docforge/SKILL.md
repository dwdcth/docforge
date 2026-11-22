---
name: docforge
description: Convert Office documents (.docx, .doc, .xlsx, .xls, .xlsb, .pptx, .ppt), OpenDocument (.odt, .ods, .odp), PDF, ebooks (.epub, .fb2, .mobi, .azw3, .chm, .djvu, .cbz), archives (.zip, .rar, .7z), RTF, HTML, CSV, and subtitles into clean GitHub-Flavored Markdown (GFM) or Google Open Knowledge Format (OKF v0.3) knowledge bundles with extracted image assets. Use when you need to read, extract, or ingest non-plain-text documents or build an OKF knowledge base.
license: MIT OR Apache-2.0
---

# Convert Documents with `docforge-cli`

Use `docforge-cli` (a single self-contained native binary with all formats enabled) to convert documents into **GFM** (`--format gfm`, default) or **Google OKF v0.3** (`--format okf`).

## Quick Commands

```bash
# Convert a single document to GitHub-Flavored Markdown (stdout)
docforge-cli report.docx

# Write GFM to a file (recommended for large documents)
docforge-cli report.pdf -o report.md

# Convert a single document to Google OKF v0.3 (YAML 1.2 frontmatter + assets/ + index.md)
docforge-cli report.docx --format okf --out-dir ./kb

# Batch-convert a directory into a complete OKF v0.3 Knowledge Bundle
docforge-cli ./docs --format okf --okf-type Reference --okf-title "Project Docs" --out-dir ./kb

# Batch-convert a directory into GFM files preserving directory layout
docforge-cli ./docs --format gfm --out-dir ./out_md

# Inspect all supported input formats
docforge-cli --list-formats
```

## Supported Input Formats

- **Modern Office (OOXML)**: `.docx`, `.docm`, `.dotx`, `.dotm`, `.xlsx`, `.xlsm`, `.xltx`, `.xltm`, `.pptx`, `.pptm`, `.ppsx`, `.ppsm`, `.potx`, `.potm`
- **Legacy Binary Office**: `.doc`, `.ppt`, `.pps`, `.pot`, `.xls`, `.xlsb`
- **OpenDocument**: `.odt`, `.ods`, `.odp`
- **PDF & Ebooks**: `.pdf` (text layer), `.epub`, `.fb2` (`.fb2.zip`), `.mobi`, `.prc`, `.azw3`, `.chm`, `.djvu`, `.cbz`
- **Text, Markup & Archives**: `.txt` (auto CJK/UTF-8 encoding detection), `.md`, `.html`/`.htm`, `.csv`, `.rtf`, `.srt`/`.vtt`/`.ass`/`.ssa`, `.zip`, `.rar`, `.7z`, `.png`/`.jpg`/`.webp`/`.bmp`/`.tiff`

## Rules & Best Practices

1. **Choose `gfm` vs `okf`**:
   - Use `--format gfm` (default) when you just need clean Markdown text (tables properly escaped, OMML/MathML formulas converted to LaTeX `$...$` / `$$...$$`, dynamic code fences).
   - Use `--format okf --out-dir <dir>` when building a structured knowledge base or preserving embedded images. It outputs OKF v0.3 Concept files (YAML 1.2 frontmatter with `type`, `generated`, `sources`, `x-docforge-*`), deduplicated images under `assets/`, and progressive-disclosure `index.md` files (root `index.md` with `okf_version: "0.3"`, subdirectory `index.md` without frontmatter).
2. **Large documents**: Always write to disk with `-o <file>` or `--out-dir <dir>` and read selectively instead of flooding stdout/context.
3. **Format detection**: Single-file inputs are detected by file content (bounded header magic + ZIP/OLE probing), not just extension. Directory traversal uses the supported extension allowlist and reports unsupported files on stderr without silently dropping them.
4. **Reserved OKF names**: In `--format okf` mode, input files named `index.*` or `log.*` are automatically renamed (e.g. `index-doc.md`) to avoid colliding with OKF bundle `index.md` / `log.md`.
5. **Exit codes**:
   - `0`: All inputs converted successfully.
   - `1`: Invalid arguments or all inputs failed.
   - `2`: Partial failure in batch mode (some files succeeded, some failed; check stderr for per-file diagnostics).
6. **Safety limits**: Built-in resource limits protect against ZIP bombs, deeply nested XML, and oversized embedded assets without crashing.
7. **Installation**: Download prebuilt binaries (`docforge-cli`) from GitHub Releases for Windows (`x86_64`), macOS (`x86_64`, `arm64`), and Linux (`x86_64-gnu`, `x86_64-musl`, `aarch64`, `loongarch64`), or build from source with `cargo build --release --features cli --bin docforge-cli`.
