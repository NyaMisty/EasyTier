# 被动带宽估计器实现总结

## 概述

实现了基于被动观测的智能带宽估计器，替换了原来简单的 EWMA 方法。新方法根据**排队延迟**、**实际吞吐**和**丢包率**动态调整带宽估计，无需主动探测。

## 核心特性

### 1. 固定起点 + 动态调整

- **初始值**: 30 Mbps（保守起点）
- **调整策略**: 
  - 干净窗口 → 温和上调（每秒最多 +10%）
  - 中性窗口 → 保持/微调
  - 拥塞窗口 → 快速下调（乘法下降）

### 2. 三种窗口状态

#### 干净窗口（Clean Window）
- **条件**: `qnorm ≤ 0.05` 且 `loss_rate ≤ 0.002`
- **动作**: 温和上调带宽估计
- **公式**: `b_hat = (1-γ) * b_hat + γ * min(throughput * 1.1, b_hat * (1 + 0.1*dt))`

#### 中性窗口（Neutral Window）
- **条件**: `0.05 < qnorm < 0.20` 且 `loss_rate < 0.01`
- **动作**: 保持或微调，不大幅改变
- **公式**: `b_hat = (1-γ) * b_hat + γ * max(b_hat, throughput)`

#### 拥塞窗口（Congestion Window）
- **条件**: `qnorm ≥ 0.20` 或 `loss_rate ≥ 0.01`
- **动作**: 快速下调（乘法下降）
- **公式**: `b_hat = max(b_hat, throughput) * (1 - 0.3*g)`，其中 `g` 是拥塞强度

### 3. 关键指标

- **qnorm**: 归一化排队延迟 = `(srtt - minrtt) / minrtt`
  - 反映当前队列堆积程度
  - 0 = 无队列，1 = 队列等于基础RTT

- **throughput**: 最近的实际吞吐（EWMA平滑）
  - 用于判断当前利用率
  - 避免估计值脱离实际

- **loss_rate**: 丢包率 (0.0..1.0)
  - 强拥塞信号
  - 触发快速下调

### 4. 路径突变保护

- **检测**: `|minrtt_new - minrtt_old| > max(3*rttvar, 20ms)`
- **动作**: 重置带宽估计到 `(30Mbps + current) / 2`
- **目的**: 避免路径切换时的错误估计延续

## 实现细节

### BandwidthEstimator 结构

```rust
pub struct BandwidthEstimator {
    b_hat: f64,                    // 当前带宽估计 (Bytes/s)
    last_update: Instant,
    last_minrtt_ms: f64,
    last_tx_bytes: u64,
    recent_throughput_bps: f64,    // 最近吞吐 (EWMA)
    
    // 配置参数
    tau_low: f64,                  // 0.05
    tau_high: f64,                 // 0.20
    p_low: f64,                    // 0.002
    p_high: f64,                   // 0.01
    alpha_per_s: f64,              // 0.10
    beta: f64,                     // 0.30
    gamma: f64,                    // 0.20
    headroom: f64,                 // 1.10
    b_floor: f64,                  // 5 Mbps
    b_cap: f64,                    // 1 Gbps
}
```

### 核心方法

#### update()

```rust
pub fn update(
    &mut self,
    current_tx_bytes: u64,
    srtt_ms: f64,
    rttvar_ms: f64,
    minrtt_ms: f64,
    loss_rate: f64,
)
```

**流程**:
1. 计算时间间隔（至少100ms）
2. 计算实际吞吐并EWMA平滑
3. 计算归一化排队延迟 qnorm
4. 检测路径突变
5. 根据窗口状态调整带宽估计

#### get_bw_bps()

返回当前带宽估计值（Bytes/s）

#### get_recent_throughput_bps()

返回最近的实际吞吐（Bytes/s）

### Throughput 集成

#### 新增字段

```rust
bw_estimator: UnsafeCell<BandwidthEstimator>,
```

#### 新增/修改方法

```rust
// 更新带宽估计
pub fn update_bw_estimate(
    &self,
    srtt_ms: f64,
    rttvar_ms: f64,
    minrtt_ms: f64,
    loss_rate: f64,
)

// 获取带宽估计
pub fn get_bw_est_bps(&self) -> f64

// 获取实际吞吐
pub fn get_recent_throughput_bps(&self) -> f64
```

## 使用示例

### 基本使用

```rust
let throughput = Throughput::new();

// 记录发送数据
throughput.record_tx_bytes(1500, true);

// 定期更新带宽估计（例如每250ms）
throughput.update_bw_estimate(
    50.0,   // srtt_ms
    10.0,   // rttvar_ms
    40.0,   // minrtt_ms
    0.001   // loss_rate
);

// 获取带宽估计
let bw_bps = throughput.get_bw_est_bps();
println!("Estimated bandwidth: {:.2} Mbps", bw_bps / 1_000_000.0);
```

