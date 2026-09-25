//! blockedBy graph helpers: cycle detection + reverse-edge derivation.
//!
//! Port of upstream `packages/rpiv-todo/state/task-graph.ts` @ `0fdf4f8`.

use std::collections::{HashMap, HashSet};

use crate::tool::types::Task;

/// Detect whether merging `new_blocked_by` into `task_id`'s `blocked_by`
/// set would introduce a cycle in the dependency graph.
///
/// Pure of any module state; takes the existing `task_list` and the
/// proposed additions explicitly so the reducer can ask "would this update
/// cycle?" without mutating state first.
pub fn detect_cycle(task_list: &[Task], task_id: i64, new_blocked_by: &[i64]) -> bool {
    let mut edges: HashMap<i64, Vec<i64>> = HashMap::new();
    for task in task_list {
        if task.id == task_id {
            // Merge existing + proposed additions, deduplicated (upstream
            // `new Set([...(t.blockedBy ?? []), ...newBlockedBy])`).
            let mut merged: Vec<i64> = Vec::new();
            let mut seen: HashSet<i64> = HashSet::new();
            for dep in task
                .blocked_by
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .copied()
                .chain(new_blocked_by.iter().copied())
            {
                if seen.insert(dep) {
                    merged.push(dep);
                }
            }
            edges.insert(task.id, merged);
        } else {
            edges.insert(task.id, task.blocked_by.clone().unwrap_or_default());
        }
    }

    let mut visiting: HashSet<i64> = HashSet::new();
    let mut visited: HashSet<i64> = HashSet::new();

    fn has_cycle_from(
        edges: &HashMap<i64, Vec<i64>>,
        node: i64,
        visiting: &mut HashSet<i64>,
        visited: &mut HashSet<i64>,
    ) -> bool {
        if visiting.contains(&node) {
            return true;
        }
        if visited.contains(&node) {
            return false;
        }
        visiting.insert(node);
        if let Some(neighbors) = edges.get(&node) {
            for next in neighbors {
                if has_cycle_from(edges, *next, visiting, visited) {
                    return true;
                }
            }
        }
        visiting.remove(&node);
        visited.insert(node);
        false
    }

    // Iterate the SAME edge keys the upstream `for (const node of
    // edges.keys())` walk visits (insertion order = task list order plus
    // the merged `task_id` entry).
    edges
        .keys()
        .any(|node| has_cycle_from(&edges, *node, &mut visiting, &mut visited))
}

/// Build the inverse adjacency map: for each task `T`, which other tasks
/// list `T` in their `blocked_by`. Consumed by the `get` action's
/// `blocks: #x, #y` suffix line (overlay gating joins with TE35).
pub fn derive_blocks(task_list: &[Task]) -> HashMap<i64, Vec<i64>> {
    let mut blocks: HashMap<i64, Vec<i64>> = HashMap::new();
    for task in task_list {
        for dep in task.blocked_by.as_deref().unwrap_or(&[]) {
            blocks.entry(*dep).or_default().push(task.id);
        }
    }
    blocks
}

#[cfg(test)]
mod tests {
    //! Port of upstream `state/task-graph.test.ts` @ `0fdf4f8`.

    use super::*;
    use crate::tool::types::{Task, TaskStatus};

    fn task(id: i64, subject: &str, blocked_by: Option<Vec<i64>>) -> Task {
        Task {
            id,
            subject: subject.to_owned(),
            status: TaskStatus::Pending,
            description: None,
            active_form: None,
            blocked_by,
            owner: None,
            metadata: None,
        }
    }

    #[test]
    fn detects_direct_cycle() {
        let tasks = vec![task(1, "a", None), task(2, "b", Some(vec![1]))];
        assert!(detect_cycle(&tasks, 1, &[2]));
    }

    #[test]
    fn returns_false_for_acyclic_graph() {
        let tasks = vec![task(1, "a", None), task(2, "b", Some(vec![1]))];
        assert!(!detect_cycle(&tasks, 2, &[1]));
    }

    #[test]
    fn empty_map_when_no_task_has_blocked_by() {
        let tasks = vec![task(1, "a", None), task(2, "b", None)];
        assert!(derive_blocks(&tasks).is_empty());
    }

    #[test]
    fn inverts_blocked_by_into_a_blocks_map() {
        let tasks = vec![
            task(1, "root", None),
            task(2, "dep", Some(vec![1])),
            task(3, "dep2", Some(vec![1, 2])),
        ];
        let blocks = derive_blocks(&tasks);
        assert_eq!(blocks.get(&1), Some(&vec![2, 3]));
        assert_eq!(blocks.get(&2), Some(&vec![3]));
        assert!(!blocks.contains_key(&3));
    }
}
