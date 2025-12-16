# 测试指南

## 快速开始

### 运行所有测试

```bash
# 运行所有单元测试
cargo test --lib -p quinn-proto

# 运行动态切换测试
cargo test --lib -p quinn-proto congestion_switch

# 运行 Brutal 算法测试
cargo test --lib -p quinn-proto brutal_tests
```

### 运行示例程序

```bash
# 动态切换演示
cargo run --example dynamic_cc --features="rustls-ring"
```

## 测试文件位置

- **动态切换测试**: `quinn-proto/src/tests/congestion_switch.rs`
- **Brutal 算法测试**: `quinn-proto/src/tests/brutal_tests.rs`
- **演示程序**: `quinn/examples/dynamic_cc.rs`

## 测试内容

### 动态切换功能 (5 个测试)

1. 基础切换功能
2. 状态转移 (Conservative 策略)
3. 多次连续切换
4. 切换到 Brutal
5. 传输过程中切换

### Brutal 算法 (9 个测试)

1. 基础功能
2. 最小窗口限制
3. MTU 更新
4. Pacing rate
5. 状态转移
6. 连接集成
7. 不同带宽配置
8. 配置构建器
9. 克隆功能

## 验证方法

### 1. 验证动态切换

```rust
// 在连接上切换算法
connection.set_congestion_controller(
    Arc::new(BrutalConfig::new(50)),
    CongestionSwitchStrategy::Fresh,
);

// 验证切换成功
assert_eq!(connection.congestion_controller_name(), "brutal");
```

### 2. 验证 Brutal 算法

```rust
// 创建 Brutal 配置
let config = Arc::new(BrutalConfig::new(100)); // 100 Mbps
let brutal = config.build(Instant::now(), 1200);

// 验证基础属性
assert_eq!(brutal.name(), "brutal");
assert!(brutal.window() >= 4 * 1200); // 最小窗口
```

### 3. 验证数据传输

所有测试都包含数据传输验证,确保:
- 切换不会中断连接
- 数据完整传输
- 没有数据包丢失

## 测试结果

最新测试结果: ✅ **285/285 测试通过**

包括:
- ✅ 5 个动态切换测试
- ✅ 9 个 Brutal 算法测试
- ✅ 271 个原有测试

## API 使用示例

### Brutal 配置

```rust
use quinn::congestion::BrutalConfig;

// 简单配置
let config = BrutalConfig::new(100); // 100 Mbps

// 高级配置
let mut config = BrutalConfig::new(100);
config.target_mbps(200).min_ack_rate(0.5);
```

### 切换策略

```rust
use quinn::CongestionSwitchStrategy;

// Fresh: 完全重置状态
CongestionSwitchStrategy::Fresh

// Conservative: 保留较小的拥塞窗口
CongestionSwitchStrategy::Conservative
```

## 调试技巧

### 查看拥塞窗口

```rust
let stats = connection.stats();
println!("拥塞窗口: {} bytes", stats.path.cwnd);
println!("RTT: {:?}", connection.rtt());
```

### 查看当前算法

```rust
println!("当前算法: {}", connection.congestion_controller_name());
```

## 注意事项

1. **Brutal 算法特性**
   - 适用于高丢包率网络
   - 丢包时增加发送速率
   - 需要配置目标带宽

2. **切换策略选择**
   - Fresh: 适用于网络条件显著变化
   - Conservative: 适用于算法优化调整

3. **最小窗口**
   - 所有算法都遵守 4 * MTU 的最小窗口
   - 保证基本连接性能

## 详细报告

完整测试报告请参阅: [TEST_REPORT.md](TEST_REPORT.md)
