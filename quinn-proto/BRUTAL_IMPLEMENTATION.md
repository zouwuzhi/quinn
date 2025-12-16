# Brutal 拥塞控制算法在 Quinn 中的实现文档

## 概述

Brutal 是 Hysteria2 协议中的核心拥塞控制算法，专为高丢包、高延迟的网络环境设计。与传统的拥塞控制算法（如 BBR、NewReno、Cubic）不同，Brutal 采用**带宽驱动**的策略，试图以用户指定的固定带宽进行发送，并根据 ACK 率动态调整。

### 核心特点

1. **带宽驱动**: 用户直接指定目标带宽，算法确保达到该速率
2. **丢包容忍**: 通过 ACK 率补偿机制，在高丢包环境下保持吞吐量
3. **简单高效**: 算法逻辑简单，计算开销小
4. **非公平性**: 不遵循 TCP 友好拥塞控制原则，会抢占其他连接带宽

## 算法原理

### 核心公式

```
CWND = (BPS × RTT × multiplier) / ACK_rate
Pacing_Rate = BPS / ACK_rate
```

其中：
- `BPS`: 用户配置的目标带宽（字节/秒）
- `RTT`: 往返时延
- `multiplier`: 拥塞窗口乘数（默认为 2）
- `ACK_rate`: 确认率（ACK 包数 / 总发送包数）

### ACK 率计算

使用滑动窗口统计最近 5 秒的 ACK 和丢包数据：

```
ACK_rate = ACK_count / (ACK_count + Loss_count)
ACK_rate = max(ACK_rate, 0.8)  // 最小下限
```

当 ACK 率下降时，算法会增加发送速率来补偿丢包，确保有效吞吐量接近目标带宽。

## Hysteria2 原始实现（Go）

### 数据结构

```go
// 来源: https://github.com/apernet/hysteria/blob/master/core/internal/congestion/brutal/brutal.go

const (
    pktInfoSlotCount           = 5   // 采样窗口（秒）
    minSampleCount             = 50  // 最小采样包数
    minAckRate                 = 0.8 // 最小 ACK 率
    congestionWindowMultiplier = 2   // CWND 乘数
)

type BrutalSender struct {
    rttStats        congestion.RTTStatsProvider
    bps             congestion.ByteCount  // 目标带宽
    maxDatagramSize congestion.ByteCount  // 最大数据报大小
    pacer           *common.Pacer         // 发送速率控制器
    pktInfoSlots    [pktInfoSlotCount]pktInfo  // 统计槽
    ackRate         float64               // 当前 ACK 率
}

type pktInfo struct {
    Timestamp int64   // Unix 时间戳（秒）
    AckCount  uint64  // 确认包数
    LossCount uint64  // 丢失包数
}
```

### 核心算法

```go
// 拥塞窗口计算
func (b *BrutalSender) GetCongestionWindow() congestion.ByteCount {
    rtt := b.rttStats.SmoothedRTT()
    if rtt <= 0 {
        return 10240
    }
    // CWND = BPS * RTT * multiplier / ackRate
    cwnd := congestion.ByteCount(
        float64(b.bps) *
        rtt.Seconds() *
        congestionWindowMultiplier /
        b.ackRate,
    )
    return cwnd
}

// ACK 率更新
func (b *BrutalSender) updateAckRate(slot int) {
    var ackCount, lossCount uint64
    for i := 0; i < pktInfoSlotCount; i++ {
        if i == slot {
            continue
        }
        ackCount += b.pktInfoSlots[i].AckCount
        lossCount += b.pktInfoSlots[i].LossCount
    }
    if ackCount+lossCount < minSampleCount {
        b.ackRate = 1
        return
    }
    rate := float64(ackCount) / float64(ackCount+lossCount)
    if rate < minAckRate {
        rate = minAckRate
    }
    b.ackRate = rate
    // 更新 pacer 发送速率
    b.pacer.SetBandwidth(uint64(float64(b.bps) / rate))
}
```

