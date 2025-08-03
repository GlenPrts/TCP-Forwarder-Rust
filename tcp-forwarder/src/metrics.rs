use anyhow::Result;
use axum::{response::Response, routing::get, Router};
use metrics::{counter, gauge, histogram, describe_counter, describe_gauge, describe_histogram};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use std::net::SocketAddr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;
use tower::ServiceBuilder;
use tower_http::cors::CorsLayer;
use tracing::info;
use tokio::sync::RwLock;

/// 全局指标管理器实例
pub static METRICS: MetricsManager = MetricsManager::new();

/// 指标管理器
pub struct MetricsManager {
    handle: OnceLock<PrometheusHandle>,
    is_initialized: RwLock<bool>,
    active_connections: AtomicI64,
}

impl MetricsManager {
    /// 创建新的指标管理器
    pub const fn new() -> Self {
        Self {
            handle: OnceLock::new(),
            is_initialized: RwLock::const_new(false),
            active_connections: AtomicI64::new(0),
        }
    }

    /// 初始化指标系统
    pub async fn initialize(&self) -> Result<()> {
        let mut initialized = self.is_initialized.write().await;
        if *initialized {
            return Ok(());
        }

        // 创建Prometheus导出器
        let recorder = PrometheusBuilder::new().build_recorder();

        // 获取handle
        let handle = recorder.handle();

        // 安装指标收集器
        metrics::set_boxed_recorder(Box::new(recorder))
            .map_err(|e| anyhow::anyhow!("安装指标收集器失败: {}", e))?;

        // 注册指标描述
        register_metrics();

        // 存储handle
        self.handle.set(handle).map_err(|_| anyhow::anyhow!("指标系统已初始化"))?;
        *initialized = true;

        Ok(())
    }

    /// 启动指标HTTP服务器
    pub async fn start_server(&self, listen_addr: SocketAddr, path: String) -> Result<()> {
        let handle = self.handle.get()
            .ok_or_else(|| anyhow::anyhow!("指标系统未初始化"))?
            .clone();
        
        // 创建路由
        let app = Router::new()
            .route(&path, get(move || async move {
                metrics_handler(handle).await
            }))
            .layer(
                ServiceBuilder::new()
                    .layer(CorsLayer::permissive())
            );

        info!("指标服务器启动在 http://{}{}", listen_addr, path);
        
        // 绑定并启动服务器
        let listener = tokio::net::TcpListener::bind(listen_addr)
            .await
            .map_err(|e| anyhow::anyhow!("无法绑定指标服务器地址: {}", e))?;
            
        axum::serve(listener, app)
            .await
            .map_err(|e| anyhow::anyhow!("指标服务器运行错误: {}", e))?;

        Ok(())
    }

    // =================== 指标记录方法 ===================

    /// 记录新连接
    pub fn record_new_connection(&self) {
        counter!("tcp_forwarder_connections_total", 1);
        let current = self.active_connections.fetch_add(1, Ordering::Relaxed) + 1;
        gauge!("tcp_forwarder_active_connections", current as f64);
    }

    /// 记录连接结束
    pub fn record_connection_closed(&self) {
        let current = self.active_connections.fetch_sub(1, Ordering::Relaxed) - 1;
        gauge!("tcp_forwarder_active_connections", current.max(0) as f64);
    }

    /// 记录连接成功
    pub fn record_connection_success(&self) {
        counter!("tcp_forwarder_connections_successful", 1);
    }

    /// 记录连接失败
    pub fn record_connection_failed(&self) {
        counter!("tcp_forwarder_connections_failed", 1);
        counter!("tcp_forwarder_errors_total", 1);
    }

    /// 记录连接建立耗时
    pub fn record_connection_duration(&self, duration: Duration) {
        histogram!("tcp_forwarder_connection_duration_seconds", duration.as_secs_f64());
    }

    /// 记录连接池命中
    pub fn record_pool_hit(&self, _ip: &str) {
        counter!("tcp_forwarder_pool_hits", 1);
    }

    /// 记录连接池未命中
    pub fn record_pool_miss(&self, _ip: &str) {
        counter!("tcp_forwarder_pool_misses", 1);
    }

    /// 记录连接池中的连接数
    pub fn record_pool_size(&self, _ip: &str, _size: f64) {
        // 暂时简化实现，不记录具体IP的池大小
        // gauge!("tcp_forwarder_pool_connections", size);
    }

    /// 记录连接池创建连接
    pub fn record_pool_connection_created(&self) {
        counter!("tcp_forwarder_pool_connections_created", 1);
    }

    /// 记录连接池创建连接失败
    pub fn record_pool_connection_failed(&self) {
        counter!("tcp_forwarder_pool_connections_failed", 1);
    }

