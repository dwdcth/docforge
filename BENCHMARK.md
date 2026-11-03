# BENCHMARK：解析吞吐与峰值内存

可引用的一组数字：fixture 程序合成（确定性、无随机、跑时生成、结束清理），
release 实测。回答三类格式决策问题：

1. **格式吞吐量级**——各格式喂给 docforge 的解析速度，供格式选择 / 容量规划参考；
2. **流式值不值**——引擎级 `parse_streaming` 与全量 `parse` 在文本件上的耗时对比
   （流式的内存收益场景在 pdf 千页件，见下"未测"）；
3. **单 case 真实峰值内存**——进程内存预算（`--single` 模式）。

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
- xlsx/pptx/odt/odf/odp/chm/cbz/fb2/mobi/djvu/压缩包：bench 未覆盖，需真实
  fixture 才有意义，后续按需补。

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
