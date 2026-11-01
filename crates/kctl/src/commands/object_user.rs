//! `kctl` subcommands for [`ObjectUser`] (an RGW user on an ObjectStore).

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::client::{self, controller_proto};
use crate::config::ConnectionInfo;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    kind: String,
    metadata: ManifestMetadata,
    spec: ManifestSpec,
}

#[derive(Debug, Deserialize)]
struct ManifestMetadata {
    name: String,
}

#[derive(Debug, Deserialize)]
struct ManifestSpec {
    store: String,
}

fn parse_manifest_str(data: &str) -> Result<Manifest> {
    let manifest: Manifest = serde_yaml::from_str(data).context("parsing YAML")?;
    let kind = manifest.kind.trim().to_ascii_lowercase();
    if kind != "objectuser" && kind != "object-user" && kind != "object_user" {
        bail!("expected kind: ObjectUser, got {}", manifest.kind);
    }
    if manifest.metadata.name.trim().is_empty() {
        bail!("metadata.name is required");
    }
    if manifest.spec.store.trim().is_empty() {
        bail!("spec.store is required");
    }
    Ok(manifest)
}

pub async fn apply_from_file(info: &ConnectionInfo, file: &str) -> Result<()> {
    let data = std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?;
    let manifest = parse_manifest_str(&data).with_context(|| format!("in {file}"))?;
    create(
        info,
        manifest.metadata.name.trim(),
        manifest.spec.store.trim(),
    )
    .await
}

pub async fn create(info: &ConnectionInfo, name: &str, store: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .create_object_user(controller_proto::CreateObjectUserRequest {
            object_user: Some(controller_proto::ObjectUser {
                name: name.to_string(),
                spec: Some(controller_proto::ObjectUserSpec {
                    store: store.to_string(),
                }),
                access_key: String::new(),
                created_at: None,
            }),
        })
        .await
        .context("create_object_user rpc")?
        .into_inner();
    if !resp.success {
        bail!("the controller did not create ObjectUser '{name}'");
    }
    let user = resp.object_user.unwrap_or_default();
    println!("Created ObjectUser '{name}' on store '{store}'.");
    println!("Access key: {}", user.access_key);
    if resp.secret.is_empty() {
        println!("Secret key: (not returned; the user already existed)");
    } else {
        println!("Secret key: {}", resp.secret);
        println!("The secret key is shown once. Store it now.");
    }
    Ok(())
}

pub async fn delete(info: &ConnectionInfo, name: &str, store: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .delete_object_user(controller_proto::DeleteObjectUserRequest {
            name: name.to_string(),
            store: store.to_string(),
        })
        .await
        .context("delete_object_user rpc")?
        .into_inner();
    if resp.success {
        println!("Deleted ObjectUser '{name}' from store '{store}'.");
    } else {
        bail!("ObjectUser '{name}' was not found on store '{store}'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_an_object_user_manifest() {
        let manifest =
            parse_manifest_str("kind: ObjectUser\nmetadata:\n  name: app\nspec:\n  store: s3\n")
                .unwrap();
        assert_eq!(manifest.metadata.name, "app");
        assert_eq!(manifest.spec.store, "s3");
    }

    #[test]
    fn refuses_other_kinds_and_empty_fields() {
        assert!(
            parse_manifest_str("kind: ObjectStore\nmetadata: {name: a}\nspec: {store: s}\n")
                .is_err()
        );
        assert!(
            parse_manifest_str("kind: ObjectUser\nmetadata: {name: ' '}\nspec: {store: s}\n")
                .is_err()
        );
        assert!(
            parse_manifest_str("kind: ObjectUser\nmetadata: {name: a}\nspec: {store: ''}\n")
                .is_err()
        );
    }
}

#[cfg(test)]
mod shipped_example_tests {
    use super::*;

    #[test]
    fn shipped_example_parses() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/object-user/backup-writer.yaml"
        );
        let data = std::fs::read_to_string(path).unwrap();
        let manifest = parse_manifest_str(&data).unwrap();
        assert_eq!(manifest.spec.store, "backups");
    }
}
