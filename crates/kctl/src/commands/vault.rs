//! Install a booted kcore Vault image. The live image serves this on port 9091
//! without TLS, the same port and the same `-k` the hypervisor installer uses.

use anyhow::{anyhow, Result};

use crate::client::vault_install_proto::{Disk, InstallRequest};
use crate::client::{vault_install_client, vault_install_proto};
use crate::config::ConnectionInfo;

pub async fn install(
    info: &ConnectionInfo,
    os_disk: &str,
    data_disk: Option<&str>,
    hostname: Option<&str>,
    reboot: bool,
) -> Result<()> {
    let mut client = vault_install_client(info).await?;
    let resp = client
        .install(InstallRequest {
            os_disk: os_disk.to_string(),
            data_disk: data_disk.unwrap_or("").to_string(),
            hostname: hostname.unwrap_or("").to_string(),
            reboot,
        })
        .await?
        .into_inner();
    if resp.accepted {
        println!("Install accepted: {}", resp.message);
        Ok(())
    } else {
        Err(anyhow!("Install rejected: {}", resp.message))
    }
}

pub async fn disks(info: &ConnectionInfo) -> Result<()> {
    let mut client = vault_install_client(info).await?;
    let resp = client
        .list_disks(vault_install_proto::ListDisksRequest {})
        .await?
        .into_inner();
    println!("{:<20} {:<10} MODEL", "PATH", "SIZE");
    for Disk { path, size, model } in resp.disks {
        println!("{path:<20} {size:<10} {model}");
    }
    Ok(())
}
