//! Tests for Brutal congestion control algorithm

use std::sync::Arc;

use super::*;
use crate::congestion::{BrutalConfig, ControllerFactory};
use crate::{Dir, Instant};

#[test]
fn brutal_basic_functionality() {
    let _guard = subscribe();
    let config = Arc::new(BrutalConfig::new(100)); // 100 Mbps
    let brutal = config.build(Instant::now(), 1200);

    assert_eq!(brutal.name(), "brutal");
    assert!(brutal.window() >= 4 * 1200); // 至少 4 * MTU
}

#[test]
fn brutal_respects_minimum_window() {
    let _guard = subscribe();
    let config = Arc::new(BrutalConfig::new(1)); // 非常低的带宽
    let brutal = config.build(Instant::now(), 1200);

    let cwnd = brutal.window();
    // 应该至少是 4 * MTU
    assert!(cwnd >= 4 * 1200, "CWND = {}", cwnd);
}

#[test]
fn brutal_mtu_update() {
    let _guard = subscribe();
    let config = Arc::new(BrutalConfig::new(100));
    let mut brutal = config.build(Instant::now(), 1200);

    let cwnd_before = brutal.window();

    // 更新 MTU
    brutal.on_mtu_update(1500);

    let cwnd_after = brutal.window();

    // MTU 增加,最小窗口也应该增加
    assert!(cwnd_after >= cwnd_before);
}

#[test]
fn brutal_pacing_rate() {
    let _guard = subscribe();
    let config = Arc::new(BrutalConfig::new(100)); // 100 Mbps
    let brutal = config.build(Instant::now(), 1200);

    let metrics = brutal.metrics();

    // Pacing rate 应该存在
    assert!(metrics.pacing_rate.is_some());

    // 在完美传输下,pacing rate 应该约为 100 Mbps = 100_000_000 bps
    let pacing_rate_bps = metrics.pacing_rate.unwrap();
    assert!(
        pacing_rate_bps >= 90_000_000 && pacing_rate_bps <= 110_000_000,
        "Pacing rate = {} bps",
        pacing_rate_bps
    );
}

#[test]
fn brutal_state_transfer() {
    let _guard = subscribe();
    let config = Arc::new(BrutalConfig::new(100));
    let brutal1 = config.clone().build(Instant::now(), 1200);

    // 获取可转移状态
    let state = brutal1.transferable_state();

    // 创建新实例并应用状态
    let mut brutal2 = config.build(Instant::now(), 1200);
    let applied = brutal2.apply_transferred_state(&state);

    // Brutal 应该支持状态转移
    assert!(applied);
}

#[test]
fn brutal_integration_with_connection() {
    let _guard = subscribe();
    let mut pair = Pair::default();
    let (client_ch, server_ch) = pair.connect();

    // 切换到 Brutal
    pair.client
        .connections
        .get_mut(&client_ch)
        .unwrap()
        .set_congestion_controller(
            Arc::new(BrutalConfig::new(50)),
            crate::CongestionSwitchStrategy::Fresh,
            pair.time,
        );
    pair.drive();

    // 验证使用 Brutal
    assert_eq!(
        pair.client_conn_mut(client_ch)
            .congestion_controller_name(),
        "brutal"
    );

    // 发送数据
    let s = pair.client_streams(client_ch).open(Dir::Uni).unwrap();
    for _ in 0..50 {
        pair.client_send(client_ch, s).write(&[42; 1024]).unwrap();
    }
    pair.client_send(client_ch, s).finish().unwrap();
    pair.drive();

    // 验证数据传输成功
    assert_matches!(
        pair.server_conn_mut(server_ch).poll(),
        Some(crate::Event::Stream(crate::StreamEvent::Opened { dir: Dir::Uni }))
    );
    assert_matches!(pair.server_streams(server_ch).accept(Dir::Uni), Some(stream) if stream == s);

    let mut recv = pair.server_recv(server_ch, s);
    let mut chunks = recv.read(false).unwrap();
    let mut total_bytes = 0;
    loop {
        match chunks.next(usize::MAX) {
            Ok(Some(chunk)) => {
                total_bytes += chunk.bytes.len();
            }
            Ok(None) => break,
            Err(crate::ReadError::Blocked) => break,
            Err(e) => panic!("Read error: {:?}", e),
        }
    }
    let _ = chunks.finalize();

    // 验证接收到所有数据
    assert_eq!(total_bytes, 50 * 1024);
}

#[test]
fn brutal_with_different_target_bandwidth() {
    let _guard = subscribe();
    let mut pair = Pair::default();
    let (client_ch, _) = pair.connect();

    // 测试不同的目标带宽设置
    let bandwidths = vec![10, 50, 100, 200]; // Mbps

    for bw in bandwidths {
        pair.client
            .connections
            .get_mut(&client_ch)
            .unwrap()
            .set_congestion_controller(
                Arc::new(BrutalConfig::new(bw)),
                crate::CongestionSwitchStrategy::Fresh,
                pair.time,
            );
        pair.drive();

        assert_eq!(
            pair.client_conn_mut(client_ch)
                .congestion_controller_name(),
            "brutal"
        );

        // 发送一些数据验证工作正常
        let s = pair.client_streams(client_ch).open(Dir::Uni).unwrap();
        pair.client_send(client_ch, s).write(&[42; 1024]).unwrap();
        pair.drive();
    }
}

#[test]
fn brutal_config_builder() {
    let _guard = subscribe();

    // 测试配置构建器
    let mut config = BrutalConfig::new(100);
    config.target_mbps(200).min_ack_rate(0.5);

    let brutal = Arc::new(config).build(Instant::now(), 1200);

    // 验证基本功能
    assert_eq!(brutal.name(), "brutal");
    assert!(brutal.window() > 0);
}

#[test]
fn brutal_clone_box() {
    let _guard = subscribe();
    let config = Arc::new(BrutalConfig::new(100));
    let brutal = config.build(Instant::now(), 1200);

    // 测试 clone_box 功能
    let cloned = brutal.clone_box();

    assert_eq!(cloned.name(), "brutal");
    assert_eq!(cloned.window(), brutal.window());
}
