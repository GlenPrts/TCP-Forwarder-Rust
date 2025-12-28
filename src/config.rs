use std::net::SocketAddr;
use ipnet::IpNet;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub target_port: u16,
    pub target_colos: Vec<String>,
    pub cidr_list: Vec<IpNet>,
    pub bind_addr: SocketAddr,
    pub max_warm_connections: usize,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            target_port: 443,
            target_colos: vec!["SJC".to_string(), "LAX".to_string()],
            cidr_list: vec![
                "104.16.0.0/12".parse().unwrap(),
                "172.64.0.0/13".parse().unwrap(),
            ],
            bind_addr: "0.0.0.0:8080".parse().unwrap(),
            max_warm_connections: 100,
        }
    }
}