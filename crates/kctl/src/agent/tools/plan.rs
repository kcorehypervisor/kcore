//! Field-level comparison of a desired spec against what the cluster already has.

use serde::Serialize;
use serde_json::Value;

use super::catalog::ResourceKind;
use super::names::{is_blank, Spec};

#[derive(Debug, Clone, Serialize)]
pub struct FieldChange {
    pub field: String,
    pub from: Value,
    pub to: Value,
    pub mutable: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct LocalPlan {
    pub action: &'static str,
    pub changes: Vec<FieldChange>,
    pub summary: String,
}

pub fn diff_spec(kind: &ResourceKind, desired: &Spec, current: Option<&Spec>) -> LocalPlan {
    let Some(current) = current else {
        return LocalPlan {
            action: "create",
            changes: Vec::new(),
            summary: format!(
                "{} will be applied as a declarative upsert. The controller creates it, updates mutable fields, or rejects immutable changes.",
                kind.title
            ),
        };
    };

    let mut changes = Vec::new();
    for field in kind.mutable.iter().chain(kind.immutable.iter()) {
        let want = desired.get(*field);
        let have = current.get(*field);
        if is_blank(want) && is_blank(have) {
            continue;
        }
        if want.is_some() && !same(want, have) {
            changes.push(FieldChange {
                field: (*field).to_string(),
                from: have.cloned().unwrap_or(Value::Null),
                to: want.cloned().unwrap_or(Value::Null),
                mutable: kind.mutable.contains(field),
            });
        }
    }

    let immutable: Vec<&str> = changes
        .iter()
        .filter(|change| !change.mutable)
        .map(|change| change.field.as_str())
        .collect();
    if !immutable.is_empty() {
        let summary = format!(
            "{} already exists and the change touches immutable fields ({}). Replacement deletes and recreates it. Ask the operator before doing that.",
            kind.title,
            immutable.join(", ")
        );
        return LocalPlan {
            action: "replace",
            changes,
            summary,
        };
    }
    if changes.is_empty() {
        return LocalPlan {
            action: "unchanged",
            changes,
            summary: format!("{} already matches this spec.", kind.title),
        };
    }
    let summary = format!(
        "{} will update {}.",
        kind.title,
        changes
            .iter()
            .map(|change| change.field.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    LocalPlan {
        action: "update",
        changes,
        summary,
    }
}

fn same(left: Option<&Value>, right: Option<&Value>) -> bool {
    left.unwrap_or(&Value::Null) == right.unwrap_or(&Value::Null)
}

#[cfg(test)]
mod tests {
    use super::super::catalog::find_kind;
    use super::*;
    use serde_json::json;

    fn spec(value: Value) -> Spec {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn a_missing_resource_is_a_create() {
        let kind = find_kind(Some("vm")).unwrap();
        let plan = diff_spec(kind, &spec(json!({"cpu": 2})), None);
        assert_eq!(plan.action, "create");
    }

    #[test]
    fn mutable_changes_are_updates_and_immutable_ones_are_replacements() {
        let kind = find_kind(Some("vm")).unwrap();
        let current = spec(json!({"cpu": 2, "network": "a"}));
        let update = diff_spec(
            kind,
            &spec(json!({"cpu": 4, "network": "a"})),
            Some(&current),
        );
        assert_eq!(update.action, "update");
        let replace = diff_spec(
            kind,
            &spec(json!({"cpu": 2, "network": "b"})),
            Some(&current),
        );
        assert_eq!(replace.action, "replace");
        let same = diff_spec(kind, &current.clone(), Some(&current));
        assert_eq!(same.action, "unchanged");
    }
}
