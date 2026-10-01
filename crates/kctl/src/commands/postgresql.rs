use anyhow::{Context, Result};

use crate::client::{self, controller_proto as proto};
use crate::config::ConnectionInfo;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostgresqlManifest {
    pub name: String,
    pub database: String,
    pub package: String,
    pub port: i32,
    pub target_node: String,
}

pub async fn create(
    info: &ConnectionInfo,
    name: &str,
    database: Option<&str>,
    package: &str,
    port: i32,
    target_node: Option<&str>,
) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .create_postgresql(proto::CreatePostgresqlRequest {
            name: name.to_string(),
            database: database.unwrap_or_default().to_string(),
            package: package.to_string(),
            port,
            target_node: target_node.unwrap_or_default().to_string(),
        })
        .await?
        .into_inner();

    let label = format!("postgresql '{name}'");
    println!(
        "{}",
        crate::apply_summary::render_apply_summary(resp.action, &resp.changed_fields, &label)
    );
    let msg = resp.message.trim();
    if !msg.is_empty() {
        println!("  {msg}");
    }
    if let Some(pg) = resp.postgresql {
        println!("  Database: {}", pg.database);
        println!("  Package:  pkgs.{}", pg.package);
        println!("  Port:     {}", pg.port);
        println!("  Node:     {}", pg.node_id);
    }
    Ok(())
}

pub fn parse_manifest(data: &str) -> Result<PostgresqlManifest> {
    let doc: serde_yaml::Value =
        serde_yaml::from_str(data).context("parsing postgresql manifest")?;
    let kind = doc["kind"].as_str().unwrap_or("");
    if !matches!(
        kind.to_ascii_lowercase().as_str(),
        "postgresql" | "postgres"
    ) {
        anyhow::bail!("expected kind: Postgresql, got {kind}");
    }
    let name = doc["metadata"]["name"]
        .as_str()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| anyhow::anyhow!("metadata.name is required"))?
        .to_string();
    let spec = &doc["spec"];
    let database = spec["database"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(name.as_str())
        .to_string();
    let package = spec["package"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("postgresql")
        .to_string();
    let port = match &spec["port"] {
        serde_yaml::Value::Null => 5432,
        serde_yaml::Value::Number(number) => number
            .as_i64()
            .and_then(|value| i32::try_from(value).ok())
            .ok_or_else(|| anyhow::anyhow!("spec.port must be an integer"))?,
        serde_yaml::Value::String(text) => text
            .trim()
            .parse::<i32>()
            .context("spec.port must be an integer")?,
        _ => anyhow::bail!("spec.port must be an integer"),
    };
    let target_node = spec["targetNode"]
        .as_str()
        .or_else(|| spec["target_node"].as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    Ok(PostgresqlManifest {
        name,
        database,
        package,
        port,
        target_node,
    })
}

pub async fn create_from_manifest(info: &ConnectionInfo, path: &str) -> Result<()> {
    let data = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    let manifest = parse_manifest(&data)?;
    create(
        info,
        &manifest.name,
        Some(&manifest.database),
        &manifest.package,
        manifest.port,
        Some(manifest.target_node.as_str()).filter(|node| !node.is_empty()),
    )
    .await
}

pub async fn delete(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .delete_postgresql(proto::DeletePostgresqlRequest {
            name: name.to_string(),
        })
        .await?
        .into_inner();
    println!("{}", resp.message);
    Ok(())
}

pub async fn get(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .get_postgresql(proto::GetPostgresqlRequest {
            name: name.to_string(),
        })
        .await?
        .into_inner();
    let pg = resp
        .postgresql
        .context(format!("postgresql '{name}' not found"))?;
    print_postgresql(&pg);
    Ok(())
}

pub async fn list(info: &ConnectionInfo, target_node: Option<&str>) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .list_postgresqls(proto::ListPostgresqlsRequest {
            target_node: target_node.unwrap_or_default().to_string(),
        })
        .await?
        .into_inner();
    if resp.postgresqls.is_empty() {
        println!("No PostgreSQL databases found");
        return Ok(());
    }
    println!(
        "{:<20}  {:<20}  {:<16}  {:>6}  {:<20}",
        "NAME", "DATABASE", "PACKAGE", "PORT", "NODE"
    );
    for pg in &resp.postgresqls {
        println!(
            "{:<20}  {:<20}  {:<16}  {:>6}  {:<20}",
            pg.name, pg.database, pg.package, pg.port, pg.node_id
        );
    }
    Ok(())
}

fn print_postgresql(pg: &proto::PostgresqlInfo) {
    println!("Name:     {}", pg.name);
    println!("Database: {}", pg.database);
    println!("Package:  pkgs.{}", pg.package);
    println!("Port:     {}", pg.port);
    println!("Node:     {}", pg.node_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_database_manifest() {
        let manifest = parse_manifest(
            "kind: Postgresql\nmetadata:\n  name: app\nspec:\n  package: postgresql_16\n  targetNode: node-a\n",
        )
        .expect("parse");
        assert_eq!(manifest.name, "app");
        assert_eq!(manifest.database, "app");
        assert_eq!(manifest.package, "postgresql_16");
        assert_eq!(manifest.port, 5432);
        assert_eq!(manifest.target_node, "node-a");
    }

    #[test]
    fn rejects_other_kinds() {
        let err = parse_manifest("kind: VM\nmetadata:\n  name: app\n").unwrap_err();
        assert!(err.to_string().contains("expected kind: Postgresql"));
    }
}
