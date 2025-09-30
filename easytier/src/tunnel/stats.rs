use atomic_shim::AtomicU64;
use std::{
    cell::UnsafeCell,
    sync::atomic::{AtomicU32, Ordering::Relaxed},
    time::{Duration, Instant},
};

/// 被动带宽估计器
/// 基于排队延迟、实际吞吐和丢包率动态调整带宽估计
#[derive(Debug, Clone)]
pub struct BandwidthEstimator {
    /// 当前带宽估计值 (Bytes/s)
    b_hat: f64,
    /// 上次更新时间
    last_update: Instant,
    /// 上次的 min_rtt (用于检测路径突变)
    last_minrtt_ms: f64,
    /// 上次累计发送字节数
    last_tx_bytes: u64,
    /// 最近窗口的实际吞吐 (Bytes/s, EWMA)
    recent_throughput_bps: f64,
    
    // 配置参数
    /// 排队延迟阈值 - 低 (干净窗口)
    tau_low: f64,
    /// 排队延迟阈值 - 高 (拥塞窗口)
    tau_high: f64,
    /// 丢包率阈值 - 低
    p_low: f64,
    /// 丢包率阈值 - 高
    p_high: f64,
    /// 上调速率 (每秒最多增加的比例)
    alpha_per_s: f64,
    /// 下调系数 (拥塞时的乘法因子)
    beta: f64,
    /// 平滑系数 (EMA)
    gamma: f64,
    /// 头顶余量
    headroom: f64,
    /// 下限带宽 (Bytes/s)
    b_floor: f64,
    /// 上限带宽 (Bytes/s)
    b_cap: f64,
}

impl Default for BandwidthEstimator {
    fn default() -> Self {
        Self::new()
    }
}

impl BandwidthEstimator {
    /// 创建新的带宽估计器，初始值 30 Mbps
    pub fn new() -> Self {
        Self {
            b_hat: 30.0 * 1_000_000.0,  // 30 Mbps
            last_update: Instant::now(),
            last_minrtt_ms: 0.0,
            last_tx_bytes: 0,
            recent_throughput_bps: 0.0,
            
            // 默认参数
            tau_low: 0.05,      // qnorm ≤ 5% 认为干净
            tau_high: 0.20,     // qnorm ≥ 20% 认为拥塞
            p_low: 0.002,       // 0.2% 丢包
            p_high: 0.01,       // 1% 丢包
            alpha_per_s: 0.10,  // 每秒最多 +10%
            beta: 0.30,         // 拥塞时 ×(1-30%)
            gamma: 0.20,        // 20% EMA 平滑
            headroom: 1.10,     // +10% 余量
            b_floor: 5.0 * 1_000_000.0,    // 5 Mbps
            b_cap: 1.0 * 1_000_000_000.0,  // 1 Gbps
        }
    }
    
