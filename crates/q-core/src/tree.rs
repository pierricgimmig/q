//! Dependency trees.
//!
//! Children are tasks the parent depends on. The store loads rows and calls
//! these functions; it does not render them.

use std::collections::{HashMap, HashSet};

use crate::{QueueError, TaskStatus, TaskTree, TreeFeature, TreeNode};

/// Flat task row used to build a [`TaskTree`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeTask {
    pub id: i64,
    pub title: String,
    pub status: TaskStatus,
    pub project: Option<String>,
    pub feature_id: Option<i64>,
    pub feature: Option<String>,
}

/// Tree rooted at `root_id`. Children are dependencies of the parent.
pub fn build_task_tree(
    tasks: &[TreeTask],
    edges: &[(i64, i64)],
    root_id: i64,
) -> Result<TaskTree, QueueError> {
    let index = index_tasks(tasks);
    if !index.contains_key(&root_id) {
        return Err(QueueError::NotFound(root_id));
    }
    let deps = dep_map(edges);
    let mut path = HashSet::new();
    let mut shown = HashSet::new();
    let Some(root) = walk(root_id, None, &index, &deps, &mut path, &mut shown) else {
        return Err(QueueError::NotFound(root_id));
    };
    Ok(TaskTree {
        feature: None,
        roots: vec![root],
    })
}

/// Forest of tasks in `feature_id`.
///
/// A root is a task in the feature that no other task in the feature depends
/// on. Tasks outside the feature are included only as dependencies and marked
/// [`TreeNode::external`].
pub fn build_feature_forest(
    tasks: &[TreeTask],
    edges: &[(i64, i64)],
    feature_id: i64,
    feature_title: &str,
) -> TaskTree {
    let index = index_tasks(tasks);
    let deps = dep_map(edges);
    let members: HashSet<i64> = tasks
        .iter()
        .filter(|task| task.feature_id == Some(feature_id))
        .map(|task| task.id)
        .collect();

    let mut depended_on = HashSet::new();
    for (&task_id, children) in &deps {
        if !members.contains(&task_id) {
            continue;
        }
        for child in children {
            if members.contains(child) {
                depended_on.insert(*child);
            }
        }
    }
    let mut roots: Vec<i64> = members
        .iter()
        .copied()
        .filter(|id| !depended_on.contains(id))
        .collect();

    let mut covered = HashSet::new();
    for id in &roots {
        let mut path = HashSet::new();
        cover_members(*id, &members, &deps, &mut path, &mut covered);
    }
    let mut leftover: Vec<i64> = members
        .iter()
        .copied()
        .filter(|id| !covered.contains(id))
        .collect();
    leftover.sort_unstable();
    for id in leftover {
        if covered.contains(&id) {
            continue;
        }
        roots.push(id);
        let mut path = HashSet::new();
        cover_members(id, &members, &deps, &mut path, &mut covered);
    }
    roots.sort_unstable();
    roots.dedup();

    let mut path = HashSet::new();
    let mut shown = HashSet::new();
    let mut nodes = Vec::new();
    for id in roots {
        if let Some(node) = walk(id, Some(feature_id), &index, &deps, &mut path, &mut shown) {
            nodes.push(node);
        }
    }
    TaskTree {
        feature: Some(TreeFeature {
            id: feature_id,
            title: feature_title.to_string(),
        }),
        roots: nodes,
    }
}

fn index_tasks(tasks: &[TreeTask]) -> HashMap<i64, &TreeTask> {
    tasks.iter().map(|task| (task.id, task)).collect()
}

fn dep_map(edges: &[(i64, i64)]) -> HashMap<i64, Vec<i64>> {
    let mut map: HashMap<i64, Vec<i64>> = HashMap::new();
    for &(task_id, depends_on) in edges {
        let children = map.entry(task_id).or_default();
        if !children.contains(&depends_on) {
            children.push(depends_on);
        }
    }
    for children in map.values_mut() {
        children.sort_unstable();
    }
    map
}

fn cover_members(
    id: i64,
    members: &HashSet<i64>,
    deps: &HashMap<i64, Vec<i64>>,
    path: &mut HashSet<i64>,
    covered: &mut HashSet<i64>,
) {
    if !members.contains(&id) || path.contains(&id) || covered.contains(&id) {
        return;
    }
    path.insert(id);
    covered.insert(id);
    if let Some(children) = deps.get(&id) {
        for child in children {
            cover_members(*child, members, deps, path, covered);
        }
    }
    path.remove(&id);
}

