# Multipath Scheduler Debug 日志功能完成报告

## 任务概述
为 Multipath Scheduler 添加完整的 debug 级别日志输出，以便追踪和调试调度决策过程。

## 完成内容

### 修改文件
**文件**: `easytier/src/peers/multipath_scheduler.rs`

### 新增日志点

#### 1. **调度器输入日志** (`pick_path` 方法开始)
```rust
tracing::debug!(
    "Multipath scheduler input: now_ms={}, packet_len={}, flow_bytes_sent={}, critical={}, paths_count={}",
    inp.now_ms, inp.packet_len, inp.flow_bytes_sent, inp.critical, inp.paths.len()
);
```
记录每次调度的输入参数。

#### 2. **Phase A - 过滤不可用路径**
```rust
tracing::debug!(
    "Phase A - Filter inactive: {} usable paths from {} total",
    candidates.len(), inp.paths.len()
);

// 记录每个候选路径的初始指标
for c in &candidates {
    tracing::debug!(
        "  Candidate path {}: srtt={:.2}ms, queue_delay={:.2}ms, eta={:.2}ms, score={:.2}",
        c.id, c.srtt_ms, c.queue_delay_ms, c.eta_ms, c.score
    );
}
```

#### 3. **Phase C - BLEST 乱序预算过滤**
```rust
// 过滤前后的数量对比
tracing::debug!(
    "Phase C - BLEST filter: {} paths remain (filtered out {})",
    candidates.len(), before_count - candidates.len()
);

// BLEST 计算详情
tracing::debug!(
    "BLEST filter: eta_fast={:.2}ms, srtt_fast={:.2}ms, delta={:.2}ms",
    eta_fast, srtt_fast, delta_ms
);

// 每个路径的过滤结果
tracing::debug!(
    "  Path {}: eta={:.2}ms, eta_diff={:.2}ms, {} BLEST filter",
    c.id, c.eta_ms, eta_diff, if pass { "PASS" } else { "FILTERED by" }
);
```

#### 4. **Phase D - QAware 目标排队时延控制**
```rust
// QAware 应用通知
tracing::debug!("Phase D - QAware penalty applied (target_qdelay={}ms)", cfg.target_qdelay_ms);

// 惩罚详情
tracing::debug!(
    "QAware penalty: path {} queue_delay={:.2}ms > target={}ms, excess={:.2}ms, penalty={:.2}, score {:.2} -> {:.2}",
    c.id, c.queue_delay_ms, cfg.target_qdelay_ms, excess, penalty, old_score, c.score
);

// 或全部超标情况
tracing::debug!(
    "QAware: all paths over target ({}ms), no additional penalty",
    cfg.target_qdelay_ms
);
```

#### 5. **Phase B - 按分数排序**
```rust
tracing::debug!("Phase B - Sorted by score:");
for (i, c) in candidates.iter().enumerate() {
    tracing::debug!(
        "  #{} Path {}: score={:.2}, eta={:.2}ms, srtt={:.2}ms",
        i + 1, c.id, c.score, c.eta_ms, c.srtt_ms
    );
}
```

#### 6. **Phase E - 冗余决策**
```rust
// 触发条件检查
tracing::debug!(
    "Redundancy triggers: short_flow={} (sent={}, thresh={}), idle_burst={}, critical={}",
    is_short_flow, inp.flow_bytes_sent, cfg.short_flow_thresh,
    is_idle_burst, inp.critical
);

// ETA 差距分析
tracing::debug!(
    "Redundancy ETA check: fastest={:.2}ms (path {}), 2nd={:.2}ms (path {}), gap={:.2}ms, delta={:.2}ms",
    eta_sorted[0].eta_ms, eta_sorted[0].id,
    eta_sorted[1].eta_ms, eta_sorted[1].id,
    eta_gap, delta_ms
);

// 决策结果
tracing::debug!(
    "Redundancy: gap ({:.2}ms) > delta ({:.2}ms), selecting path {} as redundant",
    eta_gap, delta_ms, eta_sorted[1].id
);
```

#### 7. **最终决策**
```rust
tracing::debug!(
    "Final decision: primary={}, redundant={:?}",
    decision.primary, decision.redundant
);
```

## 日志特点

### ✅ 完整性
- 覆盖所有5个决策阶段
- 记录所有关键决策点
- 包含输入和输出信息