    /// 更新带宽估计
    /// 
    /// # 参数
    /// - `current_tx_bytes`: 当前累计发送字节数
    /// - `srtt_ms`: 平滑 RTT (毫秒)
    /// - `rttvar_ms`: RTT 方差 (毫秒)
    /// - `minrtt_ms`: 最小 RTT (毫秒)
    /// - `loss_rate`: 丢包率 (0.0..1.0)
    pub fn update(
        &mut self,
        current_tx_bytes: u64,
        srtt_ms: f64,
        rttvar_ms: f64,
        minrtt_ms: f64,
        loss_rate: f64,
    ) {
        let now = Instant::now();
        let dt = (now - self.last_update).as_secs_f64();
        
        // 至少间隔 100ms 才更新
        if dt < 0.1 {
            return;
        }
        
        // 计算实际吞吐
        let delta_bytes = current_tx_bytes.saturating_sub(self.last_tx_bytes);
        let instant_throughput = if dt > 0.0 {
            delta_bytes as f64 / dt
        } else {
            0.0
        };
        
        // EWMA 平滑吞吐
        if self.recent_throughput_bps > 0.0 {
            self.recent_throughput_bps = 0.3 * instant_throughput + 0.7 * self.recent_throughput_bps;
        } else {
            self.recent_throughput_bps = instant_throughput;
        }
        
        let throughput = self.recent_throughput_bps;
        
        // 计算排队延迟
        let base = minrtt_ms.max(1.0);
        let q = (srtt_ms - base).max(0.0);
        let qnorm = (q / base).min(5.0);  // 限制最大值
        
        // 路径突变保护
        if self.last_minrtt_ms > 0.0 {
            let minrtt_jump = (minrtt_ms - self.last_minrtt_ms).abs();
            let jump_threshold = (3.0 * rttvar_ms).max(20.0);
            if minrtt_jump > jump_threshold {
                // 检测到路径突变，重置估计值
                self.b_hat = (30.0 * 1_000_000.0 + self.b_hat) / 2.0;
                tracing::debug!(
                    "BW estimator: path change detected (minrtt jump {:.2}ms > {:.2}ms), reset b_hat to {:.2} Mbps",
                    minrtt_jump, jump_threshold, self.b_hat / 1_000_000.0
                );
            }
        }
        self.last_minrtt_ms = minrtt_ms;
        
        // 根据状态更新带宽估计
        let old_b_hat = self.b_hat;
        
        if qnorm <= self.tau_low && loss_rate <= self.p_low {
            // 干净窗口 - 温和上调
            if throughput > 0.0 {
                // 有流量时才上调
                let up_cap = self.b_hat * (1.0 + self.alpha_per_s * dt);
                let target = (throughput * self.headroom).min(up_cap);
                self.b_hat = ((1.0 - self.gamma) * self.b_hat + self.gamma * target)
                    .clamp(self.b_floor, self.b_cap);
                
                tracing::trace!(
                    "BW estimator: clean window with traffic, qnorm={:.3}, loss={:.4}, throughput={:.2}Mbps, adjust from {:.2} to {:.2} Mbps",
                    qnorm, loss_rate, throughput / 1_000_000.0,
                    old_b_hat / 1_000_000.0, self.b_hat / 1_000_000.0
                );
            } else {
                // 零流量时保持不变
                tracing::trace!(
                    "BW estimator: clean window but zero traffic, keeping b_hat at {:.2} Mbps",
                    self.b_hat / 1_000_000.0
                );
            }
        } else if qnorm < self.tau_high && loss_rate < self.p_high {
            // 中性窗口 - 保持/微调
            let target = self.b_hat.max(throughput);
            self.b_hat = (1.0 - self.gamma) * self.b_hat + self.gamma * target;
            
            tracing::trace!(
                "BW estimator: neutral window, qnorm={:.3}, loss={:.4}, hold at {:.2} Mbps",
                qnorm, loss_rate, self.b_hat / 1_000_000.0
            );
        } else {
            // 拥塞窗口 - 快速下调
            let g_q = ((qnorm - self.tau_high) / (1.0 - self.tau_high)).clamp(0.0, 1.0);
            let g_p = (loss_rate / self.p_high).clamp(0.0, 1.0);
            let g = g_q.max(g_p);
            
            self.b_hat = (self.b_hat.max(throughput) * (1.0 - self.beta * g))
                .max(self.b_floor);
            
            tracing::debug!(
                "BW estimator: congestion detected, qnorm={:.3}, loss={:.4}, g={:.3}, down from {:.2} to {:.2} Mbps",
                qnorm, loss_rate, g,
                old_b_hat / 1_000_000.0, self.b_hat / 1_000_000.0
            );
        }
        
        // 更新状态
        self.last_update = now;
        self.last_tx_bytes = current_tx_bytes;
    }
    
    /// 获取当前带宽估计值 (Bytes/s)
    pub fn get_bw_bps(&self) -> f64 {
        self.b_hat
    }
    
    /// 获取最近的实际吞吐 (Bytes/s)
    pub fn get_recent_throughput_bps(&self) -> f64 {
        self.recent_throughput_bps
    }
}

