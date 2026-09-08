# 离线 sketch 证据与 o11y：最终报告

面向开发者。对应 #322 的离线证据路径，不包含实时误差反馈。原有脏工作区未修改。

## 完成的路径

- 版本化 artifact、JSON Schema 和公共 CostModel provider，按参数、查询语义、分布、环境和有效期匹配。未知测量保持 unavailable。
- CMS／CountSketch 真实参数扫描：离线误差、分离的构造／更新／读取／合并 CPU、存活堆内存、序列化大小和文件块占用。
- 固定快照 point-frequency 比较：显式观测误差门槛、formal 参数下限、CPU／retained byte-seconds 权重及同数据 exact baseline。
- Backend 先过滤不支持的布局，再推荐和绑定 frequency 配置；exact 胜出或证据不足时保留原查询，不擅自把选中参数取整。
- o11y planner/control-plane replay，以及七条显式支持查询的 exact 固定快照参考测量。

接口与复现见 [证据契约](developer_docs/offline-sketch-evidence.md)、
[benchmark 命令](../tools/empirical-bench/README.md) 和 [replay 指南](user-guide/o11y-replay.md)。

## 实测结果

sketch-bench 固定 revision `87f619e843fd2e4da784160d4e205a0d0d55f032`。
Uniform／Zipf 各 20,000 个 i64 key，key-space 1,000、seed 42、Zipf 指数 1.1。
共 18 个 sketch 配置、两个 exact baseline；CPU 五次测量、两次预热。
准确率是每个固定数据集的一次离线实验，不是跨分布保证或实时 ground truth。
比较场景为一次构建、1,000 次查询、保留 300 秒、不合并。

| 场景 | Uniform / Zipf 结果 |
| --- | --- |
| CPU-only，1% 或 5% 观测平均相对误差上限 | 都选择 exact；没有 sketch CPU 收益 |
| 通用比较器，memory-weighted | CMS 2720×5；retained heap 减少 81.68%，CPU 分别增加约 0.037 / 0.182 ms |
| Backend 可绑定的 memory-weighted 配置 | CMS 4096×5；retained heap 减少约 72.4%，CPU 同样增加 |

memory-weighted 目标为 `CPU ns + 0.01 × retained byte-seconds`；权重是示例偏好，
不是硬件测得的换算关系。Exact retained heap 为 296,976 B；CMS 2720×5 为
54,400 B，4096×5 为 81,920 B。内存由独立 System 分配器探针测得申请字节，
CPU 使用 jemalloc；不等同于进程 RSS。序列化和文件块占用不代表磁盘吞吐收益。

详见 [最终扫描报告](../tools/empirical-bench/results-sweep/MEASUREMENTS.md)。
`results/` 中旧频率测量及旧 control-plane replay 保留为初始基线；最终频率比较和
控制面结果以 `results-sweep/` 为准，不混用两轮成本。

## o11y 覆盖及限制

复用仓库已有 27 条 PromQL fixture，不运行上游 LLM-agent 评分或真实场景数据。

| 检查 | 结果 |
| --- | --- |
| Planner 候选覆盖 | 26/27 有 exact summary 候选，1/27 raw fallback；没有 sketch 候选 |
| 原始 replay 完整生命周期成本 | 证据不足，27/27 保守 raw fallback |
| Control-plane exact/default/empirical 绑定 | 均 27/27 成功；其中各 5 条根计划仍保留原查询 |
| 七条 exact 参考查询 | 固定合成 gauge 快照上测 raw 与缓存结果读取，经过公共 CostModel 和真实选择流程 |
| 其余 20 条查询 | 缺少完整参考实现，收益 unavailable |

新增 selector、sort、comparison/filter 根的原始 IR 保留回退，使绑定由 24/27 提升
为 27/27。绑定不等于全部可由 summary executor 执行。新 IR 的列更新已兼容；
keyed/non-column 更新和 warm-tier BinaryOp 仍明确拒绝。

七条参考查询覆盖瞬时 sum、窗口 max 和 sum-of-window-average：300 series、三个 job、
每分钟一个有限 gauge 样本，重复读取同一快照 60 次。保留的是完整 exact 查询结果，
不是推进时间的滑动窗口。测量排除网络、磁盘、协议序列化和输出标签物化；内存是
逻辑值字节。不能把重复读取的收益外推为线上端到端加速。
机器可读结果见 [exact 快照实验](../tools/empirical-bench/results/o11y-exact-snapshot.json)。

Quantile 测量、真实 o11y trace、时间漂移、合并后误差、在线更新与退休成本及完整线上
收益仍未覆盖。Point-frequency 误差不能借给 quantile 或 count_over_time。

## 验证与交付

- Planner mapping：349 项单元测试通过。
- Devtools：exact benchmark 3 项、replay 4 项通过；推荐 CLI 用六个实际请求验证。
- Benchmark 导出：5 项 Python 测试通过。
- Backend：control-plane 633 项、data-plane 973 项 library 测试通过；新增 7 项集成测试通过。
- Provider、测量与绑定代码经过 agent 交叉审查；产物文本、JSON、源码哈希和 provenance 核对通过。

Backend 固定依赖已发布 planner 核心提交 `dfdf6b5c1f7d667394a4ea1f56fb3786af04228d`，
普通构建和最终 replay 无需本地 Cargo source patches。源码、工具和结果随两个仓库的
配套 PR 交付，不自动合并或关闭 issue。
