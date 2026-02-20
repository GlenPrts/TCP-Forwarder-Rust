use futures::stream::{FuturesUnordered, StreamExt};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::time::timeout;

static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

struct TestStream {
    id: usize,
    ip: IpAddr,
}

impl Drop for TestStream {
    fn drop(&mut self) {
        DROP_COUNT.fetch_add(1, Ordering::SeqCst);
        println!("[DROP] TestStream#{} for {}", self.id, self.ip);
    }
}

#[tokio::test]
async fn test_connection_cleanup_timing() {
    println!("\n=== Connection Cleanup Timing Test ===\n");

    DROP_COUNT.store(0, Ordering::SeqCst);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            if let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    drop(stream);
                });
            }
        }
    });

    let candidate_ips: Vec<IpAddr> = (0..30)
        .map(|i| {
            if i == 0 {
                addr.ip()
            } else {
                format!("192.168.{}.{}", i / 256, i % 256).parse().unwrap()
            }
        })
        .collect();

    let start = Instant::now();
    let mut tasks = FuturesUnordered::new();

    for (idx, &ip) in candidate_ips.iter().enumerate() {
        let target_addr = SocketAddr::new(ip, addr.port());
        tasks.push(async move {
            let _guard = TestStream { id: idx, ip };
            let result = timeout(
                Duration::from_millis(500),
                TcpStream::connect(target_addr),
            )
            .await;
            match result {
                Ok(Ok(_)) => {
                    println!("[CONNECT] IP#{} {} SUCCESS", idx, ip);
                    Some(true)
                }
                _ => {
                    println!("[FAIL] IP#{} {} failed", idx, ip);
                    None
                }
            }
        });
    }

    let wait_start = Instant::now();
    let result = tokio::select! {
        result = async {
            while let Some(res) = tasks.next().await {
                if res.is_some() {
                    return res;
                }
            }
            None
        } => result,
        _ = tokio::time::sleep(Duration::from_millis(1000)) => None,
    };

    let wait_time = wait_start.elapsed();
    let total_time = start.elapsed();

    println!("\n[RESULTS]");
    println!(
        "Found connection after: {:?}ms",
        (total_time - wait_time).as_millis()
    );
    println!("Task processing time: {:?}ms", wait_time.as_millis());
    println!("Total until return: {:?}ms", total_time.as_millis());

    let drop_start = Instant::now();
    drop(tasks);
    let drop_time = drop_start.elapsed();

    println!("\n[DROP STATS]");
    println!(
        "Drop time: {}us ({}ms)",
        drop_time.as_micros(),
        drop_time.as_millis()
    );
    println!("Total drops: {}", DROP_COUNT.load(Ordering::SeqCst));

    assert!(result.is_some());
    assert!(drop_time < Duration::from_millis(5));
    assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 30);
}
