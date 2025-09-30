# Multipath Scheduler Debug 日志说明

## 概述

Multipath Scheduler 现在包含完整的 debug 级别日志输出，可以详细追踪调度决策过程。

## 日志级别

所有调度器日志都使用 `tracing::debug!` 输出，需要设置日志级别为 `debug` 才能看到。

## 启用方法

### 方式 1: 环境变量
```bash
RUST_LOG=easytier::peers::multipath_scheduler=debug cargo run
```

### 方式 2: 只看调度器日志
```bash
RUST_LOG=easytier::peers::multipath_scheduler=debug,easytier=info cargo run
```

## 日志结构

调度器的决策过程分为以下阶段，每个阶段都有详细的日志输出：

### 1. 输入信息
```
Multipath scheduler input: now_ms=..., packet_len=..., flow_bytes_sent=..., critical=..., paths_count=...
```
显示调度器收到的输入参数。

### 2. Phase A - 过滤不可用路径
```
Phase A - Filter inactive: X usable paths from Y total
  Candidate path 1: srtt=50.00ms, queue_delay=10.50ms, eta=65.20ms, score=9500000.00
  Candidate path 2: srtt=100.00ms, queue_delay=5.00ms, eta=110.30ms, score=5000000.00
```
- 显示有多少路径可用
- 列出每个候选路径的初始指标和分数

### 3. Phase C - BLEST 乱序预算过滤
```
Phase C - BLEST filter: X paths remain (filtered out Y)
BLEST filter: eta_fast=65.20ms, srtt_fast=50.00ms, delta=12.50ms
  Path 1: eta=65.20ms, eta_diff=0.00ms, PASS BLEST filter
  Path 2: eta=110.30ms, eta_diff=45.10ms, FILTERED by BLEST filter
```
- 显示 BLEST 过滤结果
- 计算的乱序预算 delta
- 每个路径的 ETA 差距和是否通过过滤

### 4. Phase D - QAware 目标排队时延控制
```
Phase D - QAware penalty applied (target_qdelay=10ms)
  Path 1 after QAware: score=9500000.00, queue_delay=10.50ms
QAware penalty: path 1 queue_delay=10.50ms > target=10ms, excess=0.50ms, penalty=0.03, score 9500000.00 -> 9499999.97
```
或
```
QAware: all paths over target (10ms), no additional penalty
```
- 显示队列时延惩罚的应用情况
- 详细的分数调整过程

### 5. Phase B - 按分数排序
```
Phase B - Sorted by score:
  #1 Path 1: score=9500000.00, eta=65.20ms, srtt=50.00ms
  #2 Path 2: score=5000000.00, eta=110.30ms, srtt=100.00ms
```
- 显示排序后的候选路径列表
- 第一个就是将被选为 primary 的路径

### 6. Phase E - 冗余决策
```
Phase E - Redundancy enabled: redundant_path=2
```
或
```
Phase E - Redundancy not triggered
```
或
```
Phase E - Redundancy disabled
```

#### 冗余决策详细日志：
```
Redundancy triggers: short_flow=true (sent=1024, thresh=131072), idle_burst=false, critical=false
Redundancy ETA check: fastest=65.20ms (path 1), 2nd=110.30ms (path 2), gap=45.10ms, delta=12.50ms
Redundancy: gap (45.10ms) > delta (12.50ms), selecting path 2 as redundant
```
或
```
Redundancy: gap (5.00ms) <= delta (12.50ms), not worth redundant send
```
或
```
Redundancy: no trigger condition met
```
或
```
Redundancy: only 1 path(s), need at least 2
```

### 7. 最终决策
```
Final decision: primary=1, redundant=Some(2)
```
或
```
Final decision: primary=1, redundant=None
```

## 日志示例

