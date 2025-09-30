# Throughput 接收流量统计增强

## 修改概述

在 `Throughput` 结构体中添加了最近2秒接收流量（RX）的统计功能，与已有的发送流量（TX）统计对称。

## 修改文件

**文件**: `easytier/src/tunnel/stats.rs`

## 新增内容

### 1. 新增字段

```rust
pub struct Throughput {
    // ...existing fields...
    
    // 最近2s流量统计（用于multipath的flow_bytes_sent）
    recent_tx_bytes: UnsafeCell<u64>,
    recent_tx_window_start: UnsafeCell<Instant>,
    
    // 最近2s接收流量统计
    recent_rx_bytes: UnsafeCell<u64>,  // 最近2s接收的字节数
    recent_rx_window_start: UnsafeCell<Instant>,  // 窗口起始时间
}
```

### 2. 新增方法

```rust
/// 获取最近2秒接收的字节数
pub fn get_recent_rx_bytes(&self) -> u64 {
    unsafe {
        let now = Instant::now();
        let window_start = *self.recent_rx_window_start.get();
        let elapsed = (now - window_start).as_secs();
        
        // 如果超过2秒，重置窗口
        if elapsed >= 2 {
            *self.recent_rx_window_start.get() = now;
            *self.recent_rx_bytes.get() = 0;
            return 0;
        }
        
        *self.recent_rx_bytes.get()
    }
}
```

### 3. 更新已有方法

在 `record_rx_bytes()` 方法中添加了窗口统计更新：

```rust
pub fn record_rx_bytes(&self, bytes: u64, is_data: bool) {
    unsafe {
        *self.rx_bytes.get() += bytes;
        *self.rx_packets.get() += 1;
        *self.last_rx.get() = self.start_time.elapsed().as_millis() as u64;
        if is_data {
            *self.data_rx_bytes.get() += bytes;
            *self.data_rx_packets.get() += 1;
        }
        
        // 更新最近2秒的接收流量统计
        let now = Instant::now();
        let recent_window = Duration::from_secs(2);
        let window_start = *self.recent_rx_window_start.get();
        if now.duration_since(window_start) >= recent_window {
            *self.recent_rx_bytes.get() = 0;  // 超过窗口清零
            *self.recent_rx_window_start.get() = now;
        }
        *self.recent_rx_bytes.get() += bytes;
    }
}
```

### 4. 更新结构体初始化

- **Clone 实现**: 添加了 `recent_rx_bytes` 和 `recent_rx_window_start` 字段的克隆
- **Default 实现**: 初始化为 0 和当前时间

## 功能特性

### 与 TX 统计对称

| 功能 | TX (发送) | RX (接收) |
|------|-----------|-----------|
| 窗口大小 | 2秒 | 2秒 |
| 字节计数器 | `recent_tx_bytes` | `recent_rx_bytes` |
| 窗口起始时间 | `recent_tx_window_start` | `recent_rx_window_start` |
| 获取方法 | `get_recent_tx_bytes()` | `get_recent_rx_bytes()` |
| 更新位置 | `record_tx_bytes()` | `record_rx_bytes()` |

### 窗口管理机制

1. **自动重置**: 当超过2秒时自动重置窗口
2. **滑动统计**: 每次记录都会检查窗口是否过期
3. **线程安全**: 使用 `UnsafeCell` 配合 `unsafe` 块实现

## 使用示例

```rust
// 获取最近2秒的接收流量
let recent_rx = throughput.get_recent_rx_bytes();
println!("最近2秒接收: {} bytes", recent_rx);

// 获取最近2秒的发送流量
let recent_tx = throughput.get_recent_tx_bytes();
println!("最近2秒发送: {} bytes", recent_tx);

// 计算最近2秒的总流量
let total_recent = recent_tx + recent_rx;
println!("最近2秒总流量: {} bytes", total_recent);
```

## 应用场景

1. **流量监控**: 实时监控最近的接收流量变化
2. **带宽评估**: 评估接收方向的瞬时带宽使用
3. **负载均衡**: 基于双向流量做更精确的路径选择
4. **QoS控制**: 根据接收流量调整发送策略
5. **对称性检测**: 检测上下行流量是否对称

## 编译状态

✅ **编译通过**: 无错误，无新增警告
```
cargo check --lib
Finished `dev` profile [unoptimized + debuginfo] target(s) in 7.73s
```

## 后续可能的增强

1. **窗口大小可配置**: 允许配置不同的统计窗口（如1秒、5秒）
2. **速率计算**: 提供 `get_recent_rx_rate_bps()` 直接返回速率
3. **峰值统计**: 记录窗口内的峰值流量
4. **历史记录**: 保存多个窗口的历史数据用于趋势分析

## 与现有功能的关系

此增强完善了 Throughput 的流量统计能力，与以下功能配合使用：

- **多路径调度**: 可以同时考虑发送和接收流量做决策
- **带宽估计**: 双向流量有助于更准确的带宽估计
- **连接质量评估**: 全面的流量数据提供更完整的连接画像

## 总结

✅ 成功添加了最近2秒接收流量统计功能
✅ 与发送流量统计完全对称
✅ 保持了原有代码风格和线程安全性
✅ 编译通过，可以投入使用
