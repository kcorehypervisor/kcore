use crate::client::controller_proto::ApplyAction;

/// Render a reconcile summary for `kctl` commands that perform a server-side
/// upsert. `kind_and_name` should be a short human label such as `"VM 'web'"`
/// or `"network 'default' on node 'nodeA'"`.
///
/// Examples:
/// - `created VM 'web' (id abc)`
/// - `updated VM 'web' (fields: cpu, memory_bytes)`
/// - `unchanged VM 'web'`
pub fn render_apply_summary(action: i32, changed_fields: &[String], kind_and_name: &str) -> String {
    match ApplyAction::try_from(action).unwrap_or(ApplyAction::Unspecified) {
        ApplyAction::Created => format!("created {kind_and_name}"),
        ApplyAction::Updated => {
            if changed_fields.is_empty() {
                format!("updated {kind_and_name}")
            } else {
                format!(
                    "updated {kind_and_name} (fields: {})",
                    changed_fields.join(", ")
                )
            }
        }
        ApplyAction::Unchanged => format!("unchanged {kind_and_name}"),
        ApplyAction::Unspecified => format!("created {kind_and_name}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_created() {
        assert_eq!(
            render_apply_summary(ApplyAction::Created as i32, &[], "VM 'web'"),
            "created VM 'web'"
        );
    }

    #[test]
    fn renders_updated_with_fields() {
        let fields = vec!["cpu".to_string(), "memory_bytes".to_string()];
        assert_eq!(
            render_apply_summary(ApplyAction::Updated as i32, &fields, "VM 'web'"),
            "updated VM 'web' (fields: cpu, memory_bytes)"
        );
    }

    #[test]
    fn renders_updated_without_fields() {
        assert_eq!(
            render_apply_summary(ApplyAction::Updated as i32, &[], "VM 'web'"),
            "updated VM 'web'"
        );
    }

    #[test]
    fn renders_unchanged() {
        assert_eq!(
            render_apply_summary(ApplyAction::Unchanged as i32, &[], "VM 'web'"),
            "unchanged VM 'web'"
        );
    }

    #[test]
    fn unspecified_falls_back_to_created() {
        assert_eq!(
            render_apply_summary(ApplyAction::Unspecified as i32, &[], "VM 'web'"),
            "created VM 'web'"
        );
    }
}
