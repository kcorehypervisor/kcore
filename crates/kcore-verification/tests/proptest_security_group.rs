// Proptest coverage for security group rule rendering
// Objective 4: Extended proptest coverage

use kcore_verification::security_group::*;
use proptest::prelude::*;

fn valid_octet() -> impl Strategy<Value = u8> {
    0u8..=255u8
}

fn valid_cidr() -> impl Strategy<Value = String> {
    (
        valid_octet(),
        valid_octet(),
        valid_octet(),
        valid_octet(),
        0u8..=32u8,
    )
        .prop_map(|(a, b, c, d, prefix)| format!("{}.{}.{}.{}/{}", a, b, c, d, prefix))
}

fn valid_ipv6_segment() -> impl Strategy<Value = String> {
    "[0-9a-fA-F]{1,4}"
}

fn valid_ipv6_cidr() -> impl Strategy<Value = String> {
    prop_oneof![
        // Full IPv6 address
        (
            valid_ipv6_segment(),
            valid_ipv6_segment(),
            valid_ipv6_segment(),
            valid_ipv6_segment(),
            valid_ipv6_segment(),
            valid_ipv6_segment(),
            valid_ipv6_segment(),
            valid_ipv6_segment(),
            0u8..=128u8
        )
            .prop_map(|(s1, s2, s3, s4, s5, s6, s7, s8, prefix)| format!(
                "{}:{}:{}:{}:{}:{}:{}:{}/{}",
                s1, s2, s3, s4, s5, s6, s7, s8, prefix
            )),
        // Compressed IPv6 (::)
        (valid_ipv6_segment(), valid_ipv6_segment(), 0u8..=128u8)
            .prop_map(|(s1, s2, prefix)| format!("{}:{}::/{}", s1, s2, prefix)),
        // Common patterns
        Just("2001:db8::/32".to_string()),
        Just("::1/128".to_string()),
        Just("::/0".to_string()),
        Just("fe80::/64".to_string()),
    ]
}

fn any_cidr() -> impl Strategy<Value = String> {
    prop_oneof![
        valid_cidr().prop_map(|s| s),
        valid_ipv6_cidr().prop_map(|s| s),
    ]
}

fn protocol_strategy() -> impl Strategy<Value = Protocol> {
    prop_oneof![
        Just(Protocol::Tcp),
        Just(Protocol::Udp),
        Just(Protocol::Icmp),
        Just(Protocol::All),
    ]
}

fn port_range_strategy() -> impl Strategy<Value = PortRange> {
    (1u16..=65535u16)
        .prop_flat_map(|start| (start..=65535u16).prop_map(move |end| PortRange { start, end }))
}

fn security_rule_strategy() -> impl Strategy<Value = SecurityGroupRule> {
    (
        protocol_strategy(),
        prop::option::of(port_range_strategy()),
        any_cidr(),
        "[a-zA-Z0-9 _-]{0,100}",
    )
        .prop_filter_map("valid CIDR", |(protocol, port_range, cidr, description)| {
            IpRange::new(cidr).ok().map(|source| SecurityGroupRule {
                protocol,
                port_range,
                source,
                description: description.to_string(),
            })
        })
}

proptest! {
    #[test]
    fn test_port_range_invariant(start in 1u16..=65535u16, end in 1u16..=65535u16) {
        let result = PortRange::new(start, end);
        if start <= end {
            prop_assert!(result.is_ok());
            let range = result.unwrap();
            prop_assert!(range.contains(start));
            prop_assert!(range.contains(end));
        } else {
            prop_assert!(result.is_err());
        }
    }

    #[test]
    fn test_port_range_boundaries(range in port_range_strategy(), port in 1u16..=65535u16) {
        let expected = port >= range.start && port <= range.end;
        prop_assert_eq!(range.contains(port), expected);
    }

    #[test]
    fn test_ip_range_creation(cidr in valid_cidr()) {
        let result = IpRange::new(cidr.clone());
        prop_assert!(result.is_ok(), "Valid CIDR {} should be accepted", cidr);

        if let Ok(range) = result {
            prop_assert_eq!(range.version, IpVersion::V4);
        }
    }

    #[test]
    fn test_ipv6_range_creation(cidr in valid_ipv6_cidr()) {
        let result = IpRange::new(cidr.clone());
        prop_assert!(result.is_ok(), "Valid IPv6 CIDR {} should be accepted", cidr);

        if let Ok(range) = result {
            prop_assert_eq!(range.version, IpVersion::V6);
        }
    }

    #[test]
    fn test_any_cidr_creation(cidr in any_cidr()) {
        let result = IpRange::new(cidr.clone());
        prop_assert!(result.is_ok(), "Valid CIDR {} should be accepted", cidr);
    }

    #[test]
    fn test_iptables_render_no_panic(rule in security_rule_strategy()) {
        let rendered = rule.render_iptables();
        prop_assert!(rendered.contains("iptables -A INPUT"));
        prop_assert!(rendered.contains("-s"));
        prop_assert!(rendered.contains(&rule.source.cidr));
    }

    #[test]
    fn test_nftables_render_no_panic(rule in security_rule_strategy()) {
        let rendered = rule.render_nftables();
        prop_assert!(rendered.contains(&rule.source.cidr));
        prop_assert!(rendered.contains("accept"));
    }

    #[test]
    fn test_iptables_protocol_rendering(protocol in protocol_strategy(), cidr in any_cidr()) {
        if let Ok(source) = IpRange::new(cidr) {
            let rule = SecurityGroupRule {
                protocol: protocol.clone(),
                port_range: None,
                source,
                description: String::new(),
            };

            let rendered = rule.render_iptables();

            match protocol {
                Protocol::Tcp => prop_assert!(rendered.contains("-p tcp")),
                Protocol::Udp => prop_assert!(rendered.contains("-p udp")),
                Protocol::Icmp => prop_assert!(rendered.contains("-p icmp")),
                Protocol::All => prop_assert!(!rendered.contains("-p ")),
            }
        }
    }

    #[test]
    fn test_port_range_rendering(range in port_range_strategy(), cidr in any_cidr()) {
        if let Ok(source) = IpRange::new(cidr) {
            let rule = SecurityGroupRule {
                protocol: Protocol::Tcp,
                port_range: Some(range.clone()),
                source,
                description: String::new(),
            };

            let iptables = rule.render_iptables();

            if range.start == range.end {
                let expected = format!("--dport {}", range.start);
                prop_assert!(iptables.contains(&expected));
            } else {
                let expected = format!("--dport {}:{}", range.start, range.end);
                prop_assert!(iptables.contains(&expected));
            }
        }
    }

    #[test]
    fn test_render_determinism(rule in security_rule_strategy()) {
        let render1 = rule.render_iptables();
        let render2 = rule.render_iptables();
        prop_assert_eq!(render1, render2);

        let render3 = rule.render_nftables();
        let render4 = rule.render_nftables();
        prop_assert_eq!(render3, render4);
    }

    #[test]
    fn test_description_in_rendered_output(
        rule in security_rule_strategy().prop_filter("non-empty description", |r| !r.description.is_empty())
    ) {
        let iptables = rule.render_iptables();
        let nftables = rule.render_nftables();

        prop_assert!(iptables.contains(&rule.description));
        prop_assert!(nftables.contains(&rule.description));
    }
}