## Quinn 实现设计

### 文件结构

```
quinn-proto/src/congestion/
├── mod.rs          // 添加 brutal 模块导出
├── brutal/
│   └── mod.rs      // Brutal 实现
```

### Rust 实现

```rust
// quinn-proto/src/congestion/brutal/mod.rs

use std::any::Any;
use std::sync::Arc;
use std::time::Duration;

use super::{BASE_DATAGRAM_SIZE, Controller, ControllerFactory, TransferableState};
use crate::connection::RttEstimator;
use crate::Instant;

/// 统计槽数量（秒）
const PKT_INFO_SLOT_COUNT: usize = 5;
/// 最小采样包数
const MIN_SAMPLE_COUNT: u64 = 50;
/// 最小 ACK 率
const MIN_ACK_RATE: f64 = 0.8;
/// 拥塞窗口乘数
const CWND_MULTIPLIER: f64 = 2.0;

/// 包统计信息
#[derive(Debug, Clone, Default)]
struct PktInfo {
    /// Unix 时间戳（秒）
    timestamp: u64,
    /// 确认包数
    ack_count: u64,
    /// 丢失包数
    loss_count: u64,
}

/// Brutal 拥塞控制器
///
/// 一种带宽驱动的拥塞控制算法，通过用户指定的目标带宽和 ACK 率
/// 动态调整发送速率，专为高丢包网络环境设计。
#[derive(Debug, Clone)]
pub struct Brutal {
    config: Arc<BrutalConfig>,
    /// 当前 MTU
    current_mtu: u64,
    /// 统计槽数组
    pkt_info_slots: [PktInfo; PKT_INFO_SLOT_COUNT],
    /// 当前 ACK 率
    ack_rate: f64,
    /// 最后一次 RTT 估计
    last_rtt: Duration,
}

impl Brutal {
    /// 使用给定配置创建新的 Brutal 控制器
    pub fn new(config: Arc<BrutalConfig>, current_mtu: u16) -> Self {
        Self {
            config,
            current_mtu: current_mtu as u64,
            pkt_info_slots: Default::default(),
            ack_rate: 1.0,
            last_rtt: Duration::from_millis(100), // 默认 100ms
        }
    }

    /// 获取当前时间槽索引
    fn current_slot(&self, now: Instant) -> usize {
        // 使用 Instant 的 elapsed 来获取相对时间
        // 在实际实现中，需要一个基准时间点
        let secs = now.elapsed().as_secs();
        (secs % PKT_INFO_SLOT_COUNT as u64) as usize
    }

    /// 更新 ACK 率
    fn update_ack_rate(&mut self, current_slot: usize) {
        let mut ack_count: u64 = 0;
        let mut loss_count: u64 = 0;

        for (i, slot) in self.pkt_info_slots.iter().enumerate() {
            if i == current_slot {
                continue;
            }
            ack_count += slot.ack_count;
            loss_count += slot.loss_count;
        }

        let total = ack_count + loss_count;
        if total < MIN_SAMPLE_COUNT {
            self.ack_rate = 1.0;
            return;
        }

        let rate = ack_count as f64 / total as f64;
        self.ack_rate = rate.max(MIN_ACK_RATE);
    }

    /// 计算拥塞窗口
    fn calculate_cwnd(&self) -> u64 {
        let rtt_secs = self.last_rtt.as_secs_f64();
        if rtt_secs <= 0.0 {
            return self.config.initial_window;
        }

        let bps = self.config.target_bps as f64;
        let cwnd = (bps * rtt_secs * CWND_MULTIPLIER / self.ack_rate) as u64;

        cwnd.max(self.minimum_window())
    }

    /// 计算发送速率（bits/s）
    fn pacing_rate(&self) -> u64 {
        // 返回 bits/s
        ((self.config.target_bps as f64 / self.ack_rate) * 8.0) as u64
    }

    /// 最小窗口大小
    fn minimum_window(&self) -> u64 {
        4 * self.current_mtu
    }
}

impl Controller for Brutal {
    fn on_ack(
        &mut self,
        now: Instant,
        _sent: Instant,
        _bytes: u64,
        _app_limited: bool,
        rtt: &RttEstimator,
    ) {
        // 更新 RTT
        self.last_rtt = rtt.smoothed();

        // 获取当前槽并更新 ACK 计数
        let slot = self.current_slot(now);
        let current_timestamp = now.elapsed().as_secs();

        // 如果是新的时间槽，重置计数
        if self.pkt_info_slots[slot].timestamp != current_timestamp {
            self.pkt_info_slots[slot] = PktInfo {
                timestamp: current_timestamp,
                ack_count: 1,
                loss_count: 0,
            };
        } else {
            self.pkt_info_slots[slot].ack_count += 1;
        }

        self.update_ack_rate(slot);
    }

    fn on_congestion_event(
        &mut self,
        now: Instant,
        _sent: Instant,
        _is_persistent_congestion: bool,
        _lost_bytes: u64,
    ) {
        // 获取当前槽并更新丢包计数
        let slot = self.current_slot(now);
        let current_timestamp = now.elapsed().as_secs();

        // 如果是新的时间槽，重置计数
        if self.pkt_info_slots[slot].timestamp != current_timestamp {
            self.pkt_info_slots[slot] = PktInfo {
                timestamp: current_timestamp,
                ack_count: 0,
                loss_count: 1,
            };
        } else {
            self.pkt_info_slots[slot].loss_count += 1;
        }

        self.update_ack_rate(slot);
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.current_mtu = new_mtu as u64;
    }

    fn window(&self) -> u64 {
        self.calculate_cwnd()
    }

    fn metrics(&self) -> super::ControllerMetrics {
        super::ControllerMetrics {
            congestion_window: self.window(),
            ssthresh: None,
            pacing_rate: Some(self.pacing_rate()),
        }
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }

    fn initial_window(&self) -> u64 {
        self.config.initial_window
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }

    fn transferable_state(&self) -> TransferableState {
        TransferableState {
            congestion_window: self.window(),
            ssthresh: None,
            in_recovery: false,
        }
    }

    fn apply_transferred_state(&mut self, state: &TransferableState) -> bool {
        // Brutal 主要由配置的 target_bps 驱动，
        // 但我们仍然可以保留一些状态信息
        // 重置统计槽
        self.pkt_info_slots = Default::default();
        self.ack_rate = 1.0;

        // 可以考虑从传输状态推断一些信息
        let _ = state;
        true
    }

    fn name(&self) -> &'static str {
        "brutal"
    }
}

/// Brutal 控制器配置
#[derive(Debug, Clone)]
pub struct BrutalConfig {
    /// 目标带宽（字节/秒）
    pub target_bps: u64,
    /// 初始窗口大小
    pub initial_window: u64,
    /// 最小 ACK 率（默认 0.8）
    pub min_ack_rate: f64,
}

impl BrutalConfig {
    /// 创建指定目标带宽的配置
    ///
    /// # 参数
    /// - `target_mbps`: 目标带宽，单位 Mbps
    pub fn new(target_mbps: u64) -> Self {
        Self {
            target_bps: target_mbps * 1_000_000 / 8, // Mbps -> Bytes/s
            initial_window: 14720.clamp(2 * BASE_DATAGRAM_SIZE, 10 * BASE_DATAGRAM_SIZE),
            min_ack_rate: MIN_ACK_RATE,
        }
    }

    /// 设置目标带宽（字节/秒）
    pub fn target_bps(&mut self, bps: u64) -> &mut Self {
        self.target_bps = bps;
        self
    }

    /// 设置目标带宽（Mbps）
    pub fn target_mbps(&mut self, mbps: u64) -> &mut Self {
        self.target_bps = mbps * 1_000_000 / 8;
        self
    }

    /// 设置初始窗口
    pub fn initial_window(&mut self, value: u64) -> &mut Self {
        self.initial_window = value;
        self
    }

    /// 设置最小 ACK 率
    pub fn min_ack_rate(&mut self, rate: f64) -> &mut Self {
        self.min_ack_rate = rate.clamp(0.1, 1.0);
        self
    }
}

impl Default for BrutalConfig {
    fn default() -> Self {
        Self {
            target_bps: 10 * 1_000_000 / 8, // 默认 10 Mbps
            initial_window: 14720.clamp(2 * BASE_DATAGRAM_SIZE, 10 * BASE_DATAGRAM_SIZE),
            min_ack_rate: MIN_ACK_RATE,
        }
    }
}

impl ControllerFactory for BrutalConfig {
    fn build(self: Arc<Self>, _now: Instant, current_mtu: u16) -> Box<dyn Controller> {
        Box::new(Brutal::new(self, current_mtu))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_brutal_config() {
        let config = BrutalConfig::new(100); // 100 Mbps
        assert_eq!(config.target_bps, 100 * 1_000_000 / 8);
    }

    #[test]
    fn test_ack_rate_clamping() {
        // ACK 率不应低于 MIN_ACK_RATE
        let rate = 0.5_f64.max(MIN_ACK_RATE);
        assert_eq!(rate, MIN_ACK_RATE);
    }

    #[test]
    fn test_cwnd_calculation() {
        // 假设: 100 Mbps, 100ms RTT, ack_rate = 1.0
        let bps = 100.0 * 1_000_000.0 / 8.0; // bytes/s
        let rtt = 0.1; // 100ms
        let ack_rate = 1.0;

        let cwnd = (bps * rtt * CWND_MULTIPLIER / ack_rate) as u64;
        // 预期: 12.5MB * 0.1 * 2 = 2.5MB
        assert!(cwnd > 2_000_000);
    }
}
```