pub struct WindowLatency {
    start_time: Instant,
    latency_us_window: Vec<AtomicU32>,
    latency_us_window_ts: Vec<AtomicU64>,
    latency_us_window_index: AtomicU32,
    latency_us_window_size: u32,

    sum: AtomicU32,
    count: AtomicU32,
}

impl std::fmt::Debug for WindowLatency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowLatency")
            .field("count", &self.count)
            .field("window_size", &self.latency_us_window_size)
            .field("window_latency", &self.get_latency_us::<u32>())
            .finish()
    }
}

impl WindowLatency {
    pub fn new(window_size: u32) -> Self {
        Self {
            start_time: Instant::now(),
            latency_us_window: (0..window_size).map(|_| AtomicU32::new(0)).collect(),
            latency_us_window_ts: (0..window_size).map(|_| AtomicU64::new(0)).collect(),
            latency_us_window_index: AtomicU32::new(0),
            latency_us_window_size: window_size,

            sum: AtomicU32::new(0),
            count: AtomicU32::new(0),
        }
    }

    pub fn record_latency(&self, latency_us: u32) {
        let index = self.latency_us_window_index.fetch_add(1, Relaxed);
        if self.count.load(Relaxed) < self.latency_us_window_size {
            self.count.fetch_add(1, Relaxed);
        }

        let index = index % self.latency_us_window_size;
        let old_lat = self.latency_us_window[index as usize].swap(latency_us, Relaxed);
        let ts = self.start_time.elapsed().as_millis() as u64;
        self.latency_us_window_ts[index as usize].store(ts, Relaxed);
        // tracing::debug!("record_latency: {} us to index {} time {}", latency_us, index, ts);

        if old_lat < latency_us {
            self.sum.fetch_add(latency_us - old_lat, Relaxed);
        } else {
            self.sum.fetch_sub(old_lat - latency_us, Relaxed);
        }
    }

    pub fn get_latency_us<T: From<u32> + std::ops::Div<Output = T>>(&self) -> T {
        let count = self.count.load(Relaxed);
        let sum = self.sum.load(Relaxed);
        if count == 0 {
            0.into()
        } else {
            (T::from(sum)) / T::from(count)
        }
    }
    
    /// 获取 RTT 方差（标准差的估算，使用简化方法）
    pub fn get_rttvar_us(&self) -> u32 {
        let count = self.count.load(Relaxed);
        if count == 0 {
            return 0;
        }
        
        let mean = self.get_latency_us::<u32>();
        let mut var_sum = 0u64;
        
        for i in 0..count.min(self.latency_us_window_size) {
            let lat = self.latency_us_window[i as usize].load(Relaxed);
            let diff = if lat > mean { lat - mean } else { mean - lat };
            var_sum += diff as u64 * diff as u64;
        }
        
        let variance = var_sum / count as u64;
        (variance as f64).sqrt() as u32
    }
    
    /// 获取最小 RTT（窗口内）
    pub fn get_min_rtt_us(&self) -> u32 {
        let count = self.count.load(Relaxed);
        if count == 0 {
            return 0;
        }
        
        let mut min_rtt = u32::MAX;
        for i in 0..count.min(self.latency_us_window_size) {
            let lat = self.latency_us_window[i as usize].load(Relaxed);
            if lat > 0 && lat < min_rtt {
                min_rtt = lat;
            }
        }
        
        if min_rtt == u32::MAX {
            0
        } else {
            min_rtt
        }
    }

    pub fn get_last_response_age_ms<T: From<u64> + std::ops::Div<Output = T>>(&self) -> T {
        let index = self.latency_us_window_index.load(Relaxed);
        if index == 0 {
            return 0xffffffff.into();
        }
        let index = (index - 1) % self.latency_us_window_size;
        let last_ts = self.latency_us_window_ts[index as usize].load(Relaxed);
        // tracing::debug!("get_last_response_age_ms: {} us from index {}", last_ts, index);
        let since_last_ts = self.start_time.elapsed().as_millis() as u64 - last_ts;
        T::from(since_last_ts.try_into().unwrap())
    }
}

