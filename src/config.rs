use std::net::SocketAddr;
use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::BufReader;
use anyhow::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub target_port: u16,
    pub target_colos: Vec<String>,
    pub cidr_list: Vec<IpNet>,
    pub bind_addr: SocketAddr,
    pub web_addr: SocketAddr,
    pub max_warm_connections: usize,
    pub trace_url: String,
    pub asn_url: String,
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
            web_addr: "0.0.0.0:3000".parse().unwrap(),
            max_warm_connections: 100,
            trace_url: "http://engage.cloudflareclient.com/cdn-cgi/trace".to_string(),
            asn_url: "https://asn.0x01111110.com/13335?4".to_string(),
        }
    }
}

impl AppConfig {
    pub fn load_from_file(path: &str) -> Result<Self> {
        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let config = serde_json::from_reader(reader)?;
        Ok(config)
    }
}