fn walk(
    id: i64,
    scope: Option<i64>,
    index: &HashMap<i64, &TreeTask>,
    deps: &HashMap<i64, Vec<i64>>,
    path: &mut HashSet<i64>,
    shown: &mut HashSet<i64>,
) -> Option<TreeNode> {
    let task = index.get(&id)?;
    let external = scope.is_some_and(|feature_id| task.feature_id != Some(feature_id));
    if path.contains(&id) {
        return Some(node_from(task, external, false, true, Vec::new()));
    }
    if shown.contains(&id) {
        return Some(node_from(task, external, true, false, Vec::new()));
    }
    path.insert(id);
    let mut depends_on = Vec::new();
    if let Some(children) = deps.get(&id) {
        for child in children {
            if let Some(node) = walk(*child, scope, index, deps, path, shown) {
                depends_on.push(node);
            }
        }
    }
    path.remove(&id);
    shown.insert(id);
    Some(node_from(task, external, false, false, depends_on))
}

fn node_from(
    task: &TreeTask,
    external: bool,
    already_shown: bool,
    cycle: bool,
    depends_on: Vec<TreeNode>,
) -> TreeNode {
    TreeNode {
        id: task.id,
        status: task.status,
        title: task.title.clone(),
        project: task.project.clone(),
        feature_id: task.feature_id,
        feature: task.feature.clone(),
        external,
        already_shown,
        cycle,
        depends_on,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: i64, status: TaskStatus, feature_id: Option<i64>) -> TreeTask {
        TreeTask {
            id,
            title: format!("task {id}"),
            status,
            project: Some(format!("proj {id}")),
            feature_id,
            feature: feature_id.map(|feature| format!("feature {feature}")),
        }
    }

    fn ids(nodes: &[TreeNode]) -> Vec<i64> {
        nodes.iter().map(|node| node.id).collect()
    }

    #[test]
    fn chain_lists_dependencies_under_the_parent() {
        let tasks = vec![
            task(1, TaskStatus::Done, None),
            task(2, TaskStatus::Ready, None),
            task(3, TaskStatus::Inbox, None),
        ];
        let edges = vec![(3, 2), (2, 1)];
        let tree = build_task_tree(&tasks, &edges, 3).unwrap();
        assert!(tree.feature.is_none());
        assert_eq!(tree.roots.len(), 1);
        let root = &tree.roots[0];
        assert_eq!(root.id, 3);
        assert_eq!(root.status, TaskStatus::Inbox);
        assert_eq!(ids(&root.depends_on), vec![2]);
        assert_eq!(root.depends_on[0].status, TaskStatus::Ready);
        assert_eq!(ids(&root.depends_on[0].depends_on), vec![1]);
        assert_eq!(root.depends_on[0].depends_on[0].status, TaskStatus::Done);
        assert!(root.depends_on[0].depends_on[0].depends_on.is_empty());
        assert!(!root.external);
    }

    #[test]
    fn diamond_expands_a_shared_dependency_once() {
        let tasks = vec![
            task(1, TaskStatus::Inbox, None),
            task(2, TaskStatus::Inbox, None),
            task(3, TaskStatus::Inbox, None),
            task(4, TaskStatus::Done, None),
        ];
        let edges = vec![(1, 2), (1, 3), (2, 4), (3, 4)];
        let tree = build_task_tree(&tasks, &edges, 1).unwrap();
        let root = &tree.roots[0];
        assert_eq!(ids(&root.depends_on), vec![2, 3]);
        let first = &root.depends_on[0].depends_on[0];
        assert_eq!(first.id, 4);
        assert!(!first.already_shown);
        assert!(first.depends_on.is_empty());
        let second = &root.depends_on[1].depends_on[0];
        assert_eq!(second.id, 4);
        assert!(second.already_shown);
        assert!(!second.cycle);
        assert!(second.depends_on.is_empty());

        let value = serde_json::to_value(second).unwrap();
        assert_eq!(value["already_shown"], true);
        assert!(value.get("external").is_none());
        assert!(value.get("cycle").is_none());
        assert!(value["depends_on"].as_array().unwrap().is_empty());
    }

    #[test]
    fn cycle_stops_without_looping() {
        let tasks = vec![
            task(1, TaskStatus::Inbox, None),
            task(2, TaskStatus::Blocked, None),
        ];
        let edges = vec![(1, 2), (2, 1), (1, 1)];
        let tree = build_task_tree(&tasks, &edges, 1).unwrap();
        let root = &tree.roots[0];
        assert_eq!(ids(&root.depends_on), vec![1, 2]);
        assert!(root.depends_on[0].cycle);
        assert!(root.depends_on[0].depends_on.is_empty());
        let child = &root.depends_on[1];
        assert_eq!(child.id, 2);
        assert_eq!(child.depends_on.len(), 1);
        assert_eq!(child.depends_on[0].id, 1);
        assert!(child.depends_on[0].cycle);
        assert!(child.depends_on[0].depends_on.is_empty());
    }

    #[test]
    fn missing_task_is_not_found() {
        let err = build_task_tree(&[], &[], 9).unwrap_err();
        assert!(matches!(err, QueueError::NotFound(9)));
    }

    #[test]
    fn feature_forest_keeps_isolated_tasks_and_marks_external_deps() {
        let mut shared = task(1, TaskStatus::Done, Some(8));
        shared.title = "Shared schema".into();
        shared.project = Some("db".into());
        shared.feature = Some("Other".into());
        let mut leaf = task(2, TaskStatus::Inbox, Some(7));
        leaf.title = "Add the types".into();
        let mut mid = task(3, TaskStatus::Ready, Some(7));
        mid.title = "Write the schema".into();
        let mut top = task(4, TaskStatus::Inbox, Some(7));
        top.title = "Ship the rollout".into();
        top.project = Some("api".into());
        let mut notes = task(5, TaskStatus::Inbox, Some(7));
        notes.title = "Write the notes".into();
        notes.project = Some("web".into());
        let mut outsider = task(9, TaskStatus::Inbox, Some(8));
        outsider.title = "Ignore me".into();

        let tasks = vec![shared, leaf, mid, top, notes, outsider];
        let edges = vec![(4, 1), (4, 3), (3, 2), (1, 2)];
        let tree = build_feature_forest(&tasks, &edges, 7, "Rollout");
        assert_eq!(tree.feature.as_ref().unwrap().title, "Rollout");
        assert_eq!(ids(&tree.roots), vec![4, 5]);

        let ship = &tree.roots[0];
        assert!(!ship.external);
        assert_eq!(ids(&ship.depends_on), vec![1, 3]);
        let external = &ship.depends_on[0];
        assert_eq!(external.id, 1);
        assert!(external.external);
        assert_eq!(external.feature.as_deref(), Some("Other"));
        assert_eq!(ids(&external.depends_on), vec![2]);
        assert!(!external.depends_on[0].external);
        assert!(!external.depends_on[0].already_shown);

        let schema = &ship.depends_on[1];
        assert_eq!(schema.id, 3);
        assert!(!schema.external);
        assert_eq!(ids(&schema.depends_on), vec![2]);
        assert!(schema.depends_on[0].already_shown);

        assert_eq!(tree.roots[1].id, 5);
        assert!(tree.roots[1].depends_on.is_empty());
        assert!(!tree.roots.iter().any(|node| node.id == 9));
    }

    #[test]
    fn empty_feature_is_an_empty_forest() {
        let tasks = vec![task(1, TaskStatus::Inbox, Some(2))];
        let tree = build_feature_forest(&tasks, &[], 4, "Empty");
        assert!(tree.roots.is_empty());
        assert_eq!(tree.feature.unwrap().id, 4);
    }

    #[test]
    fn feature_cycle_is_still_shown() {
        let tasks = vec![
            task(1, TaskStatus::Inbox, Some(3)),
            task(2, TaskStatus::Inbox, Some(3)),
        ];
        let edges = vec![(1, 2), (2, 1)];
        let tree = build_feature_forest(&tasks, &edges, 3, "Loop");
        assert_eq!(ids(&tree.roots), vec![1]);
        assert_eq!(tree.roots[0].depends_on[0].id, 2);
        assert!(tree.roots[0].depends_on[0].depends_on[0].cycle);
    }

    #[test]
    fn json_omits_unset_feature_and_false_flags() {
        let tasks = vec![task(1, TaskStatus::Inbox, None)];
        let tree = build_task_tree(&tasks, &[], 1).unwrap();
        let value = serde_json::to_value(&tree).unwrap();
        assert!(value.get("feature").is_none());
        assert_eq!(value["roots"][0]["id"], 1);
        assert_eq!(value["roots"][0]["status"], "inbox");
        assert!(value["roots"][0].get("external").is_none());
        assert!(value["roots"][0].get("already_shown").is_none());
        assert!(value["roots"][0]["depends_on"]
            .as_array()
            .unwrap()
            .is_empty());
    }
}
