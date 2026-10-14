use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Protocol {
    Tcp,
    Udp,
    Icmp,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IpVersion {
    V4,
    V6,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortRange {
    pub start: u16,
    pub end: u16,
}

impl PortRange {
    pub fn new(start: u16, end: u16) -> Result<Self, String> {
        if start > end {
            return Err("start port must be <= end port".to_string());
        }
        Ok(PortRange { start, end })
    }

    pub fn contains(&self, port: u16) -> bool {
        port >= self.start && port <= self.end
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpRange {
    pub cidr: String,
    pub version: IpVersion,
}

impl IpRange {
    pub fn new(cidr: String) -> Result<Self, String> {
        if Self::is_valid_ipv4_cidr(&cidr) {
            Ok(IpRange {
                cidr,
                version: IpVersion::V4,
            })
        } else if Self::is_valid_ipv6_cidr(&cidr) {
            Ok(IpRange {
                cidr,
                version: IpVersion::V6,
            })
        } else {
            Err(format!("invalid CIDR: {}", cidr))
        }
    }

    fn is_valid_ipv4_cidr(cidr: &str) -> bool {
        let parts: Vec<&str> = cidr.split('/').collect();
        if parts.len() != 2 {
            return false;
        }

        let octets: Vec<&str> = parts[0].split('.').collect();
        if octets.len() != 4 {
            return false;
        }

        for octet in octets {
            if octet.parse::<u8>().is_err() {
                return false;
            }
        }

        if let Ok(prefix) = parts[1].parse::<u8>() {
            prefix <= 32
        } else {
            false
        }
    }

    fn is_valid_ipv6_cidr(cidr: &str) -> bool {
        let parts: Vec<&str> = cidr.split('/').collect();
        if parts.len() != 2 {
            return false;
        }

        // Check prefix length
        if let Ok(prefix) = parts[1].parse::<u8>() {
            if prefix > 128 {
                return false;
            }
        } else {
            return false;
        }

        // Validate IPv6 address
        let addr = parts[0];

        // Handle :: compression
        let double_colon_count = addr.matches("::").count();
        if double_colon_count > 1 {
            return false; // Only one :: allowed
        }

        // Split by : and validate
        let segments: Vec<&str> = if double_colon_count == 1 {
            // Handle compressed notation
            let parts_split: Vec<&str> = addr.split("::").collect();
            if parts_split.len() != 2 {
                return false;
            }

            let left: Vec<&str> = if parts_split[0].is_empty() {
                vec![]
            } else {
                parts_split[0].split(':').collect()
            };
            let right: Vec<&str> = if parts_split[1].is_empty() {
                vec![]
            } else {
                parts_split[1].split(':').collect()
            };

            let total_segments = left.len() + right.len();
            if total_segments > 7 {
                return false; // Max 7 segments with ::
            }

            // Combine for validation
            left.into_iter().chain(right).collect()
        } else {
            let segs: Vec<&str> = addr.split(':').collect();
            if segs.len() != 8 {
                return false; // Full IPv6 must have 8 segments
            }
            segs
        };

        // Validate each segment is valid hex and <= 0xFFFF
        for segment in segments {
            if segment.is_empty() && double_colon_count == 0 {
                return false;
            }
            if !segment.is_empty() {
                if segment.len() > 4 {
                    return false;
                }
                if u16::from_str_radix(segment, 16).is_err() {
                    return false;
                }
            }
        }

        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecurityGroupRule {
    pub protocol: Protocol,
    pub port_range: Option<PortRange>,
    pub source: IpRange,
    pub description: String,
}

impl SecurityGroupRule {
    pub fn render_iptables(&self) -> String {
        let mut rule = String::from("iptables -A INPUT");

        rule.push_str(&format!(" -s {}", self.source.cidr));

        match self.protocol {
            Protocol::Tcp => rule.push_str(" -p tcp"),
            Protocol::Udp => rule.push_str(" -p udp"),
            Protocol::Icmp => rule.push_str(" -p icmp"),
            Protocol::All => {}
        }

        if let Some(ref range) = self.port_range {
            if range.start == range.end {
                rule.push_str(&format!(" --dport {}", range.start));
            } else {
                rule.push_str(&format!(" --dport {}:{}", range.start, range.end));
            }
        }

        rule.push_str(" -j ACCEPT");

        if !self.description.is_empty() {
            rule.push_str(&format!(" -m comment --comment \"{}\"", self.description));
        }

        rule
    }

    pub fn render_nftables(&self) -> String {
        let mut rule = String::new();

        match self.protocol {
            Protocol::Tcp => rule.push_str("tcp "),
            Protocol::Udp => rule.push_str("udp "),
            Protocol::Icmp => rule.push_str("icmp "),
            Protocol::All => rule.push_str("ip "),
        }

        if let Some(ref range) = self.port_range {
            if range.start == range.end {
                rule.push_str(&format!("dport {} ", range.start));
            } else {
                rule.push_str(&format!("dport {}-{} ", range.start, range.end));
            }
        }

        rule.push_str(&format!("ip saddr {} accept", self.source.cidr));

        if !self.description.is_empty() {
            rule.push_str(&format!(" comment \"{}\"", self.description));
        }

        rule
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_port_range_valid() {
        let range = PortRange::new(80, 443).unwrap();
        assert!(range.contains(80));
        assert!(range.contains(443));
        assert!(range.contains(100));
        assert!(!range.contains(79));
        assert!(!range.contains(444));
    }

    #[test]
    fn test_port_range_invalid() {
        assert!(PortRange::new(443, 80).is_err());
    }

    #[test]
    fn test_ip_range_valid() {
        assert!(IpRange::new("10.0.0.0/8".to_string()).is_ok());
        assert!(IpRange::new("192.168.1.0/24".to_string()).is_ok());
        assert!(IpRange::new("0.0.0.0/0".to_string()).is_ok());
    }

    #[test]
    fn test_ip_range_invalid() {
        assert!(IpRange::new("256.0.0.0/8".to_string()).is_err());
        assert!(IpRange::new("10.0.0.0".to_string()).is_err());
        assert!(IpRange::new("10.0.0.0/33".to_string()).is_err());
        assert!(IpRange::new("not-an-ip/24".to_string()).is_err());
    }

    #[test]
    fn test_ipv6_range_valid() {
        // Full IPv6 addresses
        assert!(IpRange::new("2001:0db8:0000:0000:0000:0000:0000:0001/128".to_string()).is_ok());
        assert!(IpRange::new("2001:db8:0:0:0:0:0:1/128".to_string()).is_ok());

        // Compressed IPv6 addresses
        assert!(IpRange::new("2001:db8::1/128".to_string()).is_ok());
        assert!(IpRange::new("::1/128".to_string()).is_ok());
        assert!(IpRange::new("fe80::/64".to_string()).is_ok());
        assert!(IpRange::new("::/0".to_string()).is_ok());

        // Common prefixes
        assert!(IpRange::new("2001:db8::/32".to_string()).is_ok());
        assert!(IpRange::new("fd00::/8".to_string()).is_ok());
    }

    #[test]
    fn test_ipv6_range_invalid() {
        // Invalid prefix length
        assert!(IpRange::new("2001:db8::1/129".to_string()).is_err());

        // Multiple ::
        assert!(IpRange::new("2001::db8::1/64".to_string()).is_err());

        // Too many segments
        assert!(IpRange::new("1:2:3:4:5:6:7:8:9/64".to_string()).is_err());

        // Invalid hex
        assert!(IpRange::new("gggg::1/64".to_string()).is_err());

        // Segment too long
        assert!(IpRange::new("12345::1/64".to_string()).is_err());
    }

    #[test]
    fn test_ip_version_detection() {
        let ipv4 = IpRange::new("192.168.1.0/24".to_string()).unwrap();
        assert_eq!(ipv4.version, IpVersion::V4);

        let ipv6 = IpRange::new("2001:db8::1/64".to_string()).unwrap();
        assert_eq!(ipv6.version, IpVersion::V6);
    }

    #[test]
    fn test_iptables_rendering() {
        let rule = SecurityGroupRule {
            protocol: Protocol::Tcp,
            port_range: Some(PortRange::new(80, 80).unwrap()),
            source: IpRange::new("0.0.0.0/0".to_string()).unwrap(),
            description: "HTTP traffic".to_string(),
        };

        let rendered = rule.render_iptables();
        assert!(rendered.contains("iptables -A INPUT"));
        assert!(rendered.contains("-s 0.0.0.0/0"));
        assert!(rendered.contains("-p tcp"));
        assert!(rendered.contains("--dport 80"));
        assert!(rendered.contains("-j ACCEPT"));
        assert!(rendered.contains("HTTP traffic"));
    }

    #[test]
    fn test_nftables_rendering() {
        let rule = SecurityGroupRule {
            protocol: Protocol::Tcp,
            port_range: Some(PortRange::new(443, 443).unwrap()),
            source: IpRange::new("10.0.0.0/8".to_string()).unwrap(),
            description: "HTTPS traffic".to_string(),
        };

        let rendered = rule.render_nftables();
        assert!(rendered.contains("tcp"));
        assert!(rendered.contains("dport 443"));
        assert!(rendered.contains("ip saddr 10.0.0.0/8"));
        assert!(rendered.contains("accept"));
        assert!(rendered.contains("HTTPS traffic"));
    }
}
