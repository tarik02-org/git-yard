//! Picker rows and their order: which targets show for a query and scope,
//! and how matches and recency combine.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32String};

use super::forge::{self, Forge, Request, Requests};
use super::store::pick_key;
use super::{Listing, Project, Target};
use crate::model::Timestamp;

pub struct Item {
    pub project: Arc<Project>,
    pub target: Target,
    pub time: Option<Timestamp>,
    pub subject: Option<String>,
    pub base: bool,
    pub request: Option<(Forge, Request)>,
    /// Key into the pick history.
    pub key: String,
    haystack: Utf32String,
}

impl Item {
    /// Text of the branch column.
    pub fn branch_text(&self) -> String {
        match &self.target {
            Target::Worktree {
                branch: Some(branch),
                ..
            }
            | Target::Local { branch } => branch.clone(),
            Target::Worktree { path, branch: None } => format!(
                "(detached) {}",
                path.file_name().unwrap_or_default().to_string_lossy()
            ),
            Target::Remote { remote, branch } => format!("{remote}/{branch}"),
            Target::Request { .. } => match &self.request {
                Some((_, request)) => match &request.fork_owner {
                    Some(owner) => format!("{owner}:{}", request.branch),
                    None => request.branch.clone(),
                },
                None => String::new(),
            },
        }
    }

    /// `#412 title · @author`, or the tip commit subject.
    pub fn info_text(&self) -> String {
        match &self.request {
            Some((forge, request)) => format!(
                "{}{} {} · @{}",
                forge.sigil(),
                request.number,
                request.title,
                request.author
            ),
            None => self.subject.clone().unwrap_or_default(),
        }
    }
}

/// Turns one project's listing and requests into rows. Requests attach to
/// the branch they are about; the rest become rows of their own.
pub fn items(listing: &Listing, requests: Option<&Requests>) -> Vec<Item> {
    let project = Arc::new(listing.project.clone());
    let pending: Vec<&Request> = requests.map_or_else(Vec::new, |r| r.items.iter().collect());
    let mut matched = vec![false; pending.len()];
    let mut items = Vec::with_capacity(listing.entries.len());
    for entry in &listing.entries {
        let request = pending
            .iter()
            .position(|request| forge::matches(request, entry, &project));
        if let Some(index) = request {
            matched[index] = true;
        }
        items.push(item(
            &project,
            entry.target.clone(),
            entry.time,
            entry.subject.clone(),
            entry.base,
            request.and_then(|index| Some((requests?.forge, pending[index].clone()))),
        ));
    }
    if let Some(requests) = requests {
        for (request, _) in pending
            .iter()
            .zip(&matched)
            .filter(|(_, matched)| !**matched)
        {
            items.push(item(
                &project,
                Target::Request {
                    number: request.number,
                },
                request.updated_at,
                None,
                false,
                Some((requests.forge, (*request).clone())),
            ));
        }
    }
    items
}

fn item(
    project: &Arc<Project>,
    target: Target,
    time: Option<Timestamp>,
    subject: Option<String>,
    base: bool,
    request: Option<(Forge, Request)>,
) -> Item {
    let mut item = Item {
        key: pick_key(&project.common_dir, &target.key()),
        project: project.clone(),
        target,
        time,
        subject,
        base,
        request,
        haystack: Utf32String::default(),
    };
    let mut text = format!("{} {}", project.label, item.branch_text());
    if let Some((forge, request)) = &item.request {
        text.push_str(&format!(
            " {}{} {} {}",
            forge.sigil(),
            request.number,
            request.title,
            request.author
        ));
    }
    item.haystack = Utf32String::from(text.as_str());
    item
}

fn kind_order(target: &Target) -> u8 {
    match target {
        Target::Worktree { .. } => 0,
        Target::Local { .. } => 1,
        Target::Request { .. } => 2,
        Target::Remote { .. } => 3,
    }
}

pub struct Ranker {
    matcher: Matcher,
}

impl Default for Ranker {
    fn default() -> Self {
        Ranker {
            matcher: Matcher::new(nucleo_matcher::Config::DEFAULT),
        }
    }
}