pub struct TimeBucketInFlight {
    buckets: Vec<u64>,   // 每个时间桶累计的已发送字节
    head: usize,         // 当前时间桶索引
    slot_ms: u32,        // 每个桶的时间长度
    last_tick: Instant,  // 上次“对齐”时间
    started: Instant,    // 起始时间
}

impl TimeBucketInFlight {
    /// slot_ms: 每桶毫秒；num_slots: 桶数量（覆盖窗口）；建议 slot_ms=10, num_slots=256
    pub fn new(slot_ms: u32, num_slots: usize) -> Self {
        assert!(slot_ms > 0 && num_slots > 0);
        Self {
            buckets: vec![0; num_slots],
            head: 0,
            slot_ms,
            last_tick: Instant::now(),
            started: Instant::now(),
        }
    }

    /// 发送时调用（O(1)）：把 size 字节计入当前时间桶
    pub fn on_send(&mut self, size: usize) {
        self.advance_to_now();
        self.buckets[self.head] = self.buckets[self.head].saturating_add(size as u64);
    }

    /// 估算 in-flight 字节数：累加“最近 gamma * sRTT”窗口内的桶
    pub fn estimate_inflight(&mut self, srtt_ms: u32, gamma: f32) -> u64 {
        self.advance_to_now();

        // 计算需要回看多少个桶
        let srtt_ms = srtt_ms.max(self.slot_ms); // 避免 0
        let horizon_ms = (gamma.max(1.0) * srtt_ms as f32) as u32;
        let horizon_slots = (horizon_ms as usize / self.slot_ms as usize)
            .clamp(1, self.buckets.len());

        // 从 head 往回累计 horizon_slots 个桶
        let mut sum = 0u64;
        let n = self.buckets.len();
        let mut idx = self.head;
        for _ in 0..horizon_slots {
            sum = sum.saturating_add(self.buckets[idx]);
            idx = (idx + n - 1) % n;
        }
        sum
    }

    /// 将当前时间对齐到对应的时间桶，推进 head 并清零“跨过”的桶
    fn advance_to_now(&mut self) {
        let now = Instant::now();
        let elapsed_ms = (now - self.last_tick).as_millis() as u64;
        if elapsed_ms == 0 { return; }

        let slots_advanced = (elapsed_ms / self.slot_ms as u64) as usize;
        if slots_advanced == 0 { return; }

        let n = self.buckets.len();
        for _ in 0..slots_advanced.min(n) {
            self.head = (self.head + 1) % n;
            self.buckets[self.head] = 0; // 新时间桶清零
        }
        // 如果一次跨过超过 n 个桶，相当于窗口全过期，直接清空
        if slots_advanced >= n {
            self.buckets.fill(0);
        }
        self.last_tick += Duration::from_millis((slots_advanced as u64) * self.slot_ms as u64);
    }
}


#[derive(Debug)]
pub struct Throughput {
    start_time: Instant,
    tx_bytes: UnsafeCell<u64>,
    rx_bytes: UnsafeCell<u64>,
    tx_packets: UnsafeCell<u64>,
    rx_packets: UnsafeCell<u64>,
    data_tx_bytes: UnsafeCell<u64>,
    data_rx_bytes: UnsafeCell<u64>,
    data_tx_packets: UnsafeCell<u64>,
    data_rx_packets: UnsafeCell<u64>,

    last_tx: UnsafeCell<u64>,
    last_rx: UnsafeCell<u64>,

    inflight: UnsafeCell<TimeBucketInFlight>,
    
    // 被动带宽估计器
    bw_estimator: UnsafeCell<BandwidthEstimator>,
    
    // 最近2s流量统计（用于multipath的flow_bytes_sent）
    recent_tx_bytes: UnsafeCell<u64>,  // 最近2s发送的字节数
    recent_tx_window_start: UnsafeCell<Instant>,  // 窗口起始时间
    
