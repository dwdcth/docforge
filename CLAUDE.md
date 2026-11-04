# docforge 项目纪律

## 构建与门禁（mbx 全局缓存：正常使用 + 按疑点核验）

本机 cargo 经 mise→mbx 包装：全局内容寻址缓存（跨项目共享依赖/制品）、
learned incremental 状态、每小时自动 GC。门禁与日常开发**一律走全局缓存**——
全 feature 矩阵是最贵的构建，绕开缓存得不偿失，独立临时目录是反模式
（既丢缓存收益，自己还会变成第二份越攒越陈的状态）。

防"陈旧产物幻影红"（历史仅在多代理并行政代码 + feature 组合高频切换时出现）
用**信任但核验**协议：

1. 门禁失败先重跑一次：仍红 = 真实失败，照常修；
2. 翻绿 / 结果自相矛盾（未动的模块红、局部编译错但对应源码完好）→
   `mbx cache remove /mnt/s790/project/docforge` 后重跑拿可信结果。
   该命令只清本工作区的 managed target + learned state + claims——**共享依赖
   对象保留**，重建时 deps 照常缓存命中，只重编本 crate（分钟级）。
   **不要**用全局 `mbx gc`（殃及全部活跃工作区）。

预防面：
- 并行子代理改代码期间，门禁由主管**串行**调度（cargo 有 target 锁天然串行，
  但避免与 mbx restore/GC 叠加竞争）；
- 一波并行改动接线完成后、跑全量门禁前，可预防性 `mbx cache remove
  <workspace>` 一次——成本仅本 crate 重编，换取门禁从干净一致的学习状态出发。
- **门禁组合面**：常规固定组合（default / docx,epub,fb2,html,odt,xlsx /
  --all-features / json）之外，**定期跑单 feature 扫描**
  （`for f in <全部 feature>; do cargo test --no-default-features --features $f; done`）
  ——历史审计实证两类漏网：cli_convert 曾因 json 单开缺文本族红（固定组合
  恒含 default 文本族，三轮门禁没抓到）；office 门控 bug（xlsx 单开编译不进
  解析栈）同属单开才暴露的类别。

## 工作流

- 实现工作派 sonnet 子代理：主管写任务书（文件边界互斥/验收命令/报告格式）→ 并行执行 →
  主管接线共享文件 → 绿门禁自动 commit+push（规范 message + 子代理署名）。
- **并行波纪律（缓存友好，契约清晰的机械波适用）**：子代理**只编辑不编译**——共享工作树下
  各自编译必然撞兄弟代理的飞行中文件；且每个中间源态都是内容寻址缓存（mbx）的新对象、
  零复用还污染 learned incremental 决策；同 target 的 cargo 锁还让编译互相排队。代理改完做
  纯代码自查（签名契约逐字比对/类型/借用/match 穷尽/调用点 `.into()` 形态）即交报告（标注
  "未编译待门禁"）；主管收口共享文件后**单次编译 + 修 bug + 全量门禁**——deps 全缓存命中、
  本 crate 只编一遍、学习状态干净。适用边界：契约能逐字写死的迁移/扩展波；探索型任务
  （需要编译器反馈迭代）不适用。更高并发的演进形态：每代理 git worktree + 各自 mbx
  managed target（锁/文件隔离），对象库全局共享、缓存收益不丢。
- 回写器铁律：**回写侧对齐机制必须逐字节镜像解析侧行为**（锚点权威在解析产物，解析侧没做的
  span/merge 展开回写侧绝不做）——docx/html/odt 表格回写三代理已实证。
- 通用库不内置消费者适配（rumi 的适配写在 rumi 侧）；"事实谓词归库、策略归消费者"分层。
