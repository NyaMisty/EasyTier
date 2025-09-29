use atomic_shim::AtomicU64;
use std::{
    cell::UnsafeCell,
    sync::atomic::{AtomicU32, Ordering::Relaxed}, time::Instant,
};

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

#[derive(Debug)]
pub struct Throughput {
    start_time: Instant,
    tx_bytes: UnsafeCell<u64>,
    rx_bytes: UnsafeCell<u64>,
    tx_packets: UnsafeCell<u64>,
    rx_packets: UnsafeCell<u64>,
    last_tx: UnsafeCell<u64>,
    last_rx: UnsafeCell<u64>,
}

impl Clone for Throughput {
    fn clone(&self) -> Self {
        Self {
            start_time: self.start_time,
            tx_bytes: UnsafeCell::new(unsafe { *self.tx_bytes.get() }),
            rx_bytes: UnsafeCell::new(unsafe { *self.rx_bytes.get() }),
            tx_packets: UnsafeCell::new(unsafe { *self.tx_packets.get() }),
            rx_packets: UnsafeCell::new(unsafe { *self.rx_packets.get() }),
            last_tx: UnsafeCell::new(unsafe { *self.last_tx.get() }),
            last_rx: UnsafeCell::new(unsafe { *self.last_rx.get() }),
        }
    }
}

// add sync::Send and sync::Sync traits to Throughput
unsafe impl Send for Throughput {}
unsafe impl Sync for Throughput {}

impl Default for Throughput {
    fn default() -> Self {
        Self {
            start_time: Instant::now(),
            tx_bytes: UnsafeCell::new(0),
            rx_bytes: UnsafeCell::new(0),
            tx_packets: UnsafeCell::new(0),
            rx_packets: UnsafeCell::new(0),
            last_tx: UnsafeCell::new(0),
            last_rx: UnsafeCell::new(0),
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

    pub fn last_tx_age_ms(&self) -> u64 {
        self.start_time.elapsed().as_millis() as u64 - unsafe { *self.last_tx.get() }
    }
    pub fn last_rx_age_ms(&self) -> u64 {
        self.start_time.elapsed().as_millis() as u64 - unsafe { *self.last_rx.get() }
    }

    pub fn record_tx_bytes(&self, bytes: u64) {
        unsafe {
            *self.tx_bytes.get() += bytes;
            *self.tx_packets.get() += 1;
            *self.last_tx.get() = self.start_time.elapsed().as_millis() as u64;
        }
    }

    pub fn record_rx_bytes(&self, bytes: u64) {
        unsafe {
            *self.rx_bytes.get() += bytes;
            *self.rx_packets.get() += 1;
            *self.last_rx.get() = self.start_time.elapsed().as_millis() as u64;
        }
    }
}
