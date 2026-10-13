//! Retrieve or store Ceph mgr Dashboard login credentials.
//!
//! Ceph hashes dashboard passwords server-side, so the plaintext must be kept
//! in the kcore controller (cluster-admin only) for operator retrieval via
//! `kctl get ceph-dashboard-credentials`.

use anyhow::{bail, Context, Result};

use crate::client::{self, controller_proto};
use crate::config::ConnectionInfo;

pub async fn get(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .get_ceph_dashboard_credentials(controller_proto::GetCephDashboardCredentialsRequest {
            name: name.to_string(),
        })
        .await
        .context("GetCephDashboardCredentials RPC")?
        .into_inner();
    println!("Name:       {}", resp.name);
    println!("URL:        {}", resp.url);
    println!("Username:   {}", resp.username);
    println!("Password:   {}", resp.password);
    println!("Updated at: {}", resp.updated_at);
    Ok(())
}

pub async fn set(
    info: &ConnectionInfo,
    name: &str,
    url: &str,
    username: &str,
    password: &str,
) -> Result<()> {
    if password.is_empty() {
        bail!("password must not be empty");
    }
    let mut client = client::controller_client(info).await?;
    let resp = client
        .set_ceph_dashboard_credentials(controller_proto::SetCephDashboardCredentialsRequest {
            name: name.to_string(),
            url: url.to_string(),
            username: username.to_string(),
            password: password.to_string(),
        })
        .await
        .context("SetCephDashboardCredentials RPC")?
        .into_inner();
    if !resp.success {
        bail!("failed to store dashboard credentials for '{name}'");
    }
    let stored = resp
        .credentials
        .ok_or_else(|| anyhow::anyhow!("empty credentials in response"))?;
    println!("Stored Ceph dashboard credentials for '{}'", stored.name);
    println!("URL:        {}", stored.url);
    println!("Username:   {}", stored.username);
    println!("Updated at: {}", stored.updated_at);
    Ok(())
}
