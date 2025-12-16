# 动态拥塞控制算法切换设计文档

## 概述

本文档描述了 quinn-proto 中支持在连接建立后动态修改拥塞控制算法的设计和实现。

## 设计目标

1. **最小侵入性**: 尽量不修改现有核心代码，通过扩展实现
2. **方便合并主分支**: 易于与上游保持同步
3. **灵活的切换策略**: 支持多种状态转移方式

## API 设计

### 公共类型

#### CongestionSwitchStrategy

```rust
pub enum CongestionSwitchStrategy {
    /// 从新算法的初始状态开始（默认，最安全）
    Fresh,

    /// 保守策略：限制初始窗口，保留 ssthresh
    Conservative,

    /// 激进策略：尝试保持当前窗口
    Aggressive,

    /// 自定义窗口值
    WithWindow { cwnd: u64, ssthresh: Option<u64> },
}
```

#### TransferableState

```rust
pub struct TransferableState {
    pub congestion_window: u64,
    pub ssthresh: Option<u64>,
    pub in_recovery: bool,
}
```

### Connection API

```rust
impl Connection {
    /// 动态切换拥塞控制算法
    pub fn set_congestion_controller(
        &mut self,
        factory: Arc<dyn congestion::ControllerFactory + Send + Sync>,
        strategy: CongestionSwitchStrategy,
        now: Instant,
    );

    /// 获取当前拥塞控制器名称
    pub fn congestion_controller_name(&self) -> &'static str;
}
```

## 切换策略详解

### Fresh（无状态转移）

- **行为**: 使用新控制器的默认初始状态
- **优点**: 最安全，无兼容性问题
- **缺点**: 需要通过慢启动重新探测带宽
- **适用场景**: 网络环境发生显著变化时

### Conservative（保守策略）

- **行为**:
  - 窗口取 `min(旧窗口, 2 × 新控制器初始窗口)`
  - 保留旧的 ssthresh
- **优点**: 平衡安全性和性能
- **缺点**: 可能仍有短暂的吞吐量波动
- **适用场景**: 希望快速恢复但避免激进行为

### Aggressive（激进策略）

- **行为**:
  - 尝试保持当前窗口和 ssthresh
  - 如果旧控制器在恢复状态，自动降级为 Conservative
- **优点**: 最大程度保持吞吐量
- **缺点**: 如果新算法计算方式不同，可能导致丢包
- **适用场景**: 网络稳定，仅需切换算法

### WithWindow（自定义）

- **行为**: 使用指定的 cwnd 和 ssthresh
- **适用场景**: 高级用例、测试、特殊需求

## 各控制器的状态转移行为

### NewReno

- 支持完整的状态转移
- 重置 `bytes_acked` 计数器
- 保留 `window` 和 `ssthresh`

### CUBIC

- 支持完整的状态转移
- 重置 CUBIC 特定状态（k, w_max, cwnd_inc）
- 保留 `window` 和 `ssthresh`

### BBR

- 支持窗口转移
- 重置为 Startup 模式以重新探测带宽
- 重置 `max_bandwidth` 估计
- 注意: BBR 没有 ssthresh 概念

## 边界情况处理

| 场景 | 处理方式 |
|------|---------|
| 切换时正在恢复 | Aggressive 策略自动降级为 Conservative |
| 切换时有 in-flight 数据 | 允许，新控制器在收到 ACK 时自然调整 |
| 切换到 BBR | 从 Startup 模式开始，重新探测带宽 |

## 使用示例

```rust
use std::sync::Arc;
use quinn_proto::{
    Connection, CongestionSwitchStrategy,
    congestion::{BbrConfig, CubicConfig, NewRenoConfig},
};

// 从默认（CUBIC）切换到 BBR
connection.set_congestion_controller(
    Arc::new(BbrConfig::default()),
    CongestionSwitchStrategy::Fresh,
    now,
);

// 切换回 CUBIC，保留部分状态
connection.set_congestion_controller(
    Arc::new(CubicConfig::default()),
    CongestionSwitchStrategy::Conservative,
    now,
);

// 使用自定义窗口切换到 NewReno
connection.set_congestion_controller(
    Arc::new(NewRenoConfig::default()),
    CongestionSwitchStrategy::WithWindow {
        cwnd: 100_000,
        ssthresh: Some(80_000),
    },
    now,
);

// 查询当前控制器
println!("Current controller: {}", connection.congestion_controller_name());
```

## 修改的文件

| 文件 | 修改内容 |
|------|---------|
| `congestion.rs` | 添加 `TransferableState`，扩展 `Controller` trait |
| `congestion/new_reno.rs` | 实现新 trait 方法 |
| `congestion/cubic.rs` | 实现新 trait 方法 |
| `congestion/bbr/mod.rs` | 实现新 trait 方法 |
| `connection/paths.rs` | 添加 `set_congestion_controller` 方法 |
| `connection/mod.rs` | 添加 `CongestionSwitchStrategy` 和公共 API |
| `lib.rs` | 导出新类型 |

## Controller Trait 扩展

```rust
pub trait Controller: Send + Sync {
    // ... 现有方法 ...

    /// 获取可转移状态
    fn transferable_state(&self) -> TransferableState {
        TransferableState {
            congestion_window: self.window(),
            ssthresh: self.metrics().ssthresh,
            in_recovery: false,
        }
    }

    /// 应用转移的状态
    fn apply_transferred_state(&mut self, state: &TransferableState) -> bool {
        false  // 默认不支持
    }

    /// 获取控制器名称
    fn name(&self) -> &'static str {
        "unknown"
    }
}
```

## 风险和注意事项

1. **BBR 状态不完全可转移**: BBR 需要重新测量 min_rtt 和 max_bandwidth
2. **Pacing 状态独立**: Pacing 与控制器分离，切换后自动适应
3. **RTT 估计器共享**: 所有算法共享同一个 RTT 估计器
4. **高延迟网络**: Fresh 策略在高延迟网络上恢复时间较长

## 向后兼容性

- 所有新增代码为纯增量添加
- Controller trait 新方法都有默认实现
- 不修改现有公共 API 签名
- 现有代码无需任何修改即可继续工作
