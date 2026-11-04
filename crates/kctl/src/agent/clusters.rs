//! The clusters the agent can drive: the contexts in the kctl config. Each session is bound to one
//! of them when it starts. The operator's last choice is remembered for the next run.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use serde_json::{json, Value};

use super::summary::cluster_summary;
use super::tools::rpc::Endpoints;
use super::tools::Toolbox;
use super::util::write_private;
use crate::config::load_config;

/// Builds the connection for a context name. `None` is the connection kctl itself started with
/// (flags, or the config when it has no contexts).
pub type Resolver =
    Arc<dyn Fn(Option<String>) -> BoxFuture<'static, Result<Endpoints, String>> + Send + Sync>;

const SETTINGS_FILE: &str = "settings.json";

pub struct Clusters {
    config_path: PathBuf,
    settings_path: Option<PathBuf>,
    resolver: Resolver,
    chosen: Mutex<Option<String>>,
    pinned: bool,
}

impl Clusters {
    pub fn new(config_path: PathBuf, home: Option<&Path>, resolver: Resolver) -> Self {
        let settings_path = home.map(|h| h.join(SETTINGS_FILE));
        let chosen = settings_path.as_deref().and_then(read_choice);
        Self {
            config_path,
            settings_path,
            resolver,
            chosen: Mutex::new(chosen),
            pinned: false,
        }
    }

    /// Use only the connection kctl was started with; the config's contexts are not offered.
    pub fn pinned(mut self) -> Self {
        self.pinned = true;
        self
    }

    /// Context names, sorted. Empty when the config is missing or unreadable.
    pub fn names(&self) -> Vec<String> {
        if self.pinned {
            return Vec::new();
        }
        let mut names: Vec<String> = load_config(&self.config_path)
            .map(|config| config.contexts.into_keys().collect())
            .unwrap_or_default();
        names.sort();
        names
    }

    /// What a new session uses: the remembered choice if that context still exists, otherwise the
    /// config's current context, otherwise the first one by name.
    pub fn active(&self) -> Option<String> {
        let names = self.names();
        let chosen = self.chosen.lock().unwrap().clone();
        chosen
            .filter(|name| names.contains(name))
            .or_else(|| {
                load_config(&self.config_path)
                    .ok()
                    .and_then(|config| config.current_context)
                    .filter(|name| names.contains(name))
            })
            .or_else(|| names.first().cloned())
    }

    pub fn select(&self, name: &str) -> Result<(), String> {
        if !self.names().iter().any(|n| n == name) {
            return Err(format!(
                "There is no cluster named {name} in the kctl config."
            ));
        }
        *self.chosen.lock().unwrap() = Some(name.to_string());
        if let Some(path) = &self.settings_path {
            let body = serde_json::to_vec_pretty(&json!({ "cluster": name }))
                .map_err(|e| e.to_string())?;
            write_private(path, &body).map_err(|e| format!("saving the choice: {e:#}"))?;
        }
        Ok(())
    }

    pub fn describe(&self) -> Value {
        let config = if self.pinned {
            Default::default()
        } else {
            load_config(&self.config_path).unwrap_or_default()
        };
        let mut clusters: Vec<Value> = config
            .contexts
            .iter()
            .map(|(name, context)| {
                let controller = context
                    .controllers
                    .first()
                    .cloned()
                    .filter(|c| !c.trim().is_empty())
                    .unwrap_or_else(|| context.controller.clone());
                json!({ "name": name, "controller": controller })
            })
            .collect();
        clusters.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        json!({ "active": self.active(), "clusters": clusters })
    }

    /// The tools for one cluster, or for the active one. A cluster that cannot be connected to
    /// still gets a toolbox, and its tools answer with the reason.
    pub async fn toolbox(&self, name: Option<&str>) -> Arc<Toolbox> {
        let name = name.map(str::to_string).or_else(|| self.active());
        match (self.resolver)(name).await {
            Ok(endpoints) => Arc::new(Toolbox::new(endpoints, self.config_path.clone())),
            Err(reason) => Arc::new(Toolbox::unavailable(reason, self.config_path.clone())),
        }
    }

    pub async fn summary(&self, name: Option<&str>) -> Value {
        let toolbox = self.toolbox(name).await;
        match &toolbox.unavailable {
            Some(reason) => json!({ "error": reason }),
            None => cluster_summary(&toolbox.endpoints.controller),
        }
    }
}

