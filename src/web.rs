use crate::config::AppConfig;
use crate::model::IpQuality;
use crate::state::IpManager;
use axum::{extract::State, routing::get, Json, Router};
use std::sync::Arc;
use tracing::info;

pub async fn start_web_server(config: Arc<AppConfig>, ip_manager: IpManager) {
    let app = Router::new()
        .route("/status", get(get_status))
        .with_state(ip_manager);

    info!("Web server listening on {}", config.web_addr);

    let listener = tokio::net::TcpListener::bind(config.web_addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn get_status(State(ip_manager): State<IpManager>) -> Json<Vec<IpQuality>> {
    let mut ips: Vec<IpQuality> = ip_manager.get_all_ips();
    // Sort by score (descending)
    ips.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    Json(ips)
}