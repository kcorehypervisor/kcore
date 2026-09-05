//! `kctl` subcommands for the certificate inventory, rotation, revocation,
//! CRL and OCSP surfaces the controller exposes.
//!
//! Everything here talks to the controller over the existing mTLS gRPC
//! channel, so an operator needs no extra credential and no access to the
//! controller's database or the plain-HTTP PKI endpoints.

use anyhow::{bail, Context, Result};

use crate::client::{self, controller_proto};
use crate::config::ConnectionInfo;

/// RFC 5280 §5.3.1 reason codes, spelled the way an operator would type them.
///
/// Code 7 is unused by the RFC and deliberately absent.
pub const REASON_NAMES: &[(&str, i32)] = &[
    ("unspecified", 0),
    ("key-compromise", 1),
    ("ca-compromise", 2),
    ("affiliation-changed", 3),
    ("superseded", 4),
    ("cessation-of-operation", 5),
    ("certificate-hold", 6),
    ("remove-from-crl", 8),
    ("privilege-withdrawn", 9),
    ("aa-compromise", 10),
];

/// Map an operator-typed reason onto its RFC 5280 code.
///
/// Both the dashed spelling and the bare numeric code are accepted, so scripts
/// can pass whichever they already have.
pub fn parse_reason(input: &str) -> Result<i32> {
    let normalized = input.trim().to_ascii_lowercase().replace('_', "-");
    if let Some((_, code)) = REASON_NAMES.iter().find(|(name, _)| *name == normalized) {
        return Ok(*code);
    }
    if let Ok(code) = normalized.parse::<i32>() {
        if REASON_NAMES.iter().any(|(_, c)| *c == code) {
            return Ok(code);
        }
        bail!("{code} is not an RFC 5280 revocation reason code");
    }
    bail!(
        "unknown revocation reason '{input}'; expected one of: {}",
        REASON_NAMES
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Human name for a reason code, for display.
pub fn reason_name(code: i32) -> &'static str {
    REASON_NAMES
        .iter()
        .find(|(_, c)| *c == code)
        .map(|(name, _)| *name)
        .unwrap_or("unknown")
}

/// Accept the status filter spellings `kctl get certificates --status` takes.
pub fn parse_status(input: &str) -> Result<controller_proto::CertificateStatus> {
    match input.trim().to_ascii_lowercase().as_str() {
        "" | "all" | "any" => Ok(controller_proto::CertificateStatus::Unspecified),
        "active" => Ok(controller_proto::CertificateStatus::Active),
        "rotated" | "superseded" => Ok(controller_proto::CertificateStatus::Rotated),
        "revoked" => Ok(controller_proto::CertificateStatus::Revoked),
        other => {
            bail!("unknown certificate status '{other}'; expected active, rotated, revoked or all")
        }
    }
}

fn status_label(status: i32) -> &'static str {
    match controller_proto::CertificateStatus::try_from(status) {
        Ok(controller_proto::CertificateStatus::Active) => "active",
        Ok(controller_proto::CertificateStatus::Rotated) => "rotated",
        Ok(controller_proto::CertificateStatus::Revoked) => "revoked",
        _ => "unknown",
    }
}

/// Render `days_until_expiry` so an operator can see urgency at a glance.
pub fn expiry_label(days: i32) -> String {
    if days < 0 {
        format!("EXPIRED {}d ago", -days)
    } else {
        format!("{days}d")
    }
}

/// Shorten a serial for table display, keeping enough to identify it.
pub fn short_serial(serial_hex: &str) -> String {
    if serial_hex.len() <= 20 {
        serial_hex.to_string()
    } else {
        format!(
            "{}…{}",
            &serial_hex[..12],
            &serial_hex[serial_hex.len() - 6..]
        )
    }
}

