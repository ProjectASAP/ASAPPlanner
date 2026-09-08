# 离线 sketch 证据接入与 o11y 规划评估

> 本文保留第一轮实验的历史状态（包括当时未提交、24/27 绑定以及未测构造／磁盘）。
> 后续实现、最终结果和交付状态以 [最终报告](offline-o11y-final-2026-09-08.md) 为准。

## 执行结果

三个 agent 并行完成了离线证据 provider、真实 sketch-bench 测量和
planner replay；主 agent 完成 control-plane 接入、版本兼容和集成验证。
完整步骤见 [执行计划](design_docs/empirical-o11y-execution-plan.md)。

实现与测量使用独立 worktree，原有工作区的冲突和修改未动：

- Planner：`/mydata/asapplanner-empirical-o11y`，基线 `378a754`。
- Control plane：`/mydata/ASAPQuery-backend/.worktrees/empirical-o11y-322`，基线 `95131d8`。
- sketch-bench：`/mydata/sketch-bench-empirical-322`，固定 `87f619e843fd2e4da784160d4e205a0d0d55f032`，源码未修改。

没有提交、推送、发布 PR 或关闭 issue。两个产品 worktree 的改动仍可本地审查。

## #322 已实现的路径

1. 版本化 JSON artifact、JSON Schema、显式标注的合成测试 fixture，以及真实测量文件。
2. 离线误差、CPU、内存、分布、精确参数、运行环境、样本数、离散程度、有效期和来源信息。
3. 公共 `CostModel` 的测量排序接口，以及 control plane 对其实际 sizing 参数的证据查找。
4. 全部候选都有匹配的 update CPU 证据时按该指标排序；缺失、过期、配置／环境／分布不匹配或歧义时保留默认顺序。
5. 测试证明改变兼容证据会改变真实绑定结果，同时保持参数与准确率保证；实际测量支持默认 CMS 优先，未人为制造收益或决策变化。

这次排序目标是单次更新 CPU，不是 CPU、内存、磁盘的综合最优目标。
离线误差保留为对应查询的观测，不升级为形式化保证，也不直接用来缩小 sketch。
生命周期接口只提供有依据的部分成本；缺少语义匹配的 readout、retention、retirement
或 raw/residual 成本时，完整方案仍不可估计。实时 ground truth、自估误差和在线反馈均不在本次范围内。

## 数据与测量

使用真正的 sketch-bench，比较 CMS、CountSketch 和 Polars group-by + HashMap
精确频率索引。每个数据集有 20,000 个 i64 key，生成 key 空间为 1,000，seed=42；
uniform 实际 1,000 个 distinct key，Zipf（指数 1.1）实际 952 个。
CPU 每项 5 次测量、2 次预热；误差来自每个固定数据集的一次离线准确率实验。

| 分布／算法 | 更新 CPU（ns/item） | 读 CPU（ns/key） | retained／peak requested heap | 平均绝对相对频率误差 |
| --- | ---: | ---: | ---: | ---: |
| Uniform CMS | 36.64 | 49.80 | 5,440 B | 163.47% |
| Uniform CountSketch | 1,179.19 | 3,813.40 | 9,960,000 B | 0（该离线样本） |
| Zipf CMS | 35.68 | 44.33 | 5,440 B | 231.65% |
| Zipf CountSketch | 948.19 | 3,911.34 | 9,960,000 B | 0（该离线样本） |

CMS 参数为 272 × 5，CountSketch 为 30,000 × 83。CMS 的 additive epsilon
约束以流量总量为基准，并非每个 key 的 1% 相对误差约束，不能把上表误差解释为满足后者。
CountSketch 的零观测误差也不保证其他数据上的误差为零。

内存由独立存活对象探针测量，统计申请的堆字节，不包含输入数据、分配器页开销或整个进程 RSS。
CPU 与内存探针的分配器差异有单独说明。磁盘、序列化大小和空 sketch 构造 CPU 未测量，保持 null。

对加载这些数据后执行 1,000 次 point-frequency 查询，已测 CPU 分项之和如下：

| 数据 | CMS | 精确索引 | CountSketch |
| --- | ---: | ---: | ---: |
| Uniform | 0.783 ms | 0.741 ms | 27.397 ms |
| Zipf | 0.758 ms | 0.799 ms | 22.875 ms |

这是含精确索引 prepare 的离线分项估算，未包含未测量的 sketch 构造。
因此不能报告完整 break-even 或部署后的加速比。精确索引的状态大小 290,816 B
来自上游公式，也不能与 requested heap 的差额称作实测整体内存节省。
详见 [测量报告及原始数据](../tools/empirical-bench/results/MEASUREMENTS.md)。

## o11y 结果

使用仓库已有的 27 条 PromQL fixture；这是 2026-07-17 的 vendored snapshot，
Git blob 为 `ea2eee98d78c5c9354363dc378303d98ac141672`，没有可证实的上游 commit。
这次没有运行上游 LLM-agent 评分，也没有测量 o11y 真实数据分布。

| 检查层 | 结果 |
| --- | --- |
| Planner 查询候选覆盖 | 26/27 有精确摘要候选，1/27 原始回退；0/27 有近似 sketch 候选 |
| Planner 完整生命周期选择 | 27/27 保守 raw 回退，完整物理成本证据不足 |
| Control-plane parser + typed binder | exact/default/empirical 各 24/27 绑定成功、3/27 拒绝 |
| 独立补充查询 | 两条 quantile、一条 count_over_time；default/empirical 均可产生 sketch 候选 |
| 测量匹配 | count_over_time 的 CMS／CountSketch 配置匹配更新成本；quantile 缺少测量 |

Control-plane 拒绝的三种形式是裸指标选择、`sort_desc(...)` 和 `... > 0`。
它们不是 parser 失败；当前 typed binder 没有对应可绑定的根候选。
成功绑定也不等于可部署执行，部分绑定结果仍是原始查询回退。
为接通新版 Planner，补充了 Concat 元数据处理和 Summary BinaryOp 兼容；
warm tier 尚不能执行的二元运算保持显式 fallback。

实测证据没有改变 o11y 选择，因为这些查询没有对应 sketch 候选。
补充查询的 CMS update CPU 明显较低，测量排序保持 CMS 在前。
离线 point-frequency 误差不被当作 count_over_time 或 o11y 结果误差。

完整查询 CPU／内存收益均为 null。下一阶段要量化 o11y 收益，需要精确摘要及 raw/residual
算子在对应 metric 数据、group cardinality、窗口和执行频率下的测量，随后接入完整物理成本比较。
仅增加 sketch 算法测量不能填补这些输入。

## 验证与复现

- Planner mapping 单元测试：342 项通过。
- Planner replay：4 项通过；导出归一化测试见 benchmark README。
- Control-plane 全部 library 单元测试：633 项通过。
- 新增 control-plane 集成测试：3 项通过，覆盖改变绑定、保守回退和二元运算 fallback。
- Provider、replay、control-plane 接入与 benchmark 测量口径经过其他 agent 交叉检查。

复现入口：

- [真实 benchmark 命令与测量方法](../tools/empirical-bench/README.md)
- [Planner replay 命令](user-guide/o11y-replay.md)
- Control plane：其 worktree 的 `tools/run-offline-planner-replay.py` 和
  `control_plane/docs/offline-sketch-evidence.md`。
- 输出目录：`tools/empirical-bench/results/`，包含 uniform／Zipf 的 evidence、context、
  planner replay 和 control-plane replay；控制面报告记录了实际命令和两仓库版本。

这些结果是离线可复现实验与规划覆盖报告，不是线上端到端性能结论。