    // 最近2s接收流量统计
    recent_rx_bytes: UnsafeCell<u64>,  // 最近2s接收的字节数
    recent_rx_window_start: UnsafeCell<Instant>,  // 窗口起始时间
}

impl Clone for Throughput {
    fn clone(&self) -> Self {
        Self {
            start_time: self.start_time,
            
            tx_bytes: UnsafeCell::new(unsafe { *self.tx_bytes.get() }),
            rx_bytes: UnsafeCell::new(unsafe { *self.rx_bytes.get() }),
            tx_packets: UnsafeCell::new(unsafe { *self.tx_packets.get() }),
            rx_packets: UnsafeCell::new(unsafe { *self.rx_packets.get() }),
            
            data_tx_bytes: UnsafeCell::new(unsafe { *self.data_tx_bytes.get() }),
            data_rx_bytes: UnsafeCell::new(unsafe { *self.data_rx_bytes.get() }),
            data_tx_packets: UnsafeCell::new(unsafe { *self.data_tx_packets.get() }),
            data_rx_packets: UnsafeCell::new(unsafe { *self.data_rx_packets.get() }),

            last_tx: UnsafeCell::new(unsafe { *self.last_tx.get() }),
            last_rx: UnsafeCell::new(unsafe { *self.last_rx.get() }),

            inflight: UnsafeCell::new(TimeBucketInFlight::new(10, 256)),
            
            bw_estimator: UnsafeCell::new(unsafe { (*self.bw_estimator.get()).clone() }),

            recent_tx_bytes: UnsafeCell::new(unsafe { *self.recent_tx_bytes.get() }),
            recent_tx_window_start: UnsafeCell::new(unsafe { *self.recent_tx_window_start.get() }),
            
            recent_rx_bytes: UnsafeCell::new(unsafe { *self.recent_rx_bytes.get() }),
            recent_rx_window_start: UnsafeCell::new(unsafe { *self.recent_rx_window_start.get() }),
        }
    }
}

// add sync::Send and sync::Sync traits to Throughput
unsafe impl Send for Throughput {}
unsafe impl Sync for Throughput {}

impl Default for Throughput {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            start_time: now,
            tx_bytes: UnsafeCell::new(0),
            rx_bytes: UnsafeCell::new(0),
            tx_packets: UnsafeCell::new(0),
            rx_packets: UnsafeCell::new(0),
            data_tx_bytes: UnsafeCell::new(0),
            data_rx_bytes: UnsafeCell::new(0),
            data_tx_packets: UnsafeCell::new(0),
            data_rx_packets: UnsafeCell::new(0),

            last_tx: UnsafeCell::new(0),
            last_rx: UnsafeCell::new(0),
            
            inflight: UnsafeCell::new(TimeBucketInFlight::new(10, 256)),
            
            bw_estimator: UnsafeCell::new(BandwidthEstimator::new()),

            recent_tx_bytes: UnsafeCell::new(0),
            recent_tx_window_start: UnsafeCell::new(now),
            
            recent_rx_bytes: UnsafeCell::new(0),
            recent_rx_window_start: UnsafeCell::new(now),
        }
    }
}

impl Throughput {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn tx_bytes(&self) -> u64 {
        unsafe { *self.tx_bytes.get() }
    }

    pub fn rx_bytes(&self) -> u64 {
        unsafe { *self.rx_bytes.get() }
    }

    pub fn tx_packets(&self) -> u64 {
        unsafe { *self.tx_packets.get() }
    }

    pub fn rx_packets(&self) -> u64 {
        unsafe { *self.rx_packets.get() }
    }

    pub fn data_tx_bytes(&self) -> u64 {
        unsafe { *self.data_tx_bytes.get() }
    }

    pub fn data_rx_bytes(&self) -> u64 {
        unsafe { *self.data_rx_bytes.get() }
    }

    pub fn data_tx_packets(&self) -> u64 {
        unsafe { *self.data_tx_packets.get() }
    }