fn print_certificate_table(certs: &[controller_proto::CertificateInfo]) {
    println!(
        "{:<22}  {:<28}  {:<10}  {:<9}  {:<12}  REASON",
        "SERIAL", "SUBJECT", "KIND", "STATUS", "EXPIRES IN"
    );
    for cert in certs {
        let reason = if status_label(cert.status) == "revoked" {
            reason_name(cert.revocation_reason).to_string()
        } else {
            "-".to_string()
        };
        println!(
            "{:<22}  {:<28}  {:<10}  {:<9}  {:<12}  {}",
            short_serial(&cert.serial_hex),
            cert.subject_cn,
            cert.identity_kind,
            status_label(cert.status),
            expiry_label(cert.days_until_expiry),
            reason
        );
    }
}

/// `kctl get certificates [--node ID] [--status S] [--expiring-within-days N]`
pub async fn list_certificates(
    info: &ConnectionInfo,
    node_id: Option<&str>,
    status: Option<&str>,
    expiring_within_days: i32,
) -> Result<()> {
    let status = match status {
        Some(s) => parse_status(s)?,
        None => controller_proto::CertificateStatus::Unspecified,
    };
    let mut client = client::controller_client(info).await?;
    let resp = client
        .list_certificates(controller_proto::ListCertificatesRequest {
            status: status as i32,
            node_id: node_id.unwrap_or_default().to_string(),
            expiring_within_days,
        })
        .await
        .context("list_certificates rpc")?
        .into_inner();

    if resp.certificates.is_empty() {
        println!("No certificates found");
        return Ok(());
    }
    print_certificate_table(&resp.certificates);
    Ok(())
}

/// `kctl get pki-status`
pub async fn pki_status(info: &ConnectionInfo) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let r = client
        .get_pki_status(controller_proto::GetPkiStatusRequest {})
        .await
        .context("get_pki_status rpc")?
        .into_inner();

    println!("Certificate inventory");
    println!("  Active:        {}", r.active_count);
    println!("  Rotated:       {}", r.rotated_count);
    println!("  Revoked:       {}", r.revoked_count);
    println!("  Expired:       {}", r.expired_count);
    println!(
        "  Expiring soon: {} (within {} days)",
        r.expiring_soon_count, r.warn_before_days
    );
    println!();
    println!("Rotation");
    println!(
        "  Enabled:       {}",
        if r.rotation_enabled { "yes" } else { "no" }
    );
    println!("  Renew before:  {} days", r.renew_before_days);
    println!();
    println!("Revocation");
    println!(
        "  Sub-CA:        {}",
        if r.sub_ca_available {
            "available"
        } else {
            "MISSING (rotation and revocation unavailable)"
        }
    );
    println!("  Fail mode:     {}", r.revocation_fail_mode);
    if r.crl_available {
        println!("  CRL number:    {}", r.crl_number);
        println!(
            "  CRL window:    {} -> {}",
            format_ts(r.crl_this_update.as_ref()),
            format_ts(r.crl_next_update.as_ref())
        );
    } else {
        println!("  CRL:           not generated yet");
    }
    if r.pki_http_base_url.is_empty() {
        println!("  PKI HTTP:      disabled (use `kctl get crl` over gRPC)");
    } else {
        println!("  CRL URL:       {}/pki/crl.der", r.pki_http_base_url);
        println!("  OCSP URL:      {}/pki/ocsp", r.pki_http_base_url);
    }

    if !r.soonest_expiring.is_empty() {
        println!();
        println!("Soonest expiring active certificates");
        print_certificate_table(&r.soonest_expiring);
    }

    // Non-zero exit would break `kctl get` conventions, so surface urgency as
    // a warning line instead.
    if r.expiring_soon_count > 0 {
        println!();
        println!(
            "WARNING: {} certificate(s) expire within {} days; run `kctl rotate node-certs --all` \
             or wait for the controller's rotation loop",
            r.expiring_soon_count, r.warn_before_days
        );
    }
    Ok(())
}