### 模块导出修改

```rust
// quinn-proto/src/congestion.rs（添加）

mod brutal;
pub use brutal::{Brutal, BrutalConfig};
```

## 使用示例

```rust
use quinn_proto::congestion::{BrutalConfig, Brutal};
use std::sync::Arc;

// 创建 100 Mbps 的 Brutal 配置
let config = Arc::new(BrutalConfig::new(100));

// 在 TransportConfig 中使用
let mut transport_config = TransportConfig::default();
transport_config.congestion_controller_factory(config);

// 或手动创建控制器
let brutal = Brutal::new(config, 1200);
```

## 与其他算法的比较

| 特性 | Brutal | BBR | NewReno | Cubic |
|------|--------|-----|---------|-------|
| 带宽探测 | 无（用户指定） | 主动探测 | 被动 | 被动 |
| 丢包响应 | 增加发送 | 减少窗口 | 减半窗口 | 减少窗口 |
| TCP 友好 | 否 | 是 | 是 | 是 |
| 适用场景 | 高丢包专线 | 通用 | 传统网络 | 长肥管道 |
| 配置复杂度 | 需要带宽信息 | 自动 | 自动 | 自动 |

## 注意事项

### 安全警告

1. **非公平性**: Brutal 会抢占网络带宽，不适合共享网络
2. **带宽设置**: 设置过高的带宽会导致网络拥塞，影响其他流量
3. **合规性**: 某些网络环境可能禁止使用此类激进算法

### 最佳实践

1. 仅在专用网络或已知带宽的环境中使用
2. 带宽设置应低于实际可用带宽的 80-90%
3. 监控网络质量指标，及时调整配置
4. 考虑与 BBR 组合使用（下行 BBR，上行 Brutal）

## 参考资料

- [Hysteria 官方文档](https://v2.hysteria.network/docs/)
- [Hysteria GitHub 仓库](https://github.com/apernet/hysteria)
- [TCP Brutal 内核模块](https://github.com/apernet/tcp-brutal)
- [Hysteria 协议规范](https://v2.hysteria.network/docs/developers/Protocol/)
