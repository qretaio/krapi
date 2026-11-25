//! THE DEPENDENCY GRAPH — a directed acyclic graph over string ids with
//! two judgments: a stable topological order (user-declared order is
//! preserved inside every wave: ties break by the resource's index in the
//! RGD, then by id) and the reverse-topological waves the deletion
//! law drains one at a time. Cycles are refused with the deterministic
//! cycle path (sorted edge walk, so the same graph always reports the
//! same cycle).

use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

/// One vertex: its position in the user's list (`order`) and the ids it
/// depends on (a dependency must appear before its dependent).
#[derive(Debug, Clone, Default)]
struct Vertex {
    order: usize,
    depends_on: BTreeSet<String>,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("graph contains a cycle: {}", format_cycle(.0))]
pub struct CycleError(pub Vec<String>);

fn format_cycle(cycle: &[String]) -> String {
    cycle.join(" -> ")
}

/// A directed acyclic graph over resource ids.
#[derive(Debug, Clone, Default)]
pub struct Dag {
    vertices: BTreeMap<String, Vertex>,
}

impl Dag {
    /// Add a vertex with its user-declared position.
    pub fn add_vertex(&mut self, id: &str, order: usize) {
        self.vertices.entry(id.to_owned()).or_default().order = order;
    }

    /// Add dependencies for `from` (every `dep` must appear before it).
    /// Self references and unknown ids are refused.
    ///
    /// # Errors
    /// An unknown dependency id, a self reference, or a dependency that
    /// closes a cycle.
    pub fn add_dependencies(&mut self, from: &str, deps: &[String]) -> Result<(), CycleError> {
        if !self.vertices.contains_key(from) {
            return Err(CycleError(vec![from.to_owned()]));
        }
        let mut added = Vec::new();
        for dep in deps {
            if dep == from {
                return Err(CycleError(vec![from.to_owned(), from.to_owned()]));
            }
            if !self.vertices.contains_key(dep) {
                return Err(CycleError(vec![from.to_owned(), dep.clone()]));
            }
            let Some(v) = self.vertices.get_mut(from) else {
                return Err(CycleError(vec![from.to_owned()]));
            };
            if v.depends_on.insert(dep.clone()) {
                added.push(dep.clone());
            }
        }
        // A new edge closes a cycle iff `from` is already reachable from
        // any added dependency along dependsOn edges.
        if let Some(path) = self.path_between(&added, from) {
            if let Some(v) = self.vertices.get_mut(from) {
                for dep in &added {
                    v.depends_on.remove(dep);
                }
            }
            let mut cycle = vec![from.to_owned()];
            cycle.extend(path);
            return Err(CycleError(cycle));
        }
        Ok(())
    }

    /// One deterministic path (sorted walk) from any of `froms` to `to`
    /// along dependsOn edges.
    fn path_between(&self, froms: &[String], to: &str) -> Option<Vec<String>> {
        let mut parent: BTreeMap<String, String> = BTreeMap::new();
        let mut stack: Vec<String> = Vec::new();
        for start in froms {
            if !parent.contains_key(start) {
                parent.insert(start.clone(), start.clone());
                stack.push(start.clone());
            }
        }
        while let Some(node) = stack.pop() {
            let deps = self
                .vertices
                .get(&node)
                .map_or_default(|v| v.depends_on.iter().cloned().collect::<Vec<_>>());
            for dep in deps {
                if dep == to {
                    let mut path = vec![to.to_owned()];
                    let mut cur = node.clone();
                    loop {
                        path.push(cur.clone());
                        let p = &parent[&cur];
                        if p == &cur {
                            break;
                        }
                        cur = p.clone();
                    }
                    path.reverse();
                    return Some(path);
                }
                if !parent.contains_key(&dep) {
                    parent.insert(dep.clone(), node.clone());
                    stack.push(dep);
                }
            }
        }
        None
    }

