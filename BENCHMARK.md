# BENCHMARK：解析吞吐与峰值内存

可引用的一组数字：fixture 程序合成（确定性、无随机、跑时生成、结束清理），
release 实测。回答四类格式决策问题：

1. **格式吞吐量级**——各格式喂给 docforge 的解析速度，供格式选择 / 容量规划参考；
2. **流式值不值**——引擎级 `parse_streaming` 与全量 `parse` 在文本件上的耗时对比
   （流式的内存收益场景在 pdf 千页件，见下"未测"）；
3. **单 case 真实峰值内存**——进程内存预算（`--single` 模式）；
4. **回写写侧基线**——翻译闭环回写（`writeback_*`）单程吞吐与增量成本，服务
   "流式 apply_edits" 改造的前后对照（表见下，数字待统一门禁采集）。

数字均在下述机器实测；复现命令见文末，任何机器可重跑校验。

## 方法学

- **fixture 合成**（`benches/throughput.rs`）：运行时生成到系统临时目录、跑完整体
  清理。确定性模式块循环（无随机源）：md = 标题/段落/列表/表格/代码块固定循环、
  html = h2/p/table 循环、txt = 段落循环、csv = 数字行循环（含表头）、
  docx = OOXML 段落循环、epub = 32 章 XHTML。目标大小 md/html/txt ≈ 5MB、
  csv/docx/epub ≈ 2MB（实际字节见表）。生成器流式写盘、只持有当前模式块
  （KB 级）——fixture 构造不抬高被测峰值内存。
- **docx/epub 容器**：手写最小合法 zip（STORED 无压缩条目 + CRC32），bench 因此
  不依赖 docforge 的可选 zip 依赖。吞吐按容器文件字节计；无压缩意味着容器字节
  ≈ 正文 XML/XHTML 字节（真实容器多为 deflate，口径偏大但量级一致）。
  docx 每 16 段插一个 `Heading2` 样式段——auto 结构探针命中、走 semantic 组装
  出块（与真实文档同路径）；epub 一章一页，spine 序 = 页序。
- **计时**：每 case 预热 1 轮 + 计时 3 轮取中位；吞吐 = 输入字节 / 中位耗时。
- **回写族（wb-\*）口径**：回写单程吞吐。setup（不计入计时）`parse` →
  `to_translation_units` → 每单元确定性假译文 `(id, "译:{原文}")` 二元组
  （`(id, 译)` 形态 = `source: None` 信任锚点）；计时只含 `writeback_*` 一次
  调用——回写器按对齐契约**内部重解析原文件**（与提取侧同解析器同默认选项），
  该成本计入，与真实翻译管线一致。策略统一 `BilingualInline`（译文紧跟原块）。
- **回写 fixture**：wb-txt 复用 5MB 文本件；wb-docx / wb-epub / wb-xlsx 在对应
  解析 fixture 的正文之上各加一个 1MB 确定性伪二进制条目（`word/media/image1.bin`
  / `OEBPS/media/image1.bin` / `xl/media/blob1.bin`）——锻炼 raw_copy 直拷路径
  （回写族已落地：未触碰条目不解压不重压）。wb-xlsx 为最小合法包（workbook +
  workbook rels + inlineStr 单 sheet ≈ 2MB，无 sst / styles——两侧解析对缺失
  条目均优雅回退）；格全为字符格且非空（格位 ↔ 格单元 1:1 对位、无类型守卫
  跳过）。吞吐按**输入容器总字节**计（含伪二进制条目，与解析表同口径）。
- **回写结果校验**：每轮断言 `WritebackReport.written == 送译单元数` 且
  `skipped` 为空（确定性 fixture 下每个假译文都应写入）；`written == 0` 直接
  判错拒报——防静默零写入的假吞吐。进度行打印 written。
- **回写增量对照**：`wb-docx-noread` = 同 fixture 只 parse 的计时行；`wb-docx`
  中位耗时 − 该行 ≈ 回写增量成本（扫描 + 改写 + 重打包 + 落盘；重解析与 parse
  同价）。
- **峰值内存**：`/proc/self/status` 的 `VmHWM`——进程生命期高点、**不可重置**。
  多 case 同进程跑只能报全程累计高点（默认模式只在表尾注明）；逐 case 真实峰值
  用 `--single` 模式（单进程单 case，外层循环逐 case 调用）。读数含
  `router::sniff` 的 `head_of` 整读截断瞬态（读全文件取前 8 字节——库现状，
  如实计入）与"预热 1 + 计时 3"轮次内存复用后的高点。
