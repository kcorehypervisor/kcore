//! Fill questions from what the cluster already has. A restricted question becomes a pick list;
//! an unrestricted one keeps free text and lists the existing entries in its explanation.
//! A source that cannot be read leaves the question exactly as it was.

use std::collections::{HashMap, HashSet};
use std::future::Future;

use futures_util::future::join_all;

use super::advise::{OptionSource, Question};

#[derive(Debug, Clone, PartialEq)]
pub struct ClusterOption {
    pub value: String,
    pub label: Option<String>,
}

fn noun(source: OptionSource) -> &'static str {
    match source {
        OptionSource::Networks => "networks",
        OptionSource::SshKeys => "stored ssh keys",
        OptionSource::Nodes => "nodes",
        OptionSource::Vms => "VMs",
        OptionSource::NodeImages => "images on the nodes",
    }
}

pub async fn resolve_questions<F, Fut>(questions: Vec<Question>, load: F) -> Vec<Question>
where
    F: Fn(OptionSource) -> Fut,
    Fut: Future<Output = Option<Vec<ClusterOption>>>,
{
    let mut sources: Vec<OptionSource> = Vec::new();
    for item in &questions {
        if let Some(source) = item.source {
            if !sources.contains(&source) {
                sources.push(source);
            }
        }
    }
    let loaded = join_all(sources.iter().map(|source| load(*source))).await;
    let found: HashMap<OptionSource, Option<Vec<ClusterOption>>> =
        sources.into_iter().zip(loaded).collect();

    questions
        .into_iter()
        .map(|mut item| {
            let Some(source) = item.source else {
                return item;
            };
            let Some(Some(options)) = found.get(&source) else {
                return item;
            };
            let unique = dedupe(options);
            if unique.is_empty() {
                item.why = format!("{} No {} exist yet.", item.why, noun(source));
                return item;
            }
            if item.restrict == Some(true) && item.choices.is_none() {
                item.choices = Some(unique.iter().map(|o| o.value.clone()).collect());
                item.choice_labels = Some(
                    unique
                        .iter()
                        .map(|o| o.label.clone().unwrap_or_else(|| o.value.clone()))
                        .collect(),
                );
                return item;
            }
            let shown = unique
                .iter()
                .take(12)
                .map(|o| o.value.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let more = if unique.len() > 12 {
                format!(", and {} more", unique.len() - 12)
            } else {
                String::new()
            };
            item.why = format!("{} Existing {}: {shown}{more}.", item.why, noun(source));
            item
        })
        .collect()
}

fn dedupe(items: &[ClusterOption]) -> Vec<ClusterOption> {
    let mut seen = HashSet::new();
    items
        .iter()
        .filter(|item| !item.value.is_empty() && seen.insert(item.value.clone()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opt(value: &str) -> ClusterOption {
        ClusterOption {
            value: value.to_string(),
            label: None,
        }
    }

    fn net_question(restrict: bool) -> Question {
        let mut q = Question::new("network", "Which network?", "Needed.", true);
        q.source = Some(OptionSource::Networks);
        if restrict {
            q.restrict = Some(true);
        }
        q
    }

    #[tokio::test]
    async fn a_restricted_question_becomes_a_pick_list() {
        let out = resolve_questions(vec![net_question(true)], |_| async {
            Some(vec![opt("a"), opt("b"), opt("a")])
        })
        .await;
        assert_eq!(
            out[0].choices.as_deref(),
            Some(&["a".to_string(), "b".to_string()][..])
        );
    }

    #[tokio::test]
    async fn an_unrestricted_question_lists_existing_entries() {
        let out = resolve_questions(vec![net_question(false)], |_| async {
            Some(vec![opt("a")])
        })
        .await;
        assert!(out[0].choices.is_none());
        assert!(out[0].why.contains("Existing networks: a."));
    }

    #[tokio::test]
    async fn an_empty_source_says_so_and_an_unreadable_one_changes_nothing() {
        let out = resolve_questions(vec![net_question(true)], |_| async { Some(vec![]) }).await;
        assert!(out[0].why.contains("No networks exist yet."));
        let original = net_question(true);
        let out = resolve_questions(vec![original.clone()], |_| async { None }).await;
        assert_eq!(out[0], original);
    }
}