### 场景 1: 正常路径选择（无冗余）
```
Multipath scheduler input: now_ms=1000, packet_len=1500, flow_bytes_sent=100000, critical=false, paths_count=2
Phase A - Filter inactive: 2 usable paths from 2 total
  Candidate path 1: srtt=50.00ms, queue_delay=10.00ms, eta=65.20ms, score=9500000.00
  Candidate path 2: srtt=100.00ms, queue_delay=5.00ms, eta=110.30ms, score=5000000.00
Phase C - BLEST filter: 1 paths remain (filtered out 1)
BLEST filter: eta_fast=65.20ms, srtt_fast=50.00ms, delta=12.50ms
  Path 1: eta=65.20ms, eta_diff=0.00ms, PASS BLEST filter
  Path 2: eta=110.30ms, eta_diff=45.10ms, FILTERED by BLEST filter
Phase D - QAware penalty applied (target_qdelay=10ms)
  Path 1 after QAware: score=9500000.00, queue_delay=10.00ms
Phase B - Sorted by score:
  #1 Path 1: score=9500000.00, eta=65.20ms, srtt=50.00ms
Redundancy: only 1 path(s), need at least 2
Phase E - Redundancy not triggered
Final decision: primary=1, redundant=None
```

### 场景 2: 短流触发冗余
```
Multipath scheduler input: now_ms=1000, packet_len=1500, flow_bytes_sent=1024, critical=false, paths_count=2
Phase A - Filter inactive: 2 usable paths from 2 total
  Candidate path 1: srtt=50.00ms, queue_delay=10.00ms, eta=65.20ms, score=9500000.00
  Candidate path 2: srtt=100.00ms, queue_delay=5.00ms, eta=110.30ms, score=5000000.00
Phase C - BLEST filter: 2 paths remain (filtered out 0)
BLEST filter: eta_fast=65.20ms, srtt_fast=50.00ms, delta=12.50ms
  Path 1: eta=65.20ms, eta_diff=0.00ms, PASS BLEST filter
  Path 2: eta=110.30ms, eta_diff=45.10ms, PASS BLEST filter
Phase D - QAware penalty applied (target_qdelay=10ms)
  Path 1 after QAware: score=9500000.00, queue_delay=10.00ms
  Path 2 after QAware: score=5000000.00, queue_delay=5.00ms
Phase B - Sorted by score:
  #1 Path 1: score=9500000.00, eta=65.20ms, srtt=50.00ms
  #2 Path 2: score=5000000.00, eta=110.30ms, srtt=100.00ms
Redundancy triggers: short_flow=true (sent=1024, thresh=131072), idle_burst=false, critical=false
Redundancy ETA check: fastest=65.20ms (path 1), 2nd=110.30ms (path 2), gap=45.10ms, delta=12.50ms
Redundancy: gap (45.10ms) > delta (12.50ms), selecting path 2 as redundant
Phase E - Redundancy enabled: redundant_path=2
Final decision: primary=1, redundant=Some(2)
```

### 场景 3: QAware 惩罚高队列时延路径
```
Multipath scheduler input: now_ms=1000, packet_len=1500, flow_bytes_sent=100000, critical=false, paths_count=2
Phase A - Filter inactive: 2 usable paths from 2 total
  Candidate path 1: srtt=50.00ms, queue_delay=50.00ms, eta=110.00ms, score=9000000.00
  Candidate path 2: srtt=60.00ms, queue_delay=5.00ms, eta=75.00ms, score=8500000.00
Phase C - BLEST filter: 2 paths remain (filtered out 0)
BLEST filter: eta_fast=75.00ms, srtt_fast=60.00ms, delta=15.00ms
  Path 2: eta=75.00ms, eta_diff=0.00ms, PASS BLEST filter
  Path 1: eta=110.00ms, eta_diff=35.00ms, PASS BLEST filter
Phase D - QAware penalty applied (target_qdelay=10ms)
QAware penalty: path 1 queue_delay=50.00ms > target=10ms, excess=40.00ms, penalty=2.00, score 9000000.00 -> 8999998.00
  Path 1 after QAware: score=8999998.00, queue_delay=50.00ms
  Path 2 after QAware: score=8500000.00, queue_delay=5.00ms
Phase B - Sorted by score:
  #1 Path 1: score=8999998.00, eta=110.00ms, srtt=50.00ms
  #2 Path 2: score=8500000.00, eta=75.00ms, srtt=60.00ms
Phase E - Redundancy disabled
Final decision: primary=1, redundant=None
```

