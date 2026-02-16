use crate::config::AppConfig;
use crate::model::SubnetQuality;
use crate::pool::ConnectionPool;
use crate::state::IpManager;
use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::get, Json, Router};
use serde::Serialize;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

#[derive(Clone)]
struct AppState {
    ip_manager: IpManager,
    pool: Option<Arc<ConnectionPool>>,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
    subnet_count: usize,
}

#[derive(Serialize)]
struct StatusResponse {
    total_subnets: usize,
    subnets: Vec<SubnetQuality>,
}

/// 启动 Web 服务器
///
/// # 参数
/// - `config`: 应用配置
/// - `ip_manager`: IP 管理器
/// - `pool`: 预连接池（可选）
/// - `cancel_token`: 取消令牌
pub async fn start_web_server(
    config: Arc<AppConfig>,
    ip_manager: IpManager,
    pool: Option<Arc<ConnectionPool>>,
    cancel_token: CancellationToken,
) {
    let state = AppState { ip_manager, pool };

    let app = Router::new()
        .route("/health", get(health_check))
        .route("/status", get(get_status))
        .route("/api/v1/subnets", get(get_subnets_api))
        .route("/api/v1/pool", get(get_pool_stats))
        .with_state(state);

    info!("Web server listening on {}", config.web_addr);

    let listener = match tokio::net::TcpListener::bind(config.web_addr).await {
        Ok(l) => l,
        Err(e) => {
            error!("Failed to bind web server to {}: {}", config.web_addr, e);
            return;
        }
    };

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

async fn health_check(State(state): State<AppState>) -> impl IntoResponse {
    let response = HealthResponse {
        status: "healthy",
        version: env!("CARGO_PKG_VERSION"),
        subnet_count: state.ip_manager.subnet_count(),
    };
    (StatusCode::OK, Json(response))
}

async fn get_status(State(state): State<AppState>) -> Json<Vec<SubnetQuality>> {
    let mut subnets: Vec<SubnetQuality> = state.ip_manager.get_all_subnets();
    subnets.sort_by(|a, b| b.score.total_cmp(&a.score));
    Json(subnets)
}

async fn get_subnets_api(State(state): State<AppState>) -> Json<StatusResponse> {
    let mut subnets: Vec<SubnetQuality> = state.ip_manager.get_all_subnets();
    subnets.sort_by(|a, b| b.score.total_cmp(&a.score));

    Json(StatusResponse {
        total_subnets: subnets.len(),
        subnets,
    })
}

/// 连接池统计端点
///
/// # 返回值
/// - 启用时: 200 + 统计 JSON
/// - 未启用时: 404
async fn get_pool_stats(State(state): State<AppState>) -> impl IntoResponse {
    match state.pool {
        Some(ref p) => {
            let snapshot = p.snapshot().await;
            (StatusCode::OK, Json(Some(snapshot)))
        }
        None => (StatusCode::NOT_FOUND, Json(None)),
    }
}
