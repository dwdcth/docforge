# docforge 项目纪律

## 构建与门禁（mbx 全局缓存下的确定性）

本机 cargo 经 mise→mbx（mr-boxington）包装：target 是指向 `~/.cache/mbx/targets/v1/<hash>` 的符号链接，
跨项目共享内容寻址缓存 + 每小时自动 GC + learned incremental 状态。日常开发吃缓存没问题。

**验证性门禁（测试/clippy/发布前检查）必须用独立干净 target 目录**——mbx 在剧烈并行政改
+ 多 feature 组合频繁切换时出现过陈旧产物命中（幻影编译错/测试红，重跑自愈），干净目录杜绝：

```bash
export CARGO_TARGET_DIR=/tmp/docforge-gate-target   # 门禁专用，与日常缓存隔离
cargo test --all-features
```

- 同一项目避免多个 cargo 进程并发（cargo 有 target 锁会串行化，但 mbx 的 restore/GC 在锁外）；
  并行子代理流水线由主管串行调度门禁。
- 遇"改了不该相关的代码却红/重跑变绿"：先 `CARGO_TARGET_DIR=/tmp/x cargo test` 复验；
  确系缓存则 `mbx cache remove /mnt/s790/project/docforge`（精准清本工作区 managed target +
  learned state + claims，共享依赖对象保留，重建大头仍命中）——**不要**用全局 `mbx gc`。
- 子代理任务书里写验收命令时，一律带上面的 export 行。

## 工作流

- 实现工作派 sonnet 子代理：主管写任务书（文件边界互斥/验收命令含干净 target/报告格式）→ 并行执行 →
  主管接线共享文件 → 绿门禁自动 commit+push（规范 message + 子代理署名）。
- 回写器铁律：**回写侧对齐机制必须逐字节镜像解析侧行为**（锚点权威在解析产物，解析侧没做的
  span/merge 展开回写侧绝不做）——docx/html/odt 表格回写三代理已实证。
- 通用库不内置消费者适配（rumi 的适配写在 rumi 侧）；"事实谓词归库、策略归消费者"分层。