## 调试技巧

### 1. 分析路径选择
查看 "Final decision" 和 "Phase B - Sorted by score" 来了解为什么某个路径被选中。

### 2. 检查 BLEST 过滤
如果某些路径被过滤掉，查看 "BLEST filter" 部分，检查 delta 值和 eta_diff。

### 3. 理解冗余决策
查看 "Redundancy triggers" 和 "Redundancy ETA check" 来了解冗余是否应该触发。

### 4. 分析性能问题
如果某个路径表现不佳但仍被选中，检查：
- 初始分数（Phase A）
- BLEST 过滤结果（Phase C）
- QAware 惩罚（Phase D）
- 最终排序（Phase B）

### 5. 验证配置效果
修改配置参数后，通过日志观察：
- `target_qdelay_ms` 的影响（QAware 部分）
- `blest_delta_cap_ms` 的影响（BLEST 部分）
- `short_flow_thresh` 的影响（Redundancy 部分）

## 日志过滤

### 只看某一阶段
```bash
# 只看 BLEST 过滤
RUST_LOG=easytier::peers::multipath_scheduler=debug cargo run 2>&1 | grep "BLEST"

# 只看冗余决策
RUST_LOG=easytier::peers::multipath_scheduler=debug cargo run 2>&1 | grep "Redundancy"

# 只看最终决策
RUST_LOG=easytier::peers::multipath_scheduler=debug cargo run 2>&1 | grep "Final decision"
```

### 统计决策情况
```bash
# 统计使用冗余的次数
RUST_LOG=easytier::peers::multipath_scheduler=debug cargo run 2>&1 | grep "redundant=Some" | wc -l

# 统计路径选择分布
RUST_LOG=easytier::peers::multipath_scheduler=debug cargo run 2>&1 | grep "Final decision: primary=" | sort | uniq -c
```

## 性能考虑

- Debug 日志会有一定的性能开销
- 生产环境建议使用 `info` 或更高级别
- 调试时可以针对特定模块启用 debug
- 日志输出量较大时考虑重定向到文件

## 与 peer.rs 集成

在 `peer.rs` 中调用调度器时，也会看到相应的日志：
```
[peer.rs] multipath scheduler input: now_ms=..., packet_len=1500, flow_bytes_sent=..., paths_count=...
[multipath_scheduler.rs] Multipath scheduler input: ...
[multipath_scheduler.rs] Phase A - Filter inactive: ...
...
[multipath_scheduler.rs] Final decision: primary=1, redundant=None
[peer.rs] multipath selected: primary=1, redundant=None, conn_ids=[...]
```

## 故障排查清单

1. ✅ 路径未被选中？
   - 检查 Phase A 是否被过滤（inactive）
   - 检查 Phase C BLEST 是否被过滤
   - 比较 Phase B 的分数

2. ✅ 冗余未触发？
   - 检查触发条件（short_flow/idle_burst/critical）
   - 检查 ETA gap 是否大于 delta
   - 检查是否有至少2个可用路径

3. ✅ 高延迟路径被选中？
   - 检查是否因为高带宽/低丢包而得分更高
   - 检查 QAware 是否启用
   - 检查 target_qdelay_ms 配置

4. ✅ 频繁切换路径？
   - 检查路径分数是否接近
   - 考虑调整权重配置
   - 检查路径质量是否波动

## 总结

通过这些详细的 debug 日志，你可以：
- 🔍 完整追踪每个包的路径选择过程
- 📊 分析调度算法的行为
- 🐛 快速定位问题
- ⚙️ 验证配置参数的效果
- 📈 优化调度策略