- **结果校验**：每轮检查产出页数 > 0（epub 32 页、其余 1 页）；流式路径校验
  sink 实收页数与 `meta.page_count` 一致。
- **速率波动**：同 case 中位耗时受后台负载影响明显（本机采集时同 case 重复跑
  差距可达数倍），看量级、别抠个位数；重要对比请各自重跑多次。

## 机器环境

| 项 | 值 |
| --- | --- |
| CPU | AMD Ryzen 7 PRO 8845HS w/ Radeon 780M Graphics（8C16T） |
| 内存 | MemTotal 32,176,872 kB（约 32GB） |
| rustc | 1.99.1 (58cf8f9c 2026-10-13) |
| 内核 | 6.18.18.c1126-trim |
| profile | release（`cargo bench` 的 bench profile 默认继承 release，opt-level=3） |
| 采集日期 | 2026-11-04 |

## 未测（如实声明）

- **pdf（含"千页 PDF 深度流式峰值 = 单页"）**：本机无 pdftoppm 渲染链与 OCR
  模型，扫描件深度流式场景无法如实测出——不编数字，留待具备 OCR 环境的机器按
  同一方法学补测 `docforge::backends::pdf::parse_streaming`（页产即吐、峰值 =
  单页是设计口径，无实测数字背书前不应引用）。
- pptx/odt/odf/odp/chm/cbz/fb2/mobi/djvu/压缩包：bench 未覆盖，需真实
  fixture 才有意义，后续按需补（xlsx 已进回写族 `wb-xlsx`；解析表尚无 xlsx 行）。

## 数字：默认 feature 面（txt/md/csv/html，含流式对比）

采集时 1 分钟负载 1.63（后台编译间隙窗口，同配置连跑三次中位耗时一致）：

```
docforge 解析吞吐基准（fixture 程序合成）
方法学：每 case 预热 1 轮 + 计时 3 轮取中位；吞吐 = 输入字节 / 中位耗时；字节 = fixture 实际大小

case                     字节         中位耗时           吞吐  VmHWM
------------ -------------- ------------ ------------  ----------
txt               5,243,105       8.8 ms   568.3 MB/s  -
md                5,243,816      18.1 ms   276.2 MB/s  -
html              5,243,220      15.4 ms   325.0 MB/s  -
csv               2,097,184      24.5 ms    81.8 MB/s  -
txt-stream        5,243,105       6.5 ms   771.4 MB/s  -
md-stream         5,243,816      18.9 ms   265.1 MB/s  -

VmHWM（累计）= 42.1 MB：多 case 同进程跑，VmHWM 是进程生命期高点、不可重置，只反映全程峰值；逐 case 真实峰值请用 --single 模式逐个跑（见 --help）。
```

## 数字：加 epub/docx feature

采集时 1 分钟负载 1.66：

```
case                     字节         中位耗时           吞吐  VmHWM
------------ -------------- ------------ ------------  ----------
txt               5,243,105       8.8 ms   570.1 MB/s  -
md                5,243,816      18.5 ms   270.0 MB/s  -
html              5,243,220      15.7 ms   317.6 MB/s  -
csv               2,097,184      24.7 ms    81.1 MB/s  -
txt-stream        5,243,105       6.4 ms   786.6 MB/s  -
md-stream         5,243,816      18.1 ms   277.0 MB/s  -
docx              2,097,699       5.1 ms   390.5 MB/s  -
epub              2,120,137       4.0 ms   499.9 MB/s  -

VmHWM（累计）= 42.6 MB：多 case 同进程跑，VmHWM 是进程生命期高点、不可重置，只反映全程峰值；逐 case 真实峰值请用 --single 模式逐个跑（见 --help）。
```

## 数字：逐 case 真实峰值内存（--single，默认面）

采集时 1 分钟负载 1.19-2.30（同窗口；txt-stream 为同窗口单独补跑的一条）。
**VmHWM 与负载无关、可直接引用；耗时列分进程跑、受残余负载影响更大，引用吞吐
请以上面两张表为准。**