/// `kctl get crl [--output FILE]`
pub async fn get_crl(info: &ConnectionInfo, output: Option<&std::path::Path>) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let r = client
        .get_crl(controller_proto::GetCrlRequest {})
        .await
        .context("get_crl rpc")?
        .into_inner();
    if !r.success {
        bail!("controller has no CRL: {}", r.message);
    }

    if let Some(path) = output {
        // DER when the caller asked for a .der file, PEM otherwise: that is
        // what `openssl crl -in <file>` and web servers expect respectively.
        let want_der = path
            .extension()
            .map(|e| e.eq_ignore_ascii_case("der"))
            .unwrap_or(false);
        if want_der {
            std::fs::write(path, &r.crl_der)
                .with_context(|| format!("writing {}", path.display()))?;
        } else {
            std::fs::write(path, r.crl_pem.as_bytes())
                .with_context(|| format!("writing {}", path.display()))?;
        }
        println!(
            "Wrote CRL number {} ({} revoked, {} format) to {}",
            r.crl_number,
            r.revoked_count,
            if want_der { "DER" } else { "PEM" },
            path.display()
        );
    } else {
        println!("CRL number:   {}", r.crl_number);
        println!("Revoked:      {}", r.revoked_count);
        println!("This update:  {}", format_ts(r.this_update.as_ref()));
        println!("Next update:  {}", format_ts(r.next_update.as_ref()));
        println!();
        print!("{}", r.crl_pem);
    }
    Ok(())
}

/// `kctl revoke cert --serial S | --node ID | --subject CN --reason R`
pub async fn revoke_certificate(
    info: &ConnectionInfo,
    serial_hex: Option<&str>,
    subject_cn: Option<&str>,
    node_id: Option<&str>,
    reason: &str,
) -> Result<()> {
    if serial_hex.is_none() && subject_cn.is_none() && node_id.is_none() {
        bail!("one of --serial, --subject or --node is required");
    }
    let reason_code = parse_reason(reason)?;

    let mut client = client::controller_client(info).await?;
    let r = client
        .revoke_certificate(controller_proto::RevokeCertificateRequest {
            serial_hex: serial_hex.unwrap_or_default().to_string(),
            subject_cn: subject_cn.unwrap_or_default().to_string(),
            node_id: node_id.unwrap_or_default().to_string(),
            reason: reason_code,
        })
        .await
        .context("revoke_certificate rpc")?
        .into_inner();

    if !r.success {
        bail!("revocation failed: {}", r.message);
    }
    println!("{}", r.message);
    for cert in &r.revoked {
        println!(
            "  revoked {} ({}) reason={}",
            cert.serial_hex,
            cert.subject_cn,
            reason_name(cert.revocation_reason)
        );
    }
    if r.crl_number > 0 {
        println!("CRL regenerated as number {}", r.crl_number);
    } else {
        println!(
            "WARNING: the CRL was not regenerated; peers will not see this revocation until the \
             controller's next CRL refresh"
        );
    }
    Ok(())
}

/// `kctl rotate node-certs [--node ID | --all]`
pub async fn rotate_node_certs(
    info: &ConnectionInfo,
    node_id: Option<&str>,
    all_nodes: bool,
) -> Result<()> {
    if node_id.is_none() && !all_nodes {
        bail!("one of --node or --all is required");
    }
    if node_id.is_some() && all_nodes {
        bail!("--node and --all are mutually exclusive");
    }

    let mut client = client::controller_client(info).await?;
    let r = client
        .rotate_node_certs(controller_proto::RotateNodeCertsRequest {
            node_id: node_id.unwrap_or_default().to_string(),
            all_nodes,
        })
        .await
        .context("rotate_node_certs rpc")?
        .into_inner();

    if r.results.is_empty() {
        println!("{}", r.message);
        return Ok(());
    }
    println!(
        "{:<28}  {:<8}  {:<22}  MESSAGE",
        "NODE", "RESULT", "NEW SERIAL"
    );
    for result in &r.results {
        println!(
            "{:<28}  {:<8}  {:<22}  {}",
            result.node_id,
            if result.success { "ok" } else { "failed" },
            if result.serial_hex.is_empty() {
                "-".to_string()
            } else {
                short_serial(&result.serial_hex)
            },
            result.message
        );
    }
    if !r.success {
        // Partial failure is expected during a rolling update: the nodes that
        // did not rotate keep their existing certificates and the controller's
        // reconciler retries them.
        bail!("{}", r.message);
    }
    println!("{}", r.message);
    Ok(())
}