    pub fn data_rx_packets(&self) -> u64 {
        unsafe { *self.data_rx_packets.get() }
    }

    pub fn last_tx_age_ms(&self) -> u64 {
        self.start_time.elapsed().as_millis() as u64 - unsafe { *self.last_tx.get() }
    }
    pub fn last_rx_age_ms(&self) -> u64 {
        self.start_time.elapsed().as_millis() as u64 - unsafe { *self.last_rx.get() }
    }

    pub fn estimated_inflight_bytes(&self, srtt_ms: u32, gamma: f32) -> u64 {
        unsafe { (*self.inflight.get()).estimate_inflight(srtt_ms, gamma) }
    }
    
    /// 更新带宽估计（被动观测法）
    /// 
    /// # 参数
    /// - `srtt_ms`: 平滑 RTT (毫秒)
    /// - `rttvar_ms`: RTT 方差 (毫秒)
    /// - `minrtt_ms`: 最小 RTT (毫秒)
    /// - `loss_rate`: 丢包率 (0.0..1.0)
    pub fn update_bw_estimate(
        &self,
        srtt_ms: f64,
        rttvar_ms: f64,
        minrtt_ms: f64,
        loss_rate: f64,
    ) {
        unsafe {
            let current_tx_bytes = *self.data_tx_bytes.get();
            (*self.bw_estimator.get()).update(
                current_tx_bytes,
                srtt_ms,
                rttvar_ms,
                minrtt_ms,
                loss_rate,
            );
        }
    }
    
    /// 获取带宽估计值（Bytes/s）
    pub fn get_bw_est_bps(&self) -> f64 {
        unsafe { (*self.bw_estimator.get()).get_bw_bps() }
    }
    
    /// 获取最近的实际吞吐（Bytes/s）
    pub fn get_recent_throughput_bps(&self) -> f64 {
        unsafe { (*self.bw_estimator.get()).get_recent_throughput_bps() }
    }
    
    /// 获取最近2秒发送的字节数（用于multipath的flow_bytes_sent）
    pub fn get_recent_tx_bytes(&self) -> u64 {
        unsafe {
            let now = Instant::now();
            let window_start = *self.recent_tx_window_start.get();
            let elapsed = (now - window_start).as_secs();
            
            // 如果超过2秒，重置窗口
            if elapsed >= 2 {
                *self.recent_tx_window_start.get() = now;
                *self.recent_tx_bytes.get() = 0;
                return 0;
            }
            
            *self.recent_tx_bytes.get()
        }
    }
    
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