```
case                     字节         中位耗时           吞吐  VmHWM
------------ -------------- ------------ ------------  ----------
txt               5,243,105      11.8 ms   423.2 MB/s  23.9 MB
md                5,243,816      29.8 ms   167.7 MB/s  31.5 MB
html              5,243,220      16.8 ms   298.5 MB/s  26.1 MB
csv               2,097,184      24.6 ms    81.2 MB/s  41.4 MB
txt-stream        5,243,105       9.0 ms   553.9 MB/s  24.6 MB
md-stream         5,243,816      17.9 ms   279.8 MB/s  32.3 MB
```

（txt-stream / md-stream 与 txt / md 同一 fixture，VmHWM 逐 case 对比即流式 vs
全量的峰值内存差：txt 24.6 vs 23.9、md 32.3 vs 31.5 MB——流式不高于全量，且
同 fixture 多次跨进程跑 VmHWM 稳定在 ±0.7MB 内。）

## 数字：回写吞吐（wb-*，feature 面 docx+epub+xlsx）

回写吞吐表（默认模式；`written` = 实际写入单元数 = 送译单元数，bench 逐轮断言）：

```
docforge 回写吞吐基准（回写单程：setup 的译文对构造不计入计时，writeback_* 内部重解析计入；wb-*-noread = 同 fixture 只-parse 对照行）

case               输入字节       中位耗时         吞吐  written  VmHWM
---------------- -------------- ------------ ------------  --------  ----------
wb-txt             5,243,105      28.7 ms   174.0 MB/s     8555      -
wb-docx            3,146,393      20.3 ms   148.2 MB/s     6326      -
wb-docx-noread     3,146,393       5.6 ms   536.2 MB/s        -      -
wb-epub            3,168,833      17.8 ms   170.1 MB/s       32      -
wb-xlsx            3,147,129      24.6 ms   121.8 MB/s     5680      -
```

（默认模式同进程跑，VmHWM 列只报累计口径、表尾注明——与解析表同规矩。）

逐 case 真实峰值（--single；wb case 的 VmHWM 另含 setup 整批译文对的驻留——
翻译管线回写前的真实形态）：

```
case               输入字节       中位耗时         吞吐  written  VmHWM
---------------- -------------- ------------ ------------  --------  ----------
wb-txt             5,243,105      48.4 ms   103.3 MB/s     8555   85.6 MB
wb-docx            3,146,393      45.8 ms    65.6 MB/s     6326   38.2 MB
wb-docx-noread     3,146,393       6.2 ms   483.1 MB/s        -   16.4 MB
wb-epub            3,168,833      18.0 ms   167.7 MB/s       32   25.2 MB
wb-xlsx            3,147,129      29.0 ms   103.5 MB/s     5680   37.3 MB
```

### 改造后复测（流式 apply_edits + 输入 Cursor + 64KB 批冲落地后）

多轮中位（与上表同口径）：

```
wb-txt      28.8 ms   173.7 MB/s     8555      -      （基线 28.7 —— 持平）
wb-docx     20.2 ms   148.5 MB/s     6326      -      （基线 20.3 —— 持平）
wb-epub     18.3 ms   165.0 MB/s       32      -      （基线 17.8 —— 持平）
wb-xlsx     25.2 ms   119.0 MB/s     5680      -      （基线 24.9 —— 持平）
```

--single 峰值（内存与负载无关，可比）：**wb-txt 85.6 → 68.1 MB（-20%）**；
wb-docx 38.2→37.5；wb-epub 25.2→26.4；wb-xlsx 37.3→39.6——zip 族持平
（拼接串消除 ≈ Cursor 整读成本：前者随 XML 体量增长、后者随整包体量增长；
纯文本大件净收益、图重件中性，时间面全程持平）。

过程记录（方法学）：① 流式初版因逐编辑碎片 write_all 产生时间回归
（docx 增量 15.1→22.5ms），加 64KB 批冲后追平——碎片化调用开销是流式
改造的隐性成本，改造时必须带批冲；② 对照期遇 mbx 陈旧 bench 产物致
全 case ~2× 假回归，`mbx cache remove <workspace>` 后复测恢复——跨版本
benchmark 对照先做同窗口配对（基线 worktree）再下结论。

### A/B：改写条目压缩等级（zlib-rs 后端，3MB 级包裹）

| 等级 | wb-docx --single | 结论 |
| --- | --- | --- |
| 默认（≈6） | 20.7 ms | 维持现状 |
| Level 1 | 17.0 ms（-18%） | 省时但输出 XML 压缩差 10-20%——一次性回写省 3.7ms 无感、体积增大持久，不采纳 |

