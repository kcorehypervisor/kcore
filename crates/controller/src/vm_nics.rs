use tonic::Status;

use crate::controller_proto;
use crate::db::VmNicRow;

pub const MAX_NICS: usize = 8;

/// Turn a CreateVm NIC list into the interfaces that will be stored.
///
/// An empty list is the historical single-NIC default. NIC 0 stays on
/// `vms.network`. Later entries are extra interfaces, each on a different
/// network, so a guest can be multi-homed without two NICs fighting over
/// the same bridge.
pub fn normalize_declared_nics(
    nics: &[controller_proto::Nic],
) -> Result<Vec<controller_proto::Nic>, Status> {
    let mut out = Vec::new();
    if nics.is_empty() {
        out.push(controller_proto::Nic {
            network: "default".to_string(),
            model: "virtio".to_string(),
            mac_address: String::new(),
        });
    } else {
        for nic in nics {
            let network = nic.network.trim();
            if network.is_empty() {
                return Err(Status::invalid_argument("nic network must not be empty"));
            }
            let mac = nic.mac_address.trim().to_string();
            validate_mac(&mac)?;
            let model = {
                let raw = nic.model.trim();
                if raw.is_empty() {
                    "virtio".to_string()
                } else {
                    raw.to_string()
                }
            };
            out.push(controller_proto::Nic {
                network: network.to_string(),
                model,
                mac_address: mac,
            });
        }
    }
    if out.len() > MAX_NICS {
        return Err(Status::invalid_argument(format!(
            "a VM can have at most {MAX_NICS} NICs"
        )));
    }
    let mut seen = std::collections::HashSet::new();
    for nic in &out {
        if !seen.insert(nic.network.clone()) {
            return Err(Status::invalid_argument(format!(
                "VM NIC network '{}' is listed more than once; each NIC must join a different network",
                nic.network
            )));
        }
    }
    Ok(out)
}

pub fn validate_mac(mac: &str) -> Result<(), Status> {
    if mac.is_empty() {
        return Ok(());
    }
    let parts: Vec<&str> = mac.split(':').collect();
    let ok = parts.len() == 6
        && parts
            .iter()
            .all(|part| part.len() == 2 && part.chars().all(|c| c.is_ascii_hexdigit()));
    if ok {
        Ok(())
    } else {
        Err(Status::invalid_argument(format!(
            "nic mac_address '{mac}' must be six hex bytes separated by colons"
        )))
    }
}

pub fn spec_nics(primary_network: &str, extras: &[VmNicRow]) -> Vec<controller_proto::Nic> {
    let mut nics = vec![controller_proto::Nic {
        network: primary_network.to_string(),
        model: "virtio".to_string(),
        mac_address: String::new(),
    }];
    for extra in extras {
        nics.push(controller_proto::Nic {
            network: extra.network.clone(),
            model: if extra.model.is_empty() {
                "virtio".to_string()
            } else {
                extra.model.clone()
            },
            mac_address: extra.mac_address.clone(),
        });
    }
    nics
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nic(network: &str, mac: &str) -> controller_proto::Nic {
        controller_proto::Nic {
            network: network.to_string(),
            model: String::new(),
            mac_address: mac.to_string(),
        }
    }

    #[test]
    fn empty_list_is_one_default_nic() {
        let nics = normalize_declared_nics(&[]).expect("default");
        assert_eq!(nics.len(), 1);
        assert_eq!(nics[0].network, "default");
        assert_eq!(nics[0].model, "virtio");
    }

    #[test]
    fn rejects_duplicate_networks_and_too_many_nics() {
        let err =
            normalize_declared_nics(&[nic("prod", ""), nic("prod", "")]).expect_err("duplicate");
        assert!(err.to_string().contains("more than once"));

        let many: Vec<_> = (0..9).map(|i| nic(&format!("net{i}"), "")).collect();
        let err = normalize_declared_nics(&many).expect_err("cap");
        assert!(err.to_string().contains("8"));
    }

    #[test]
    fn accepts_a_second_nic_and_checks_macs() {
        let nics = normalize_declared_nics(&[
            nic("frontend", "52:54:00:aa:bb:cc"),
            controller_proto::Nic {
                network: "backend".into(),
                model: "e1000".into(),
                mac_address: String::new(),
            },
        ])
        .expect("two nics");
        assert_eq!(nics[1].model, "e1000");
        assert!(normalize_declared_nics(&[nic("frontend", "not-a-mac")]).is_err());
    }
}