    pub fn record_tx_bytes(&self, bytes: u64, is_data: bool) {
        unsafe {
            *self.tx_bytes.get() += bytes;
            *self.tx_packets.get() += 1;
            *self.last_tx.get() = self.start_time.elapsed().as_millis() as u64;
            if is_data {
                *self.data_tx_bytes.get() += bytes;
                *self.data_tx_packets.get() += 1;
                (*self.inflight.get()).on_send(bytes as usize);
            }

            // 更新最近2秒的流量统计
            let now = Instant::now();
            let recent_window = Duration::from_secs(2);
            let window_start = *self.recent_tx_window_start.get();
            if now.duration_since(window_start) >= recent_window {
                *self.recent_tx_bytes.get() = 0;  // 超过窗口清零
                *self.recent_tx_window_start.get() = now;
            }
            *self.recent_tx_bytes.get() += bytes;
        }
    }

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_throughput_bw_estimate() {
        let throughput = Throughput::new();
        
        // 初始带宽应该是30 Mbps
        let initial_bw = throughput.get_bw_est_bps();
        println!("Initial bw_est: {:.2} Mbps", initial_bw / 1_000_000.0);
        assert_eq!(initial_bw, 30.0 * 1_000_000.0, "初始带宽应该是30 Mbps");
        
        // 模拟发送 1MB 数据
        for _ in 0..100 {
            throughput.record_tx_bytes(10_000, true); // 每次10KB，is_data=true
        }
        
        // 等待100ms
        thread::sleep(Duration::from_millis(100));
        
        // 第一次更新带宽估计 (干净窗口: qnorm=0.25, loss=0)
        // srtt=50ms, minrtt=40ms -> qnorm = (50-40)/40 = 0.25 (中性窗口)
        throughput.update_bw_estimate(50.0, 10.0, 40.0, 0.0);
        let bw1 = throughput.get_bw_est_bps();
        let thr1 = throughput.get_recent_throughput_bps();
        println!("After 100ms: bw_est={:.2} Mbps, throughput={:.2} Mbps", 
            bw1 / 1_000_000.0, thr1 / 1_000_000.0);
        
        // 实际吞吐约为 1MB / 0.1s = 10MB/s
        // 在中性窗口下，带宽估计应该接近实际吞吐或保持
        assert!(bw1 > 0.0, "带宽估计应该大于0");
        assert!(thr1 > 5_000_000.0, "吞吐应该约10MB/s");
        
        // 再发送500KB
        for _ in 0..50 {
            throughput.record_tx_bytes(10_000, true);
        }
        
        // 再等待100ms
        thread::sleep(Duration::from_millis(100));
        
        // 第二次更新 (干净窗口: qnorm=0.025, loss=0)
        // srtt=41ms, minrtt=40ms -> qnorm = (41-40)/40 = 0.025 < 0.05 (干净窗口)
        throughput.update_bw_estimate(41.0, 5.0, 40.0, 0.0);
        let bw2 = throughput.get_bw_est_bps();
        let thr2 = throughput.get_recent_throughput_bps();
        println!("After another 100ms: bw_est={:.2} Mbps, throughput={:.2} Mbps", 
            bw2 / 1_000_000.0, thr2 / 1_000_000.0);
        
        // 在干净窗口下，有流量时应该朝吞吐方向调整
        // 由于第二次吞吐较低（500KB/100ms = 5MB/s），带宽估计会被拉低
        // 但应该在合理范围内
        assert!(bw2 > 0.0 && bw2 < 50.0 * 1_000_000.0, 
            "带宽估计应该在合理范围内: {:.2} Mbps", bw2 / 1_000_000.0);
        
        // 验证累计字节数
        assert_eq!(throughput.data_tx_bytes(), 1_500_000, "总共发送了1.5MB");
    }
    
    #[test]
    fn test_throughput_recent_bytes() {
        let throughput = Throughput::new();
        
        // 发送一些数据
        throughput.record_tx_bytes(100_000, true);
        assert_eq!(throughput.get_recent_tx_bytes(), 100_000);
        
        throughput.record_tx_bytes(50_000, true);
        assert_eq!(throughput.get_recent_tx_bytes(), 150_000);
        
        // 等待超过2秒
        thread::sleep(Duration::from_millis(2100));
        
        // 窗口应该被重置
        let recent = throughput.get_recent_tx_bytes();
        assert_eq!(recent, 0, "超过2秒后应该重置为0");
        
        // 再发送一些数据
        throughput.record_tx_bytes(200_000, true);
        assert_eq!(throughput.get_recent_tx_bytes(), 200_000);
    }
    
    #[test]
    fn test_throughput_inflight() {
        let throughput = Throughput::new();
        
        // 发送一些数据
        for _ in 0..10 {
            throughput.record_tx_bytes(1500, true);
        }
        
        // 估算 in-flight（假设 RTT 50ms）
        let inflight = throughput.estimated_inflight_bytes(50, 1.5);
        println!("Estimated inflight: {} bytes", inflight);
        assert!(inflight > 0, "应该有 in-flight 数据");
    }
    