    /// The stable topological order: dependencies first, ties broken by
    /// (user order, id) so the RGD's list order survives everywhere the
    /// graph allows it.
    ///
    /// # Errors
    /// A cycle — refused with the deterministic cycle path.
    pub fn topological_sort(&self) -> Result<Vec<String>, CycleError> {
        let mut remaining: BTreeMap<&str, usize> = BTreeMap::new();
        let mut unlocks: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for id in self.vertices.keys() {
            remaining.insert(id, 0);
        }
        for (id, v) in &self.vertices {
            for dep in &v.depends_on {
                if let Some(r) = remaining.get_mut(id.as_str()) {
                    *r += 1;
                }
                unlocks.entry(dep.as_str()).or_default().push(id);
            }
        }
        let mut ready: BinaryHeap<std::cmp::Reverse<(usize, String)>> = BinaryHeap::new();
        for (id, &r) in &remaining {
            if r == 0 {
                ready.push(std::cmp::Reverse((
                    self.vertices[*id].order,
                    (*id).to_owned(),
                )));
            }
        }
        let mut order = Vec::with_capacity(self.vertices.len());
        while let Some(std::cmp::Reverse((_, id))) = ready.pop() {
            if let Some(unlocked) = unlocks.get(id.as_str()) {
                for u in unlocked {
                    if let Some(r) = remaining.get_mut(*u) {
                        *r -= 1;
                        if *r == 0 {
                            ready.push(std::cmp::Reverse((
                                self.vertices[*u].order,
                                (*u).to_owned(),
                            )));
                        }
                    }
                }
            }
            order.push(id.clone());
        }
        if order.len() == self.vertices.len() {
            return Ok(order);
        }
        Err(CycleError(self.find_cycle()))
    }

    /// The reverse-topological waves: dependents grouped so each wave can
    /// drain (be deleted) before the next. Waves are ordered for DELETION
    /// — dependents first, dependencies last.
    ///
    /// # Errors
    /// A cycle — refused with the deterministic cycle path.
    pub fn reverse_layers(&self) -> Result<Vec<Vec<String>>, CycleError> {
        let mut remaining: BTreeMap<&str, usize> = BTreeMap::new();
        let mut unlocks: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for id in self.vertices.keys() {
            remaining.insert(id, 0);
        }
        for (id, v) in &self.vertices {
            for dep in &v.depends_on {
                if let Some(r) = remaining.get_mut(dep.as_str()) {
                    *r += 1;
                }
                unlocks.entry(id.as_str()).or_default().push(dep);
            }
        }
        let mut ready: BinaryHeap<std::cmp::Reverse<(usize, String)>> = BinaryHeap::new();
        for (id, &r) in &remaining {
            if r == 0 {
                ready.push(std::cmp::Reverse((
                    self.vertices[*id].order,
                    (*id).to_owned(),
                )));
            }
        }
        let mut layers: Vec<Vec<String>> = Vec::new();
        while !ready.is_empty() {
            let layer_size = ready.len();
            let mut batch = Vec::with_capacity(layer_size);
            for _ in 0..layer_size {
                if let Some(std::cmp::Reverse((_, id))) = ready.pop() {
                    batch.push(id);
                }
            }
            for id in &batch {
                if let Some(unlocked) = unlocks.get(id.as_str()) {
                    for u in unlocked {
                        if let Some(r) = remaining.get_mut(*u) {
                            *r -= 1;
                            if *r == 0 {
                                ready.push(std::cmp::Reverse((
                                    self.vertices[*u].order,
                                    (*u).to_owned(),
                                )));
                            }
                        }
                    }
                }
            }
            layers.push(batch);
        }
        let processed: usize = layers.iter().map(Vec::len).sum();
        if processed == self.vertices.len() {
            return Ok(layers);
        }
        Err(CycleError(self.find_cycle()))
    }

    /// The deterministic cycle report: a sorted-DFS walk so the same
    /// graph always reports the same cycle.
    fn find_cycle(&self) -> Vec<String> {
        #[derive(Clone, Default)]
        struct Marks {
            visited: BTreeSet<String>,
            on_stack: BTreeSet<String>,
        }
        fn dfs(
            dag: &Dag,
            node: &str,
            marks: &mut Marks,
            path: &mut Vec<String>,
        ) -> Option<Vec<String>> {
            marks.visited.insert(node.to_owned());
            marks.on_stack.insert(node.to_owned());
            path.push(node.to_owned());
            let deps = dag
                .vertices
                .get(node)
                .map_or_default(|v| v.depends_on.iter().cloned().collect::<Vec<_>>());
            for dep in deps {
                if !marks.visited.contains(&dep) {
                    if let Some(c) = dfs(dag, &dep, marks, path) {
                        return Some(c);
                    }
                } else if marks.on_stack.contains(&dep) {
                    // close the cycle from the first occurrence of `dep`
                    let start = path.iter().position(|p| p == &dep)?;
                    let mut cycle: Vec<String> = path[start..].to_vec();
                    cycle.push(dep);
                    return Some(cycle);
                }
            }
            marks.on_stack.remove(node);
            path.pop();
            None
        }
        let mut marks = Marks::default();
        let mut path = Vec::new();
        for id in self.vertices.keys() {
            if !marks.visited.contains(id)
                && let Some(c) = dfs(self, id, &mut marks, &mut path)
            {
                return c;
            }
            path.clear();
        }
        Vec::new()
    }
}