### 集成到 PeerConn

```rust
// 在 peer_conn.rs 中定期更新
pub fn update_bandwidth_estimate(&self) {
    let latency_stats = self.stats.latency_stats.read().unwrap();
    let srtt_us = latency_stats.get_latency_us::<u32>();
    let rttvar_us = latency_stats.get_rttvar_us();
    let minrtt_us = latency_stats.get_min_rtt_us();
    
    let loss_rate = self.stats.loss_rate_stats.load(Ordering::Relaxed) as f32 / 100.0;
    
    self.stats.throughput.update_bw_estimate(
        (srtt_us as f64) / 1000.0,
        (rttvar_us as f64) / 1000.0,
        (minrtt_us as f64) / 1000.0,
        loss_rate as f64,
    );
}
```

## 测试验证

### 测试场景

1. **test_throughput_bw_estimate**: 基本带宽估计功能
   - 验证初始值30 Mbps
   - 验证干净窗口下的调整
   - 验证吞吐变化时的响应

2. **test_bw_estimate_with_zero_traffic**: 零流量场景
   - 验证零流量时保持初始值
   - 允许小幅波动（±1 Mbps）

3. **test_bw_estimate_congestion**: 拥塞检测
   - 验证干净窗口 → 拥塞窗口
   - 验证带宽快速下调

4. **test_bw_estimate_path_change**: 路径突变
   - 验证路径切换检测
   - 验证带宽重置逻辑

### 测试结果

```
running 6 tests
test tunnel::stats::tests::test_throughput_bw_estimate ... ok
test tunnel::stats::tests::test_bw_estimate_with_zero_traffic ... ok
test tunnel::stats::tests::test_bw_estimate_congestion ... ok
test tunnel::stats::tests::test_bw_estimate_path_change ... ok
test tunnel::stats::tests::test_throughput_inflight ... ok
test tunnel::stats::tests::test_throughput_recent_bytes ... ok

test result: ok. 6 passed; 0 failed
```

## 参数调优建议

### 保守配置（稳定优先）

```rust
tau_low: 0.03,      // 更严格的干净窗口
tau_high: 0.25,     // 更宽松的拥塞阈值
alpha_per_s: 0.05,  // 更慢的上调
beta: 0.20,         // 更温和的下调
```

### 激进配置（性能优先）

```rust
tau_low: 0.08,      // 更宽松的干净窗口
tau_high: 0.15,     // 更严格的拥塞阈值
alpha_per_s: 0.15,  // 更快的上调
beta: 0.40,         // 更激进的下调
```

### 网络类型适配

#### 以太网（低延迟、低抖动）
```rust
target_qdelay_ms: 5,
tau_low: 0.05,
tau_high: 0.15,
```

#### Wi-Fi/蜂窝（中等延迟、中等抖动）
```rust
target_qdelay_ms: 10,
tau_low: 0.08,
tau_high: 0.20,
```

#### 卫星/长距离（高延迟、高抖动）
```rust
target_qdelay_ms: 20,
tau_low: 0.10,
tau_high: 0.25,
```

## 性能特点

### 优势

1. **无需主动探测**: 纯被动观测，不额外占用带宽
2. **快速响应拥塞**: 检测到拥塞立即乘法下降
3. **温和增长**: 干净窗口下稳步上调，避免突变
4. **路径感知**: 自动检测和适应路径变化
5. **零流量友好**: 无流量时保持估计值

### 局限

1. **需要准确的 RTT 测量**: 依赖 srtt、minrtt 的准确性
2. **需要丢包率数据**: 需要连接层提供丢包统计
3. **初始值偏保守**: 30 Mbps 起点可能对高带宽链路偏低
4. **响应有延迟**: 至少100ms更新一次

## 后续优化方向

1. **自适应初始值**: 根据历史数据调整起点
2. **多时间尺度**: 短期/长期分别估计
3. **应用层反馈**: 结合应用层吞吐需求
4. **链路类型识别**: 自动识别并适配参数
5. **协同调度**: 与多路径调度器深度整合

## 调试支持

### 日志级别

- **trace**: 每次更新的详细信息
  - 窗口状态
  - 吞吐变化
  - 带宽调整

- **debug**: 关键事件
  - 路径突变检测
  - 拥塞检测
  - 大幅调整

### 启用调试日志

```bash
RUST_LOG=easytier::tunnel::stats=trace cargo run
```

## 相关文档

- `PATH_SNAPSHOT_IMPLEMENTATION.md`: PathSnapshot 参数对接
- `MULTIPATH_DEBUG_LOGGING.md`: 多路径调度日志
- `MULTIPATH_INTEGRATION_REPORT.md`: 多路径集成报告

---

**实现日期**: 2025-01-01  
**版本**: v1.0  
**状态**: ✅ 已完成并测试通过