fn format_ts(ts: Option<&prost_types::Timestamp>) -> String {
    match ts {
        Some(t) if t.seconds > 0 => time::OffsetDateTime::from_unix_timestamp(t.seconds)
            .map(|dt| {
                format!(
                    "{:04}-{:02}-{:02} {:02}:{:02}:{:02}Z",
                    dt.year(),
                    dt.month() as u8,
                    dt.day(),
                    dt.hour(),
                    dt.minute(),
                    dt.second()
                )
            })
            .unwrap_or_else(|_| "-".to_string()),
        _ => "-".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Argument validation happens before any connection attempt, so these
    /// tests never need a live controller.
    fn unreachable_controller() -> ConnectionInfo {
        ConnectionInfo {
            address: "http://127.0.0.1:1".to_string(),
            addresses: vec![],
            insecure: true,
            tls_server_name: None,
            cert_pem: None,
            key_pem: None,
            ca_pem: None,
            cert: None,
            key: None,
            ca: None,
        }
    }

    #[test]
    fn parse_reason_accepts_names_underscores_and_codes() {
        assert_eq!(parse_reason("key-compromise").expect("name"), 1);
        assert_eq!(parse_reason("KEY_COMPROMISE").expect("underscores"), 1);
        assert_eq!(parse_reason(" superseded ").expect("padded"), 4);
        assert_eq!(parse_reason("10").expect("numeric"), 10);
        assert_eq!(parse_reason("0").expect("unspecified"), 0);
    }

    #[test]
    fn parse_reason_rejects_code_seven_and_nonsense() {
        // RFC 5280 §5.3.1 leaves 7 unused, so accepting it would put a value
        // on the CRL that no verifier can interpret.
        let err = parse_reason("7").expect_err("code 7 is unused");
        assert!(format!("{err}").contains("not an RFC 5280"), "{err}");
        assert!(parse_reason("because-i-said-so").is_err());
        assert!(parse_reason("99").is_err());
    }

    #[test]
    fn reason_name_round_trips_every_code() {
        for (name, code) in REASON_NAMES {
            assert_eq!(reason_name(*code), *name);
            assert_eq!(parse_reason(name).expect("round trip"), *code);
        }
        assert_eq!(reason_name(-1), "unknown");
        assert_eq!(reason_name(7), "unknown");
    }

    #[test]
    fn parse_status_accepts_filters_and_aliases() {
        assert_eq!(
            parse_status("active").expect("active"),
            controller_proto::CertificateStatus::Active
        );
        assert_eq!(
            parse_status("superseded").expect("alias"),
            controller_proto::CertificateStatus::Rotated
        );
        assert_eq!(
            parse_status("all").expect("all"),
            controller_proto::CertificateStatus::Unspecified
        );
        assert_eq!(
            parse_status("").expect("empty means all"),
            controller_proto::CertificateStatus::Unspecified
        );
        assert!(parse_status("pending").is_err());
    }

    #[test]
    fn status_label_covers_every_variant() {
        assert_eq!(
            status_label(controller_proto::CertificateStatus::Active as i32),
            "active"
        );
        assert_eq!(
            status_label(controller_proto::CertificateStatus::Rotated as i32),
            "rotated"
        );
        assert_eq!(
            status_label(controller_proto::CertificateStatus::Revoked as i32),
            "revoked"
        );
        assert_eq!(status_label(99), "unknown");
    }

    #[test]
    fn expiry_label_calls_out_expired_certificates() {
        assert_eq!(expiry_label(30), "30d");
        assert_eq!(expiry_label(0), "0d");
        assert_eq!(expiry_label(-3), "EXPIRED 3d ago");
    }

    #[test]
    fn short_serial_keeps_short_serials_intact() {
        assert_eq!(short_serial("0A1B"), "0A1B");
        let long = "0123456789ABCDEF0123456789ABCDEF";
        let short = short_serial(long);
        assert!(short.starts_with("0123456789AB"), "{short}");
        assert!(short.ends_with("ABCDEF"), "{short}");
        assert!(short.chars().count() < long.chars().count());
    }

    #[test]
    fn format_ts_renders_epoch_seconds_and_handles_absence() {
        assert_eq!(format_ts(None), "-");
        assert_eq!(
            format_ts(Some(&prost_types::Timestamp {
                seconds: 0,
                nanos: 0
            })),
            "-"
        );
        assert_eq!(
            format_ts(Some(&prost_types::Timestamp {
                seconds: 1_700_000_000,
                nanos: 0
            })),
            "2023-11-14 22:13:20Z"
        );
    }

    #[tokio::test]
    async fn revoke_requires_a_target() {
        let info = unreachable_controller();
        let err = revoke_certificate(&info, None, None, None, "superseded")
            .await
            .expect_err("no target selector");
        assert!(format!("{err}").contains("--serial"), "{err}");
    }

    #[tokio::test]
    async fn rotate_node_certs_rejects_ambiguous_and_empty_selection() {
        let info = unreachable_controller();
        let err = rotate_node_certs(&info, None, false)
            .await
            .expect_err("nothing selected");
        assert!(format!("{err}").contains("--node"), "{err}");

        let err = rotate_node_certs(&info, Some("node-1"), true)
            .await
            .expect_err("both selected");
        assert!(format!("{err}").contains("mutually exclusive"), "{err}");
    }
}

/// Property tests for the reason-code mapping, which has to stay a bijection
/// against the RFC 5280 table.
#[cfg(test)]
mod prop_tests {
    use super::{parse_reason, reason_name, REASON_NAMES};
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig { cases: 1_000, .. ProptestConfig::default() })]

        /// Any casing or underscore spelling of a known reason name parses to
        /// the same code.
        #[test]
        fn reason_parsing_ignores_case_and_separator(
            idx in 0usize..REASON_NAMES.len(),
            upper in any::<bool>(),
            underscores in any::<bool>(),
        ) {
            let (name, code) = REASON_NAMES[idx];
            let mut spelling = name.to_string();
            if underscores {
                spelling = spelling.replace('-', "_");
            }
            if upper {
                spelling = spelling.to_ascii_uppercase();
            }
            prop_assert_eq!(parse_reason(&spelling).ok(), Some(code));
        }

        /// Codes outside the RFC table are always rejected, so an operator
        /// cannot put a meaningless reason on the CRL.
        #[test]
        fn unknown_codes_are_rejected(code in -1000i32..1000) {
            let known = REASON_NAMES.iter().any(|(_, c)| *c == code);
            prop_assert_eq!(parse_reason(&code.to_string()).is_ok(), known);
        }

        /// `parse_reason` never panics, whatever an operator types.
        #[test]
        fn parse_reason_never_panics(s in ".{0,32}") {
            let _ = parse_reason(&s);
        }

        /// Every code that parses has a display name that is not "unknown".
        #[test]
        fn parsed_codes_always_have_a_name(idx in 0usize..REASON_NAMES.len()) {
            let (_, code) = REASON_NAMES[idx];
            prop_assert_ne!(reason_name(code), "unknown");
        }
    }
}
