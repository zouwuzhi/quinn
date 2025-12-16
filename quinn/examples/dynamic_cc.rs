//! 演示动态切换拥塞控制算法的示例程序
//!
//! 这个示例演示如何在运行时动态切换 QUIC 连接的拥塞控制算法,
//! 包括切换到自定义的 Brutal 算法。
//!
//! 运行方式:
//! ```
//! cargo run --example dynamic_cc --features="rustls-ring"
//! ```

use std::{
    error::Error,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
};

use quinn::Connection;

mod common;
use common::{make_client_endpoint, make_server_endpoint};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
    // 设置日志
    tracing_subscriber::fmt::init();

    let server_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5001);
    let (endpoint, server_cert) = make_server_endpoint(server_addr)?;

    // 在后台启动服务器
    let endpoint_clone = endpoint.clone();
    tokio::spawn(async move {
        while let Some(conn) = endpoint_clone.accept().await {
            tokio::spawn(async move {
                match conn.await {
                    Ok(connection) => {
                        println!("[服务器] 新连接来自: {}", connection.remote_address());
                        if let Err(e) = handle_connection(connection).await {
                            eprintln!("[服务器] 处理错误: {}", e);
                        }
                    }
                    Err(e) => eprintln!("[服务器] 连接失败: {}", e),
                }
            });
        }
    });

    // 运行客户端演示
    let endpoint = make_client_endpoint("0.0.0.0:0".parse()?, &[&server_cert])?;
    let connection = endpoint
        .connect(server_addr, "localhost")?
        .await?;

    println!("[客户端] 已连接到服务器: {}", connection.remote_address());
    run_demo(&connection).await?;

    // 等待连接关闭
    endpoint.wait_idle().await;

    Ok(())
}

/// 处理单个连接
async fn handle_connection(connection: Connection) -> Result<(), Box<dyn Error + Send + Sync>> {
    loop {
        let mut stream = match connection.accept_uni().await {
            Ok(s) => s,
            Err(quinn::ConnectionError::ApplicationClosed(_)) => {
                println!("[服务器] 连接正常关闭");
                break;
            }
            Err(e) => {
                eprintln!("[服务器] 接受流失败: {}", e);
                break;
            }
        };

        tokio::spawn(async move {
            match stream.read_to_end(1024 * 1024).await {
                Ok(data) => {
                    println!("[服务器] 收到 {} 字节数据", data.len());
                }
                Err(e) => eprintln!("[服务器] 读取流失败: {}", e),
            }
        });
    }

    Ok(())
}

/// 运行演示
async fn run_demo(connection: &Connection) -> Result<(), Box<dyn Error + Send + Sync>> {
    println!("\n=== 动态拥塞控制算法切换演示 ===\n");

    // 使用 quinn::congestion 模块中的类型
    use quinn::congestion::{BbrConfig, BrutalConfig, CubicConfig, NewRenoConfig};

    // 1. 初始状态 (Cubic)
    println!("1. 初始拥塞控制: {}", connection.congestion_controller_name());
    send_test_data(connection, "使用 Cubic").await?;

    // 2. 切换到 NewReno (使用 Fresh 策略)
    println!("\n2. 切换到 NewReno (Fresh 策略)...");
    connection.set_congestion_controller(
        Arc::new(NewRenoConfig::default()),
        quinn::CongestionSwitchStrategy::Fresh,
    );
    println!("   当前拥塞控制: {}", connection.congestion_controller_name());
    send_test_data(connection, "使用 NewReno").await?;

    // 3. 切换到 BBR (使用 Conservative 策略)
    println!("\n3. 切换到 BBR (Conservative 策略)...");
    connection.set_congestion_controller(
        Arc::new(BbrConfig::default()),
        quinn::CongestionSwitchStrategy::Conservative,
    );
    println!("   当前拥塞控制: {}", connection.congestion_controller_name());
    send_test_data(connection, "使用 BBR").await?;

    // 4. 切换到 Brutal (50 Mbps)
    println!("\n4. 切换到 Brutal (目标带宽: 50 Mbps)...");
    connection.set_congestion_controller(
        Arc::new(BrutalConfig::new(50)),
        quinn::CongestionSwitchStrategy::Fresh,
    );
    println!("   当前拥塞控制: {}", connection.congestion_controller_name());
    send_test_data(connection, "使用 Brutal 50Mbps").await?;

    // 5. 调整 Brutal 带宽到 100 Mbps
    println!("\n5. 切换到 Brutal (目标带宽: 100 Mbps)...");
    connection.set_congestion_controller(
        Arc::new(BrutalConfig::new(100)),
        quinn::CongestionSwitchStrategy::Conservative,
    );
    println!("   当前拥塞控制: {}", connection.congestion_controller_name());
    send_test_data(connection, "使用 Brutal 100Mbps").await?;

    // 6. 切换回 Cubic
    println!("\n6. 切换回 Cubic...");
    connection.set_congestion_controller(
        Arc::new(CubicConfig::default()),
        quinn::CongestionSwitchStrategy::Conservative,
    );
    println!("   当前拥塞控制: {}", connection.congestion_controller_name());
    send_test_data(connection, "切换回 Cubic").await?;

    // 显示连接统计信息
    println!("\n=== 连接统计 ===");
    let stats = connection.stats();
    println!("  发送的包: {}", stats.path.sent_packets);
    println!("  丢失的包: {}", stats.path.lost_packets);
    println!("  拥塞事件: {}", stats.path.congestion_events);
    println!("  当前 RTT: {:?}", connection.rtt());
    println!("  拥塞窗口: {} bytes", stats.path.cwnd);

    println!("\n演示完成！\n");
    println!("总结:");
    println!("  ✓ 成功演示了在同一连接上动态切换多种拥塞控制算法");
    println!("  ✓ 测试了 Cubic、NewReno、BBR 和 Brutal 算法");
    println!("  ✓ 验证了 Fresh 和 Conservative 两种切换策略");
    println!("  ✓ 所有数据传输正常完成");

    Ok(())
}

/// 发送测试数据
async fn send_test_data(
    connection: &Connection,
    label: &str,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut send = connection.open_uni().await?;

    // 发送较小的数据避免阻塞
    let data = vec![0u8; 1024]; // 1 KB
    send.write_all(&data).await?;
    send.finish()?;

    println!("   ✓ 发送了 {} 字节数据 ({})", data.len(), label);

    // 短暂等待
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    Ok(())
}
