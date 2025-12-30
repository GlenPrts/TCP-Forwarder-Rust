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
    pub trace_url: String,
    pub asn_url: String,
    pub ip_store_file: String,
    
    // New selection strategy parameters
    pub selection_top_k_percent: f64,    // e.g., 0.1 for top 10%
    pub selection_random_n_subnets: usize, // e.g., 3 subnets
    pub selection_random_m_ips: usize,     // e.g., 2 IPs per subnet
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
            trace_url: "http://engage.cloudflareclient.com/cdn-cgi/trace".to_string(),
            asn_url: "https://asn.0x01111110.com/13335?4".to_string(),
            ip_store_file: "subnet_results.json".to_string(),
            selection_top_k_percent: 0.1,
            selection_random_n_subnets: 3,
            selection_random_m_ips: 2,
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