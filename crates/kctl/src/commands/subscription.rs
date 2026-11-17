//! Enterprise update-channel subscription (GitHub classic PAT).

use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::client::{self, controller_proto};
use crate::config::ConnectionInfo;

pub async fn get(info: &ConnectionInfo) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .get_subscription(controller_proto::GetSubscriptionRequest {})
        .await
        .context("GetSubscription RPC")?
        .into_inner();
    if !resp.configured {
        println!("subscription: not configured");
        println!("{}", resp.message);
        return Ok(());
    }
    println!("subscription: configured");
    if !resp.github_login.is_empty() {
        println!("github_login: {}", resp.github_login);
    }
    if !resp.latest_release.is_empty() {
        println!("latest_release: {}", resp.latest_release);
    }
    println!("{}", resp.message);
    Ok(())
}

pub async fn set(info: &ConnectionInfo, token_file: &Path) -> Result<()> {
    let token = fs::read_to_string(token_file)
        .with_context(|| format!("read {}", token_file.display()))?
        .trim()
        .to_string();
    if token.is_empty() {
        bail!("{} is empty", token_file.display());
    }
    let mut client = client::controller_client(info).await?;
    let resp = client
        .set_subscription(controller_proto::SetSubscriptionRequest {
            token,
            clear: false,
        })
        .await
        .context("SetSubscription RPC")?
        .into_inner();
    if !resp.success {
        bail!("{}", resp.message);
    }
    println!("{}", resp.message);
    if !resp.github_login.is_empty() {
        println!("github_login: {}", resp.github_login);
    }
    Ok(())
}

pub async fn clear(info: &ConnectionInfo) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .set_subscription(controller_proto::SetSubscriptionRequest {
            token: String::new(),
            clear: true,
        })
        .await
        .context("SetSubscription RPC")?
        .into_inner();
    if !resp.success {
        bail!("{}", resp.message);
    }
    println!("{}", resp.message);
    Ok(())
}