    #[test]
    fn test_bw_estimate_with_zero_traffic() {
        let throughput = Throughput::new();
        
        let bw_before = throughput.get_bw_est_bps();
        println!("Before update: {:.2} Mbps", bw_before / 1_000_000.0);
        
        // 不发送任何数据，直接更新带宽
        thread::sleep(Duration::from_millis(100));
        // qnorm=(50-40)/40=0.25，属于中性窗口
        throughput.update_bw_estimate(50.0, 10.0, 40.0, 0.0);
        
        let bw_after = throughput.get_bw_est_bps();
        let thr = throughput.get_recent_throughput_bps();
        println!("After update: bw={:.2} Mbps, throughput={:.2} Mbps", 
            bw_after / 1_000_000.0, thr / 1_000_000.0);
        
        // 零流量时应该接近初始值（允许小幅波动）
        assert!((bw_after - 30.0 * 1_000_000.0).abs() < 1_000_000.0, 
            "零流量时应保持接近初始带宽，expected=30Mbps, actual={:.2}Mbps", 
            bw_after / 1_000_000.0);
    }
    
    #[test]
    fn test_bw_estimate_congestion() {
        let throughput = Throughput::new();
        
        // 第一次：发送1MB，100ms，干净窗口
        for _ in 0..100 {
            throughput.record_tx_bytes(10_000, true);
        }
        thread::sleep(Duration::from_millis(100));
        throughput.update_bw_estimate(41.0, 5.0, 40.0, 0.0);  // qnorm=0.025，干净
        let bw1 = throughput.get_bw_est_bps();
        println!("BW1 (clean): {:.2} Mbps", bw1 / 1_000_000.0);
        
        // 第二次：发送100KB，100ms，但检测到拥塞
        for _ in 0..10 {
            throughput.record_tx_bytes(10_000, true);
        }
        thread::sleep(Duration::from_millis(100));
        // srtt=60ms, minrtt=40ms -> qnorm = (60-40)/40 = 0.5 > 0.2 (拥塞)
        throughput.update_bw_estimate(60.0, 15.0, 40.0, 0.005);
        let bw2 = throughput.get_bw_est_bps();
        println!("BW2 (congestion): {:.2} Mbps", bw2 / 1_000_000.0);
        
        // 拥塞时应该下调
        assert!(bw2 < bw1, "检测到拥塞时应该降低带宽估计");
    }
    
    #[test]
    fn test_bw_estimate_path_change() {
        let throughput = Throughput::new();
        
        // 发送一些数据
        for _ in 0..100 {
            throughput.record_tx_bytes(10_000, true);
        }
        thread::sleep(Duration::from_millis(100));
        
        // 第一次更新 (minrtt=40ms)
        throughput.update_bw_estimate(50.0, 10.0, 40.0, 0.0);
        let bw1 = throughput.get_bw_est_bps();
        println!("BW1: {:.2} Mbps (minrtt=40ms)", bw1 / 1_000_000.0);
        
        // 再发送一些数据
        for _ in 0..50 {
            throughput.record_tx_bytes(10_000, true);
        }
        thread::sleep(Duration::from_millis(100));
        
        // 模拟路径切换 (minrtt从40ms突变到120ms，差值80ms > 3*20ms=60ms)
        // rttvar=20ms，所以阈值是max(60ms, 20ms) = 60ms
        // 80ms > 60ms，应该触发路径突变保护
        throughput.update_bw_estimate(130.0, 20.0, 120.0, 0.0);
        let bw2 = throughput.get_bw_est_bps();
        let thr = throughput.get_recent_throughput_bps();
        println!("BW2 (after path change): {:.2} Mbps (minrtt=120ms, throughput={:.2}Mbps)", 
            bw2 / 1_000_000.0, thr / 1_000_000.0);
        
        // 路径突变应该首先重置带宽，然后根据拥塞状态调整
        // qnorm = (130-120)/120 = 0.083，属于中性窗口
        // 所以重置后不会再大幅调整
        // 验证带宽发生了明显变化（可能上升也可能下降，取决于新吞吐）
        println!("BW change: from {:.2} to {:.2} Mbps", bw1 / 1_000_000.0, bw2 / 1_000_000.0);
        assert!(bw2 > 0.0 && bw2 != bw1, "路径突变后带宽应该有所变化");
    }
}