impl Ranker {
    /// Indices of the rows to show, best first.
    ///
    /// Without a query and scope: worktrees, base branches and previously
    /// picked targets, most recent first. Scoped to a project: all of its
    /// rows, worktrees first. With a query: every matching row; recency and
    /// kind add a bonus smaller than a few matched characters.
    pub fn rank(
        &mut self,
        items: &[Item],
        query: &str,
        scope: Option<&Path>,
        picks: &BTreeMap<String, Timestamp>,
        now: Timestamp,
    ) -> Vec<usize> {
        let recency = |item: &Item| picks.get(&item.key).copied().max(item.time);
        let in_scope = |item: &Item| scope.is_none_or(|scope| item.project.common_dir == scope);

        if query.trim().is_empty() {
            let mut shown: Vec<usize> = (0..items.len())
                .filter(|&index| {
                    let item = &items[index];
                    in_scope(item)
                        && (scope.is_some()
                            || matches!(item.target, Target::Worktree { .. })
                            || (item.base && matches!(item.target, Target::Local { .. }))
                            || picks.contains_key(&item.key))
                })
                .collect();
            shown.sort_by(|&a, &b| {
                let (a, b) = (&items[a], &items[b]);
                let kind = |item: &Item| scope.map(|_| kind_order(&item.target));
                kind(a)
                    .cmp(&kind(b))
                    .then(recency(b).cmp(&recency(a)))
                    .then(a.project.label.cmp(&b.project.label))
            });
            return shown;
        }

        let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
        let mut scored: Vec<(u32, Option<Timestamp>, usize)> = items
            .iter()
            .enumerate()
            .filter(|(_, item)| in_scope(item))
            .filter_map(|(index, item)| {
                let score = pattern.score(item.haystack.slice(..), &mut self.matcher)?;
                let age = recency(item).map(|time| now - time);
                let bonus = recency_bonus(age) + kind_bonus(&item.target);
                Some((score + bonus, recency(item), index))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
        scored.into_iter().map(|(_, _, index)| index).collect()
    }
}

/// A matched character scores about 16, so these only reorder close matches.
fn recency_bonus(age: Option<Timestamp>) -> u32 {
    match age {
        Some(age) if age < 86_400 => 24,
        Some(age) if age < 7 * 86_400 => 16,
        Some(age) if age < 30 * 86_400 => 8,
        _ => 0,
    }
}

fn kind_bonus(target: &Target) -> u32 {
    match target {
        Target::Worktree { .. } => 8,
        Target::Local { .. } => 4,
        Target::Request { .. } => 2,
        Target::Remote { .. } => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pick::{Entry, Remote};

    const NOW: Timestamp = 1_000_000_000;

    fn listing(label: &str, entries: Vec<(Target, Timestamp, bool)>) -> Listing {
        Listing {
            project: Project {
                common_dir: format!("/{label}/.git").into(),
                root: "/".into(),
                label: label.into(),
                main_path: format!("/{label}").into(),
                remotes: vec![Remote {
                    name: "origin".into(),
                    url: None,
                }],
            },
            entries: entries
                .into_iter()
                .map(|(target, age, base)| Entry {
                    target,
                    time: Some(NOW - age),
                    subject: None,
                    base,
                })
                .collect(),
        }
    }

    fn worktree(label: &str, branch: &str) -> Target {
        Target::Worktree {
            path: format!("/{label}.{branch}").into(),
            branch: Some(branch.into()),
        }
    }

    fn fixture() -> Vec<Item> {
        let charts = listing(
            "charts",
            vec![
                (worktree("charts", "main"), 50, true),
                (worktree("charts", "feat"), 10, false),
                (
                    Target::Local {
                        branch: "develop".into(),
                    },
                    99,
                    true,
                ),
                (
                    Target::Local {
                        branch: "old".into(),
                    },
                    5,
                    false,
                ),
                (
                    Target::Remote {
                        remote: "origin".into(),
                        branch: "review-me".into(),
                    },
                    1,
                    false,
                ),
            ],
        );
        let api = listing("api", vec![(worktree("api", "main"), 20, true)]);
        let mut items = items(&charts, None);
        items.extend(super::items(&api, None));
        items
    }

    fn names(items: &[Item], order: &[usize]) -> Vec<String> {
        order
            .iter()
            .map(|&index| {
                format!(
                    "{} {}",
                    items[index].project.label,
                    items[index].branch_text()
                )
            })
            .collect()
    }

    #[test]
    fn empty_query_shows_worktrees_and_bases_by_recency() {
        let items = fixture();
        let order = Ranker::default().rank(&items, "", None, &BTreeMap::new(), NOW);
        assert_eq!(
            names(&items, &order),
            ["charts feat", "api main", "charts main", "charts develop"]
        );
    }

    #[test]
    fn scope_shows_every_row_of_the_project_worktrees_first() {
        let items = fixture();
        let scope = Path::new("/charts/.git");
        let order = Ranker::default().rank(&items, "", Some(scope), &BTreeMap::new(), NOW);
        assert_eq!(
            names(&items, &order),
            [
                "charts feat",
                "charts main",
                "charts old",
                "charts develop",
                "charts origin/review-me"
            ]
        );
    }

    #[test]
    fn query_reaches_remote_branches_and_picks_rank_up() {
        let items = fixture();
        let order = Ranker::default().rank(&items, "review", None, &BTreeMap::new(), NOW);
        assert_eq!(names(&items, &order), ["charts origin/review-me"]);

        let mut picks = BTreeMap::new();
        let api_main = items
            .iter()
            .find(|item| item.project.label == "api")
            .unwrap();
        picks.insert(api_main.key.clone(), NOW);
        let order = Ranker::default().rank(&items, "", None, &picks, NOW);
        assert_eq!(names(&items, &order)[0], "api main");
    }
}
