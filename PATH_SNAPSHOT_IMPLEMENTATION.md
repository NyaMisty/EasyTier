# PathSnapshot 参数对接实现总结

本文档说明了如何将多路径调度器 `PathSnapshot` 的各参数与 `peer_conn.rs` 中的 `PeerConnInfo` 和 `PeerConnStats` 对接。

## 实现概览

### 1. srtt_ms, rttvar_ms, min_rtt_ms

**来源**: `WindowLatency`

**实现位置**: `easytier/src/tunnel/stats.rs`

**新增方法**:
- `get_latency_us<T>()` - 已存在，返回平均延迟（微秒）
- `get_rttvar_us()` - 新增，计算 RTT 方差（标准差）
- `get_min_rtt_us()` - 新增，返回窗口内最小 RTT

**使用**:
```rust
let srtt_us = latency_stats.get_latency_us::<u32>();
let srtt_ms = (srtt_us as f32) / 1000.0;

let rttvar_us = latency_stats.get_rttvar_us();
let rttvar_ms = (rttvar_us as f32) / 1000.0;

let min_rtt_us = latency_stats.get_min_rtt_us();
let min_rtt_ms = (min_rtt_us as f32) / 1000.0;
```

### 2. bw_est_Bps

**来源**: `Throughput`

**实现位置**: `easytier/src/tunnel/stats.rs`

**新增字段**:
- `last_bw_update: UnsafeCell<Instant>` - 上次带宽更新时间
- `estimated_bw_bps: UnsafeCell<f64>` - 估计的带宽（Bytes/s）
- `bw_alpha: f64` - EWMA 平滑系数（默认 0.2）

**新增方法**:
- `update_bw_estimate(interval_ms: u32)` - 使用 EWMA 更新带宽估计
- `get_bw_est_bps() -> f64` - 获取当前带宽估计

**使用**:
```rust
// 定期更新带宽估计（例如每秒）
throughput.update_bw_estimate(1000);

// 获取带宽估计
let bw_est_Bps = throughput.get_bw_est_bps();
```

### 3. loss_rate

**来源**: `PeerConnStats` (已存在)

**实现**: 已在 `PeerConn` 中实现

**使用**:
```rust
let loss_rate = loss_rate_stats.load(Ordering::Relaxed) as f32 / 100.0;
```

### 4. inflight_bytes

**来源**: 分为两类

#### 4.1 Tunnel 支持汇报（如 TCP）

**实现位置**: `easytier/src/tunnel/mod.rs`

**新增结构**:
```rust
pub struct TunnelMetrics {
    pub inflight_bytes: Option<u64>,
}
```

**新增 Trait 方法**:
```rust
pub trait Tunnel: Send {
    // ...existing methods...
    
    fn get_metrics(&self) -> Option<TunnelMetrics> {
        None  // 默认不支持
    }
}
```

**实现要求**: TCP/QUIC 等支持的 tunnel 应该实现 `get_metrics()` 方法。

#### 4.2 Tunnel 不支持（如 UDP）

**来源**: `Throughput::estimate_inflight()`

**实现位置**: `easytier/src/tunnel/stats.rs`

**相关结构**: `TimeBucketInFlight`

**方法**:
```rust
pub fn estimated_inflight_bytes(&self, srtt_ms: u32, gamma: f32) -> u64
```

**使用**:
```rust
// gamma = 1.3
let inflight = throughput.estimated_inflight_bytes(srtt_ms, 1.3);
```

**自动选择逻辑**（在 `PeerConn::get_path_snapshot` 中）:
```rust
let inflight_bytes = if let Some(metrics) = tunnel_metrics {
    if let Some(tunnel_inflight) = metrics.inflight_bytes {
        // 优先使用 tunnel 汇报的值
        tunnel_inflight as u32
    } else {
        // tunnel 不支持，使用 Throughput 估算
        throughput.estimated_inflight_bytes(srtt_ms_u32, 1.3) as u32
    }
} else {
    // 没有 tunnel metrics，使用 Throughput 估算
    throughput.estimated_inflight_bytes(srtt_ms_u32, 1.3) as u32
};
```

### 5. last_tx_age_ms

**来源**: `Throughput` (已存在)

**实现**: 已在 `Throughput` 中实现

**使用**:
```rust
let last_tx_age_ms = throughput.last_tx_age_ms() as u32;
```

### 6. active

**实现**: 始终为 `true`

**说明**: 表示路径始终活跃。

## PeerConn 集成

### 新增方法

**位置**: `easytier/src/peers/peer_conn.rs`

```rust
pub fn get_path_snapshot(
    &self,
    path_id: u64,
    tunnel_metrics: Option<crate::tunnel::TunnelMetrics>,
) -> crate::peers::multipath_scheduler::PathSnapshot
```

### 使用示例

```rust
// 不带 tunnel metrics（UDP 等）
let snapshot = peer_conn.get_path_snapshot(1, None);

// 带 tunnel metrics（TCP 等）
let metrics = TunnelMetrics {
    inflight_bytes: Some(1024),
};
let snapshot = peer_conn.get_path_snapshot(1, Some(metrics));
```

## 测试

**位置**: `easytier/src/peers/peer_conn.rs`

测试函数 `test_path_snapshot_construction` 验证了：
1. PathSnapshot 的基本构造
2. 延迟统计的正确性
3. 丢包率的正确性
4. 带/不带 tunnel metrics 的 inflight_bytes 计算

## 后续工作

1. **实现 Tunnel 的 get_metrics()**:
   - TCP tunnel: 实现 get_metrics() 返回实际的 inflight bytes
   - QUIC tunnel: 实现 get_metrics() 返回实际的 inflight bytes
   - UDP/其他: 保持默认 (返回 None)

2. **带宽估算优化**:
   - 在适当位置定期调用 `update_bw_estimate()`
   - 调整 EWMA 系数 `bw_alpha` 以获得更好的估算效果

3. **调度器集成**:
   - 在多路径调度器调用处使用 `get_path_snapshot()`
   - 确保正确传入 tunnel_metrics（如果可用）

4. **性能优化**:
   - 考虑缓存 PathSnapshot 以减少重复计算
   - 优化 WindowLatency 的 rttvar 计算（如果成为瓶颈）

## 关键设计决策

1. **inflight_bytes 的两级策略**: 优先使用 tunnel 层的精确值，否则通过发送时间窗口估算，这样既保证了准确性又保证了通用性。

2. **UnsafeCell 的使用**: Throughput 使用 UnsafeCell 以支持内部可变性，允许在 `&self` 方法中修改状态。这要求使用者确保线程安全。

3. **EWMA 带宽估算**: 使用指数加权移动平均平滑带宽估计，平衡了响应速度和稳定性。

4. **WindowLatency 的方差计算**: 使用简化的方差计算方法，在性能和准确性之间取得平衡。
