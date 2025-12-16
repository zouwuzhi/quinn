//! Tests for dynamic congestion controller switching

use std::sync::Arc;

use super::*;
use crate::congestion::{
    BbrConfig, BrutalConfig, ControllerFactory, CubicConfig, NewRenoConfig,
};
use crate::{CongestionSwitchStrategy, Dir, Event, StreamEvent};

#[test]
fn congestion_controller_switch_basic() {
    let _guard = subscribe();
    let mut pair = Pair::default();
    let (client_ch, _server_ch) = pair.connect();

    // 验证初始使用 Cubic
    assert_eq!(
        pair.client_conn_mut(client_ch)
            .congestion_controller_name(),
        "cubic"
    );

    // 切换到 NewReno (使用 Fresh 策略)
    pair.client
        .connections
        .get_mut(&client_ch)
        .unwrap()
        .set_congestion_controller(
            Arc::new(NewRenoConfig::default()),
            CongestionSwitchStrategy::Fresh,
            pair.time,
        );
    pair.drive();

    // 验证切换成功
    assert_eq!(
        pair.client_conn_mut(client_ch)
            .congestion_controller_name(),
        "new_reno"
    );

    // 发送一些数据验证工作正常
    let s = pair.client_streams(client_ch).open(Dir::Uni).unwrap();
    pair.client_send(client_ch, s).write(&[42; 1024]).unwrap();
    pair.drive();

    // 确保数据传输成功
    assert_eq!(pair.client_conn_mut(client_ch).stats().path.lost_packets, 0);
}

#[test]
fn congestion_controller_switch_with_state_transfer() {
    let _guard = subscribe();
    let mut pair = Pair::default();
    let (client_ch, _) = pair.connect();

    // 发送一些数据建立拥塞状态
    let s = pair.client_streams(client_ch).open(Dir::Uni).unwrap();
    for _ in 0..10 {
        pair.client_send(client_ch, s).write(&[42; 1024]).unwrap();
    }
    pair.drive();

    let cwnd_before = pair.client_conn_mut(client_ch).congestion_window();

    // 使用 Conservative 策略切换
    pair.client
        .connections
        .get_mut(&client_ch)
        .unwrap()
        .set_congestion_controller(
            Arc::new(NewRenoConfig::default()),
            CongestionSwitchStrategy::Conservative,
            pair.time,
        );

    let cwnd_after = pair.client_conn_mut(client_ch).congestion_window();

    // Conservative 策略应保留较小的窗口
    assert!(cwnd_after <= cwnd_before);

    // 验证切换成功
    assert_eq!(
        pair.client_conn_mut(client_ch)
            .congestion_controller_name(),
        "new_reno"
    );
}

#[test]
fn congestion_controller_multiple_switches() {
    let _guard = subscribe();
    let mut pair = Pair::default();
    let (client_ch, _) = pair.connect();

    // Cubic -> NewReno -> BBR -> Brutal -> Cubic
    let controllers: Vec<(&str, Arc<dyn ControllerFactory + Send + Sync>)> = vec![
        ("new_reno", Arc::new(NewRenoConfig::default())),
        ("bbr", Arc::new(BbrConfig::default())),
        ("brutal", Arc::new(BrutalConfig::new(100))),
        ("cubic", Arc::new(CubicConfig::default())),
    ];

    for (name, factory) in controllers {
        pair.client
            .connections
            .get_mut(&client_ch)
            .unwrap()
            .set_congestion_controller(factory, CongestionSwitchStrategy::Fresh, pair.time);
        pair.drive();
        assert_eq!(
            pair.client_conn_mut(client_ch)
                .congestion_controller_name(),
            name
        );

        // 发送一些数据验证工作正常
        let s = pair.client_streams(client_ch).open(Dir::Uni).unwrap();
        pair.client_send(client_ch, s).write(&[42; 100]).unwrap();
        pair.drive();
    }

    // 验证没有丢包
    assert_eq!(pair.client_conn_mut(client_ch).stats().path.lost_packets, 0);
}

#[test]
fn congestion_controller_switch_to_brutal() {
    let _guard = subscribe();
    let mut pair = Pair::default();
    let (client_ch, server_ch) = pair.connect();

    // 切换到 Brutal 算法
    pair.client
        .connections
        .get_mut(&client_ch)
        .unwrap()
        .set_congestion_controller(
            Arc::new(BrutalConfig::new(50)), // 50 Mbps
            CongestionSwitchStrategy::Fresh,
            pair.time,
        );
    pair.drive();

    // 验证切换成功
    assert_eq!(
        pair.client_conn_mut(client_ch)
            .congestion_controller_name(),
        "brutal"
    );

    // 发送大量数据测试 Brutal 算法
    let s = pair.client_streams(client_ch).open(Dir::Uni).unwrap();
    for _ in 0..20 {
        pair.client_send(client_ch, s).write(&[42; 1024]).unwrap();
    }
    pair.drive();

    // 验证服务器收到数据
    assert_matches!(
        pair.server_conn_mut(server_ch).poll(),
        Some(Event::Stream(StreamEvent::Opened { dir: Dir::Uni }))
    );
    assert_matches!(pair.server_streams(server_ch).accept(Dir::Uni), Some(stream) if stream == s);
}

#[test]
fn congestion_controller_switch_during_data_transfer() {
    let _guard = subscribe();
    let mut pair = Pair::default();
    let (client_ch, _) = pair.connect();

    let s = pair.client_streams(client_ch).open(Dir::Uni).unwrap();

    // 开始发送数据
    for _ in 0..5 {
        pair.client_send(client_ch, s).write(&[42; 1024]).unwrap();
    }
    pair.drive_client();

    // 在传输过程中切换拥塞控制算法
    pair.client
        .connections
        .get_mut(&client_ch)
        .unwrap()
        .set_congestion_controller(
            Arc::new(BbrConfig::default()),
            CongestionSwitchStrategy::Conservative,
            pair.time,
        );

    // 继续发送数据
    for _ in 0..5 {
        pair.client_send(client_ch, s).write(&[42; 1024]).unwrap();
    }
    pair.drive();

    // 验证切换成功且数据传输正常
    assert_eq!(
        pair.client_conn_mut(client_ch)
            .congestion_controller_name(),
        "bbr"
    );
    assert_eq!(pair.client_conn_mut(client_ch).stats().path.lost_packets, 0);
}
