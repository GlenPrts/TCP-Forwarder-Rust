use ipnet::IpNet;
use rand::prelude::*;
use std::net::IpAddr;

/// 在给定子网内生成一个随机 IP 地址
///
/// 对于 IPv4，会避开网络地址和广播地址（如果子网足够大）
/// 对于 IPv6，会避开网络地址（对于 /128 或 /127 子网除外）
pub fn generate_random_ip_in_subnet(subnet: &IpNet, rng: &mut impl Rng) -> IpAddr {
    match subnet {
        IpNet::V4(net) => {
            let start: u32 = net.network().into();
            let end: u32 = net.broadcast().into();

            // 对于 /31 和 /32 子网，直接使用整个范围
            // 对于更大的子网，避开网络地址和广播地址
            let (effective_start, effective_end) = if end - start >= 2 {
                (start + 1, end - 1)
            } else {
                (start, end)
            };

            let ip_u32 = if effective_start <= effective_end {
                rng.gen_range(effective_start..=effective_end)
            } else {
                effective_start
            };
            IpAddr::V4(std::net::Ipv4Addr::from(ip_u32))
        }
        IpNet::V6(net) => {
            let start: u128 = net.network().into();
            let end: u128 = net.broadcast().into();

            // 对于 /128 和 /127 子网，直接使用整个范围
            // 对于更大的子网，避开网络地址
            let (effective_start, effective_end) = if end - start >= 2 {
                (start + 1, end - 1)
            } else {
                (start, end)
            };

            let ip_u128 = if effective_start <= effective_end {
                rng.gen_range(effective_start..=effective_end)
            } else {
                effective_start
            };
            IpAddr::V6(std::net::Ipv6Addr::from(ip_u128))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_random_ip_in_subnet_v4() {
        let subnet: IpNet = "192.168.1.0/24".parse().unwrap();
        let mut rng = rand::thread_rng();

        for _ in 0..100 {
            let ip = generate_random_ip_in_subnet(&subnet, &mut rng);
            assert!(subnet.contains(&ip));

            // 确保不是网络地址或广播地址
            if let IpAddr::V4(v4) = ip {
                let ip_u32: u32 = v4.into();
                assert!(ip_u32 > 0xC0A80100); // > 192.168.1.0
                assert!(ip_u32 < 0xC0A801FF); // < 192.168.1.255
            }
        }
    }

    #[test]
    fn test_generate_random_ip_in_small_subnet() {
        let subnet: IpNet = "192.168.1.0/31".parse().unwrap();
        let mut rng = rand::thread_rng();

        let ip = generate_random_ip_in_subnet(&subnet, &mut rng);
        assert!(subnet.contains(&ip));
    }

    #[test]
    fn test_generate_random_ip_in_subnet_v6() {
        let subnet: IpNet = "2001:db8::/64".parse().unwrap();
        let mut rng = rand::thread_rng();

        for _ in 0..100 {
            let ip = generate_random_ip_in_subnet(&subnet, &mut rng);
            assert!(subnet.contains(&ip));

            // 确保生成的是 IPv6 地址
            assert!(ip.is_ipv6());
        }
    }

    #[test]
    fn test_generate_random_ip_in_small_subnet_v6() {
        // /127 子网：只有 2 个地址
        let subnet: IpNet = "2001:db8::0/127".parse().unwrap();
        let mut rng = rand::thread_rng();

        let ip = generate_random_ip_in_subnet(&subnet, &mut rng);
        assert!(subnet.contains(&ip));
        assert!(ip.is_ipv6());
    }

    #[test]
    fn test_generate_random_ip_in_single_host_v6() {
        // /128 子网：单个主机
        let subnet: IpNet = "2001:db8::1/128".parse().unwrap();
        let mut rng = rand::thread_rng();

        let ip = generate_random_ip_in_subnet(&subnet, &mut rng);
        assert!(subnet.contains(&ip));
        assert_eq!(ip, "2001:db8::1".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn test_generate_random_ip_in_single_host_v4() {
        // /32 子网：单个主机
        let subnet: IpNet = "192.168.1.100/32".parse().unwrap();
        let mut rng = rand::thread_rng();

        let ip = generate_random_ip_in_subnet(&subnet, &mut rng);
        assert!(subnet.contains(&ip));
        assert_eq!(ip, "192.168.1.100".parse::<IpAddr>().unwrap());
    }
}
