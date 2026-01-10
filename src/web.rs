use crate::config::AppConfig;
use crate::model::SubnetQuality;
use crate::state::IpManager;
use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::get, Json, Router};
use serde::Serialize;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

/// 健康检查响应
#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
    subnet_count: usize,
}

/// 状态响应（带元数据）
#[derive(Serialize)]
struct StatusResponse {
    total_subnets: usize,
    subnets: Vec<SubnetQuality>,
}

/// 启动 Web 服务器
pub async fn start_web_server(
    config: Arc<AppConfig>,
    ip_manager: IpManager,
    cancel_token: CancellationToken,
) {
    let app = Router::new()
        .route("/health", get(health_check))
        .route("/status", get(get_status))
        .route("/api/v1/subnets", get(get_subnets_api))
        .with_state(ip_manager);

    info!("Web server listening on {}", config.web_addr);

    let listener = match tokio::net::TcpListener::bind(config.web_addr).await {
        Ok(l) => l,
        Err(e) => {
            error!("Failed to bind web server to {}: {}", config.web_addr, e);
            return;
        }
    };

    // 使用 axum 的优雅关闭
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            cancel_token.cancelled().await;
            info!("Web server received shutdown signal");
        })
        .await
        .unwrap_or_else(|e| {
            error!("Web server error: {}", e);
        });

    info!("Web server stopped");
}

/// 健康检查端点
async fn health_check(State(ip_manager): State<IpManager>) -> impl IntoResponse {
    let subnet_count = ip_manager.get_all_subnets().len();

    let response = HealthResponse {
        status: "healthy",
        version: env!("CARGO_PKG_VERSION"),
        subnet_count,
    };

    (StatusCode::OK, Json(response))
}

/// 获取状态（兼容旧 API）
async fn get_status(State(ip_manager): State<IpManager>) -> Json<Vec<SubnetQuality>> {
    let mut subnets: Vec<SubnetQuality> = ip_manager.get_all_subnets();
    // 按评分降序排序
    subnets.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Json(subnets)
}

/// 获取子网列表（新 API，带元数据）
async fn get_subnets_api(State(ip_manager): State<IpManager>) -> Json<StatusResponse> {
    let mut subnets: Vec<SubnetQuality> = ip_manager.get_all_subnets();
    subnets.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Json(StatusResponse {
        total_subnets: subnets.len(),
        subnets,
    })
}