### ✅ 可读性
- 使用清晰的前缀标识阶段（Phase A/B/C/D/E）
- 数值格式化为易读格式（{:.2}ms）
- 层级缩进（使用空格前缀）

### ✅ 调试友好
- 记录计算中间值（如 delta、gap、excess）
- 显示分数变化（old -> new）
- 标注通过/过滤状态（PASS / FILTERED by）

### ✅ 性能考虑
- 使用 `tracing::debug!` 而非 `println!`
- 生产环境默认不输出（需显式启用）
- 格式化参数延迟求值

## 使用方法

### 启用日志
```bash
# 只看调度器日志
RUST_LOG=easytier::peers::multipath_scheduler=debug cargo run

# 调度器+其他模块
RUST_LOG=easytier::peers::multipath_scheduler=debug,easytier=info cargo run
```

### 测试验证
```bash
# 运行测试查看日志
cargo test --lib test_phase_b_bandwidth_preference -- --nocapture

# 运行所有调度器测试
RUST_LOG=debug cargo test --lib multipath_scheduler -- --nocapture
```

## 日志输出示例

### 简单场景
```
Multipath scheduler input: now_ms=1000, packet_len=1500, flow_bytes_sent=1024, critical=false, paths_count=2
Phase A - Filter inactive: 2 usable paths from 2 total
  Candidate path 1: srtt=50.00ms, queue_delay=10.00ms, eta=65.20ms, score=9500000.00
  Candidate path 2: srtt=100.00ms, queue_delay=5.00ms, eta=110.30ms, score=5000000.00
Phase C - BLEST filter: 1 paths remain (filtered out 1)
BLEST filter: eta_fast=65.20ms, srtt_fast=50.00ms, delta=12.50ms
  Path 1: eta=65.20ms, eta_diff=0.00ms, PASS BLEST filter
  Path 2: eta=110.30ms, eta_diff=45.10ms, FILTERED by BLEST filter
Phase D - QAware penalty applied (target_qdelay=10ms)
Phase B - Sorted by score:
  #1 Path 1: score=9500000.00, eta=65.20ms, srtt=50.00ms
Redundancy: only 1 path(s), need at least 2
Phase E - Redundancy not triggered
Final decision: primary=1, redundant=None
```

## 编译状态

✅ **编译通过**: 无错误，无警告
```bash
cargo check --lib
Finished `dev` profile [unoptimized + debuginfo] target(s) in 55s
```

## 调试价值

### 1. **问题诊断**
- 快速定位为什么某个路径被选中/被过滤
- 理解冗余为何触发/未触发
- 追踪分数计算过程

### 2. **算法验证**
- 验证 BLEST delta 计算是否正确
- 确认 QAware 惩罚是否生效
- 检查冗余条件判断

### 3. **参数调优**
- 观察不同配置参数的影响
- 比较不同权重下的路径选择
- 分析阈值设置的合理性

### 4. **性能分析**
- 统计路径选择分布
- 分析冗余发送频率
- 评估过滤效果

## 后续优化建议

1. **可选的详细级别**
   - 添加 `trace` 级别用于更详细的日志
   - 添加配置控制日志详细程度

2. **结构化日志**
   - 考虑使用 `tracing::span` 进行结构化
   - 添加更多上下文字段（如 flow_id）

3. **统计信息**
   - 定期输出调度统计摘要
   - 记录路径使用时长和频率

4. **日志采样**
   - 对高频调度进行采样记录
   - 避免日志量过大影响性能

## 相关文档

- [MULTIPATH_DEBUG_LOGGING.md](./MULTIPATH_DEBUG_LOGGING.md) - 详细的日志使用指南
- [MULTIPATH_COMPLETION_SUMMARY.md](./MULTIPATH_COMPLETION_SUMMARY.md) - Multipath 集成总结
- [MULTIPATH_INTEGRATION_REPORT.md](./MULTIPATH_INTEGRATION_REPORT.md) - 集成技术报告

## 总结

✅ 成功为 Multipath Scheduler 添加了完整的 debug 级别日志
✅ 覆盖所有决策阶段，提供详细的追踪信息
✅ 日志格式清晰易读，便于调试和分析
✅ 性能影响可控，生产环境可按需启用
✅ 编译通过，无警告，可立即投入使用

现在开发者可以通过设置 `RUST_LOG=easytier::peers::multipath_scheduler=debug` 来完整追踪每个包的路径选择过程，极大地提升了调试和优化的效率！