    /// 记录连接池连接重用
    pub fn record_pool_connection_reused(&self) {
        counter!("tcp_forwarder_pool_connections_reused", 1);
    }

    /// 记录连接池连接关闭
    pub fn record_pool_connection_closed(&self) {
        counter!("tcp_forwarder_pool_connections_closed", 1);
    }

    /// 记录连接池健康检查
    pub fn record_pool_health_check(&self, is_healthy: bool) {
        if is_healthy {
            counter!("tcp_forwarder_pool_health_checks_passed", 1);
        } else {
            counter!("tcp_forwarder_pool_health_checks_failed", 1);
        }
    }

    /// 记录活跃IP数量
    pub fn record_active_ips(&self, count: f64) {
        gauge!("tcp_forwarder_active_ips", count);
    }

    /// 记录IP评分
    pub fn record_ip_score(&self, _ip: &str, _score: f64) {
        // 暂时简化实现，不记录具体IP的评分
        // gauge!("tcp_forwarder_ip_score", score);
    }

    /// 记录IP探测
    pub fn record_ip_probe(&self, _ip: &str) {
        counter!("tcp_forwarder_ip_probes_total", 1);
    }

    /// 记录IP探测成功
    pub fn record_ip_probe_success(&self, _ip: &str) {
        counter!("tcp_forwarder_ip_probes_successful", 1);
    }

    /// 记录IP探测失败
    pub fn record_ip_probe_failed(&self, _ip: &str) {
        counter!("tcp_forwarder_ip_probes_failed", 1);
    }

    /// 记录探测耗时
    pub fn record_probe_duration(&self, duration: Duration) {
        histogram!("tcp_forwarder_probe_duration_seconds", duration.as_secs_f64());
    }

    /// 记录传输字节数
    pub fn record_transfer_bytes(&self, bytes: u64) {
        histogram!("tcp_forwarder_transfer_bytes", bytes as f64);
    }

    /// 记录运行时间
    pub fn record_uptime(&self, uptime_seconds: f64) {
        gauge!("tcp_forwarder_uptime_seconds", uptime_seconds);
    }

    /// 记录错误
    pub fn record_error(&self) {
        counter!("tcp_forwarder_errors_total", 1);
    }
}

/// 指标处理器
async fn metrics_handler(handle: PrometheusHandle) -> Response<String> {
    let metrics = handle.render();
    Response::builder()
        .status(200)
        .header("content-type", "text/plain; version=0.0.4")
        .body(metrics)
        .unwrap()
}

/// 注册所有指标的描述
fn register_metrics() {
    // 连接相关指标
    describe_counter!("tcp_forwarder_connections_total", "总连接数");
    describe_counter!("tcp_forwarder_connections_successful", "成功连接数");
    describe_counter!("tcp_forwarder_connections_failed", "失败连接数");
    describe_gauge!("tcp_forwarder_active_connections", "当前活跃连接数");
    
    // 连接池相关指标
    describe_gauge!("tcp_forwarder_pool_connections", "连接池中的连接数");
    describe_counter!("tcp_forwarder_pool_hits", "连接池命中次数");
    describe_counter!("tcp_forwarder_pool_misses", "连接池未命中次数");
    describe_counter!("tcp_forwarder_pool_connections_created", "连接池创建连接数");
    describe_counter!("tcp_forwarder_pool_connections_failed", "连接池创建连接失败数");
    describe_counter!("tcp_forwarder_pool_connections_reused", "连接池重用连接数");
    describe_counter!("tcp_forwarder_pool_connections_closed", "连接池关闭连接数");
    describe_counter!("tcp_forwarder_pool_health_checks_passed", "连接池健康检查通过数");
    describe_counter!("tcp_forwarder_pool_health_checks_failed", "连接池健康检查失败数");
    
    // IP和评分相关指标
    describe_gauge!("tcp_forwarder_active_ips", "活跃IP数量");
    describe_gauge!("tcp_forwarder_ip_score", "IP评分");
    describe_counter!("tcp_forwarder_ip_probes_total", "IP探测总数");
    describe_counter!("tcp_forwarder_ip_probes_successful", "IP探测成功数");
    describe_counter!("tcp_forwarder_ip_probes_failed", "IP探测失败数");
    
    // 延迟相关指标
    describe_histogram!("tcp_forwarder_connection_duration_seconds", "连接建立耗时");
    describe_histogram!("tcp_forwarder_probe_duration_seconds", "探测耗时");
    describe_histogram!("tcp_forwarder_transfer_bytes", "传输字节数");
    
    // 系统指标
    describe_gauge!("tcp_forwarder_uptime_seconds", "运行时间（秒）");
    describe_counter!("tcp_forwarder_errors_total", "错误总数");
}