（咨询假设的 miniz_oxide 45-65MB/s 前提对我们不成立——已是 zlib-rs 后端；
数字为实测。）

读数口径（回填时按此解读）：

- **VmHWM 组成**（wb case）：fixture 流式生成的少量分配 + setup（IR + 全单元 +
  全译文对 ≈ 2× 正文文本驻留）+ 回写单程（重解析中间态 + **拼接产物整份 String
  驻留**）。"流式 apply_edits" 改造要消掉的是最后一项——改造后同 fixture 复测
  本表，差值即改造的内存收益；吞吐列同时观察不回退。
- **增量对照**：`wb-docx` 中位耗时 − `wb-docx-noread` 中位耗时 ≈ 回写增量成本
  （扫描 + 改写 + 重打包 + 落盘）。
- **raw_copy 生效判据**：容器格（wb-docx/wb-epub/wb-xlsx）fixture 含 1MB 伪
  二进制条目——直拷生效时回写耗时应与**正文改写量**同量级而非与容器大小同
  量级；若接近全容器重压缩的耗时，说明直拷路径未吃到。

## 复现

```bash
# 1) 默认 feature 面（txt/md/csv/html + 流式对比）：吞吐表
cargo bench --bench throughput

# 2) 加 epub/docx：容器格式两行
cargo bench --bench throughput --features epub,docx,html

# 3) 逐 case 真实峰值内存（VmHWM 进程生命期不可重置，必须单进程单 case）：
for c in $(cargo bench --bench throughput -- --list); do
  cargo bench --bench throughput -- --single "$c"
done

# 4) 回写族吞吐表（wb-*）：容器 case 需 feature；含 wb-docx-noread 对照行
cargo bench --bench throughput --features docx,epub,xlsx

# 5) 回写族逐 case 真实峰值（含 wb case：峰值另含整批译文对的 setup 驻留）
for c in $(cargo bench --bench throughput --features docx,epub,xlsx -- --list); do
  cargo bench --bench throughput --features docx,epub,xlsx -- --single "$c"
done
```

说明：老版本 cargo 的 `--bench throughput --release` 写法在新 cargo（1.99 实测）
已去掉 `--release` 别名——`cargo bench` 默认走 bench profile（继承 release 优化），
要显式声明就用 `--profile release`；两种写法产物等价。

## 数字怎么读（回答什么决策问题）

- **格式吞吐列**：同一批内容四种载体喂 docforge 的速度量级。txt 最快（纯文本
  直提）；md 附加块级语法解析、html 附加标签状态机，同档但更慢；csv 最慢——
  2MB 合成件 = 35,106 行全量进 `TableData` 结构化存储，代价在结构化而非文本。
  容量规划按"MB/s × 机器核数"估上限，本表为单线程口径。
- **`txt` vs `txt-stream` / `md` vs `md-stream`**：引擎级流式（`parse_streaming`）
  与全量 `parse` 的耗时对比。两口径同为单页件——流式在此**没有额外开销**
  （txt 上反而更快：全量路径 RawDoc + Document 双驻留的搬运被省掉），但"流式
  省内存"的真实收益场景在 pdf 千页件（页产即吐、峰值 = 单页），本表测不到
  （见"未测"）。对单页文本件，选 `parse_streaming` 的理由是消费模型（边产边耗、
  不攒 Document）而非性能。
- **VmHWM 列（--single）**：单 case 进程级峰值 ≈ "输入文件 + 解析中间态 + IR"
  的驻留高点，做内存预算时按此量级留余量。注意 csv 是峰值最高的文本格式
  （2MB 输入 → 41.4MB 峰值，约 20 倍：全表结构化驻留）；大 CSV 场景预算按输入
  的 20 倍留。默认模式的表尾 VmHWM 是多 case 累计高点，**不可**当作任何单 case
  的峰值引用。
- **回写吞吐表（wb-\*，数字待采集）**：回写单程含内部重解析，吞吐天然低于解析
  表同格式行，差值 = 翻译闭环写侧的增量（`wb-docx` − `wb-docx-noread` 即增量）。
  基线用途：回写族现状 = 拼接产物（改写后的正文 XML/整份文本）整份 String 驻留
  后一次性落盘；"流式 apply_edits" 改造目标是边改边写、不攒整份产物——改造前后
  各采一次回写表（吞吐列看是否回退、`--single` VmHWM 列看驻留下降），差值即
  改造收益。读数判据见上"数字：回写吞吐"节的读数口径。