fn read_choice(path: &Path) -> Option<String> {
    let value: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    value
        .get("cluster")?
        .as_str()
        .map(str::to_string)
        .filter(|name| !name.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(dir: &Path, current: Option<&str>) -> PathBuf {
        let path = dir.join("config");
        let current = current
            .map(|c| format!("current-context: {c}\n"))
            .unwrap_or_default();
        std::fs::write(
            &path,
            format!(
                "{current}contexts:\n  lab:\n    controller: 10.0.0.1:9090\n  home:\n    controllers:\n    - 10.0.0.2:9090\n    controller: 10.0.0.9:9090\n"
            ),
        )
        .unwrap();
        path
    }

    fn resolver(seen: Arc<Mutex<Vec<Option<String>>>>) -> Resolver {
        Arc::new(move |name| {
            seen.lock().unwrap().push(name.clone());
            Box::pin(async move {
                match name.as_deref() {
                    Some("lab") | Some("home") | None => Ok(Toolbox::unreachable().endpoints),
                    Some(other) => Err(format!("no certificates for {other}")),
                }
            })
        })
    }

    #[test]
    fn the_active_cluster_falls_back_from_choice_to_current_context_to_first_name() {
        let dir = tempfile::tempdir().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let none = Clusters::new(
            config(dir.path(), None),
            Some(dir.path()),
            resolver(seen.clone()),
        );
        assert_eq!(none.active().as_deref(), Some("home"));
        let current = Clusters::new(
            config(dir.path(), Some("lab")),
            Some(dir.path()),
            resolver(seen.clone()),
        );
        assert_eq!(current.active().as_deref(), Some("lab"));
        current.select("home").unwrap();
        assert_eq!(current.active().as_deref(), Some("home"));
    }

    #[test]
    fn the_choice_is_remembered_and_refused_when_it_names_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let path = config(dir.path(), Some("lab"));
        let first = Clusters::new(path.clone(), Some(dir.path()), resolver(seen.clone()));
        assert!(first.select("nowhere").is_err());
        first.select("home").unwrap();

        let second = Clusters::new(path.clone(), Some(dir.path()), resolver(seen.clone()));
        assert_eq!(second.active().as_deref(), Some("home"));

        std::fs::write(&path, "contexts:\n  lab:\n    controller: 10.0.0.1:9090\n").unwrap();
        assert_eq!(second.active().as_deref(), Some("lab"));
    }

    #[test]
    fn the_settings_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let clusters = Clusters::new(config(dir.path(), None), Some(dir.path()), resolver(seen));
        clusters.select("lab").unwrap();
        let mode = std::fs::metadata(dir.path().join(SETTINGS_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn describe_lists_every_context_with_its_first_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let clusters = Clusters::new(
            config(dir.path(), Some("lab")),
            Some(dir.path()),
            resolver(seen),
        );
        let described = clusters.describe();
        assert_eq!(described["active"], "lab");
        assert_eq!(
            described["clusters"],
            json!([
                { "name": "home", "controller": "10.0.0.2:9090" },
                { "name": "lab", "controller": "10.0.0.1:9090" },
            ])
        );
    }

    #[tokio::test]
    async fn a_toolbox_is_built_for_the_named_cluster_and_failures_become_an_unavailable_toolbox() {
        let dir = tempfile::tempdir().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let clusters = Clusters::new(
            config(dir.path(), Some("lab")),
            Some(dir.path()),
            resolver(seen.clone()),
        );
        assert!(clusters.toolbox(Some("home")).await.unavailable.is_none());
        assert!(clusters.toolbox(None).await.unavailable.is_none());
        let broken = clusters.toolbox(Some("broken")).await;
        assert_eq!(
            broken.unavailable.as_deref(),
            Some("no certificates for broken")
        );
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                Some("home".to_string()),
                Some("lab".to_string()),
                Some("broken".to_string())
            ]
        );
        assert!(clusters.summary(Some("broken")).await["error"]
            .as_str()
            .unwrap()
            .contains("no certificates"));
    }

    #[tokio::test]
    async fn an_unavailable_cluster_refuses_tool_calls_with_the_reason() {
        let clusters = Clusters::new(
            PathBuf::from("/nonexistent"),
            None,
            resolver(Arc::new(Mutex::new(Vec::new()))),
        );
        let toolbox = clusters.toolbox(Some("broken")).await;
        struct Never;
        impl super::super::tools::Ask for Never {
            fn form<'a>(
                &'a self,
                _message: &'a str,
                _schema: Value,
            ) -> BoxFuture<'a, super::super::tools::FormReply> {
                Box::pin(async { super::super::tools::FormReply::Cancel })
            }
        }
        let out = toolbox
            .call(
                "kcore_rpc",
                &json!({ "service": "controller", "method": "listNodes" }),
                &Never,
            )
            .await;
        assert!(
            out.text.contains("no certificates for broken"),
            "{}",
            out.text
        );
        let catalog = toolbox.call("kcore_catalog", &json!({}), &Never).await;
        assert!(!catalog.text.contains("cannot be reached"));
    }
}
