use std::{
    borrow::Borrow,
    collections::{HashMap, HashSet},
    error::Error,
    fmt::{Debug, Display},
    hash::Hash,
};

#[allow(unused_imports)]
use log::debug;

pub trait GraphNode {
    type I: Hash + Eq + Clone;

    // Identifier for a node, unique among nodes in the set under consideration.
    fn id(&self) -> impl Borrow<Self::I>;
    // IDs of nodes that have an edge from this node to that node.
    fn child_ids(&self) -> Vec<impl Borrow<Self::I>>;
}

// Ajacency-list for a directed acyclic "graph" (dunno maybe incorrect
// terminology, it doesn't make any promises about connectedness so it might be
// zero or several actual "graphs"), where nodes are identified with a usize.
#[derive(Debug)]
pub struct Dag<G: GraphNode> {
    nodes: Vec<G>,
    // maps ids that nodes know about themselves to their index in `nodes`.
    id_to_idx: HashMap<G::I, usize>,
    // edges[i] contains the destinations of the edges originating from node i.
    edges: Vec<Vec<usize>>,
    // indexes of nodes that aren't anyones child.
    root_idxs: HashSet<usize>,
}

#[derive(Debug, Eq, PartialEq)]
pub enum DagError<I> {
    // Two nodes had the same ID
    DuplicateId(I),
    // Node identified by `parent` referred to `child`, but the latter didn't exist.
    NoSuchChild { parent: I, child: I },
    // A cycle existed containing the node with this ID,
    Cycle(I),
}

impl<I: Debug> Display for DagError<I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        match self {
            Self::DuplicateId(id) => write!(f, "duplicate key {:?}", id),
            Self::NoSuchChild { parent, child } => {
                write!(f, "{:?} refers to nonexistent {:?}", parent, child)
            }
            Self::Cycle(id) => write!(f, "cycle in graph, containing {:?}", id),
        }
    }
}

impl<I: Debug> Error for DagError<I> {}

impl<G: GraphNode> Dag<G> {
    pub fn empty() -> Self {
        Self {
            nodes: Vec::new(),
            id_to_idx: HashMap::new(),
            edges: Vec::new(),
            root_idxs: HashSet::new(),
        }
    }

    pub fn new(nodes: impl IntoIterator<Item = G>) -> Result<Self, DagError<G::I>>
    {
        let nodes: Vec<G> = nodes.into_iter().collect();

        // We eventually wanna have a vector and just index it by an integer, so
        // start by mapping the arbitrary "node IDs" to vec indexes.
        // At this point we also reject duplicates (this is why we don't just
        // wanna use `ollect`).
        let mut id_to_idx = HashMap::new();
        for (idx, node) in nodes.iter().enumerate() {
            let id = node.id();
            let id = id.borrow();
            if id_to_idx.contains_key(id) {
                return Err(DagError::DuplicateId(id.clone()));
            }
            id_to_idx.insert(id.clone(), idx);
        }

        // Now build the adjacency list.
        let mut edges = Vec::new();
        for (idx, node) in nodes.iter().enumerate() {
            if idx >= edges.len() {
                edges.resize(idx + 1, Vec::new())
            }
            for child_id in node.child_ids() {
                let child_idx =
                    id_to_idx
                        .get(child_id.borrow())
                        .ok_or_else(|| DagError::NoSuchChild {
                            parent: node.id().borrow().clone(),
                            child: child_id.borrow().clone(),
                        })?;
                edges[idx].push(*child_idx);
            }
        }

        // Now we validate the DAG (no cycles) and find root nodes.
        // Root nodes are those with no edges pointing to them.
        let mut root_idxs: HashSet<usize> = (0..edges.len()).collect();
        // This set is just used to avoid duplicating work.
        let mut visited: HashSet<usize> = HashSet::new();
        // This one actually detects cycles.
        let mut visited_stack: HashSet<usize> = HashSet::new();
        // This is a bit annoying in Rust because you cannot capture
        // environments into a named function but you cannot recurse into a
        // closure, so we just have to pass everything through args explicitly.
        // Returns the index of a node which was found to be part of a cycle
        // (in that case root_idxs won't be valid and we must bail).
        fn recurse(
            visited: &mut HashSet<usize>,
            visited_stack: &mut HashSet<usize>,
            start_idx: usize,
            edges: &Vec<Vec<usize>>,
            // Nodes will be removed from here if they are found to be another
            // node's child.
            root_idxs: &mut HashSet<usize>,
        ) -> Option<usize> {
            if visited_stack.contains(&start_idx) {
                return Some(start_idx);
            }
            if visited.contains(&start_idx) {
                // Already explored from this node and found no cycles.
                return None;
            }
            visited.insert(start_idx);
            visited_stack.insert(start_idx);
            for child in &edges[start_idx] {
                root_idxs.remove(child);
                if let Some(i) = recurse(visited, visited_stack, *child, edges, root_idxs) {
                    return Some(i);
                }
            }
            visited_stack.remove(&start_idx);
            None
        }
        for i in 0..edges.len() {
            if let Some(node_in_cycle) =
                recurse(&mut visited, &mut visited_stack, i, &edges, &mut root_idxs)
            {
                return Err(DagError::Cycle(nodes[node_in_cycle].id().borrow().clone()));
            }
        }

        Ok(Self {
            nodes,
            edges,
            id_to_idx,
            root_idxs: root_idxs.into_iter().collect(),
        })
    }

    // Return a new graph with a node added
    pub fn with_node(mut self, node: G) -> Result<Self, DagError<G::I>> {
        let new_idx = self.nodes.len();
        self.id_to_idx.insert(node.id().borrow().clone(), new_idx);
        self.edges.push(
            node.child_ids()
                .into_iter()
                .map(|id| {
                    self.id_to_idx
                        .get(id.borrow())
                        .ok_or(DagError::NoSuchChild {
                            parent: node.id().borrow().clone(),
                            child: id.borrow().clone(),
                        })
                        .copied()
                })
                .collect::<Result<Vec<_>, DagError<G::I>>>()?,
        );
        for child_id in node.child_ids() {
            self.root_idxs.remove(&self.id_to_idx[child_id.borrow()]);
        }
        self.root_idxs.insert(new_idx);
        self.nodes.push(node);
        Ok(self)
    }

    // Iterate over nodes, visiting children before their parents.
    pub fn bottom_up(&self) -> BottomUp<'_, G> {
        BottomUp {
            dag: self,
            visit_stack: Vec::new(),
            unvisited_roots: self.root_idxs.iter().copied().collect(),
        }
    }

    pub fn nodes(&self) -> impl Iterator<Item = &G> + Clone {
        self.nodes.iter()
    }

    pub fn node(&self, id: &G::I) -> Option<&G> {
        // TODO this is dumb lol get rid of id_to_idx
        Some(&self.nodes[*self.id_to_idx.get(id.borrow())?])
    }

    // Iterate all the descendants of the relevant node, visiting parents before
    // their children.
    pub fn top_down_from(&self, id: &G::I) -> Option<TopDown<G>> {
        Some(TopDown {
            dag: self,
            visit_stack: Vec::new(),
            unvisited_roots: vec![*self.id_to_idx.get(id.borrow())?],
        })
    }
}

#[derive(Clone)]
pub struct BottomUp<'a, G: GraphNode> {
    dag: &'a Dag<G>,
    visit_stack: Vec<usize>,
    unvisited_roots: Vec<usize>,
}

impl<'a, G: GraphNode> Iterator for BottomUp<'a, G> {
    type Item = &'a G;

    fn next(&mut self) -> Option<&'a G> {
        // I found the basic non-recursive DFS post-order algorithm here:
        // https://codingots.medium.com/tree-traversal-without-recursion-221cbea6d004
        // This is a translation of that, where "s1" is temp_stack and s2 is
        // self.visit_stack. In that version there is only one root node but
        // here we have several.
        // First phase is to build up the stack of nodes to visit using
        // temp_stack as an intermediate.
        if self.visit_stack.is_empty() {
            let mut temp_stack = vec![self.unvisited_roots.pop()?];
            while let Some(cur_idx) = temp_stack.pop() {
                self.visit_stack.push(cur_idx);
                for child_idx in &self.dag.edges[cur_idx] {
                    temp_stack.push(*child_idx);
                }
            }
        }
        // Now we just cruise down the to_visit stack drinking a large bottle of
        // cranberry juice listening to Fleetwood Mac.
        Some(&self.dag.nodes[self.visit_stack.pop().unwrap()])
    }
}

#[derive(Clone)]
pub struct TopDown<'a, G: GraphNode> {
    dag: &'a Dag<G>,
    visit_stack: Vec<usize>,
    // Note these "roots" don't have to be roots in the overall DAG, they are
    // only roots of the top-down traversals we're doing.
    unvisited_roots: Vec<usize>,
}

impl<'a, G: GraphNode> Iterator for TopDown<'a, G> {
    type Item = &'a G;

    fn next(&mut self) -> Option<&'a G> {
        if self.visit_stack.is_empty() {
            self.visit_stack.push(self.unvisited_roots.pop()?)
        }
        let cur_idx = self.visit_stack.pop().unwrap();
        for child_idx in &self.dag.edges[cur_idx] {
            self.visit_stack.push(*child_idx);
        }
        Some(&self.dag.nodes[cur_idx])
    }
}

#[cfg(test)]
mod tests {
    use std::hash::RandomState;

    use test_case::test_case;

    use super::*;

    // We don't have any actual need to clone these but for weird Rust reasons
    // (https://users.rust-lang.org/t/unnecessary-trait-bound-requirement-for-clone/110045)
    // the Clone implementation derived for BottomUp has a bound that the graph
    // node type is Clone.
    #[derive(Debug, Eq, PartialEq, Hash, Clone)]
    struct TestGraphNode {
        id: usize,
        child_ids: Vec<usize>,
    }

    impl GraphNode for TestGraphNode {
        type I = usize;

        fn id(&self) -> impl Borrow<usize> {
            self.id
        }

        fn child_ids(&self) -> Vec<impl Borrow<usize>> {
            self.child_ids.iter().collect()
        }
    }

    fn nodes(edges: impl IntoIterator<Item = Vec<usize>>) -> Vec<TestGraphNode> {
        edges
            .into_iter()
            .enumerate()
            .map(|(id, child_ids)| TestGraphNode { id, child_ids })
            .collect()
    }

    #[test_case(vec![], None; "empty")]
    #[test_case(nodes([vec![1], vec![]]), None; "one edge")]
    #[test_case(nodes([vec![1], vec![2, 3], vec![], vec![]]), None; "tree")]
    #[test_case(nodes([vec![1], vec![2, 3], vec![], vec![],
                       vec![5], vec![6, 7], vec![], vec![]]), None; "trees")]
    #[test_case(nodes([vec![0]]), Some(DagError::Cycle(0)); "self-link")]
    // Note we don't actually care that the Cycle is reported on node 0, but
    // luckily that's stable behaviour so it's just easy to assert it that way.
    #[test_case(nodes([vec![1], vec![2], vec![3], vec![0]]), Some(DagError::Cycle(0)); "a loop")]
    #[test_case(nodes([vec![1]]), Some(DagError::NoSuchChild{parent: 0, child: 1}); "no child")]
    fn test_graph_validity(edges: Vec<TestGraphNode>, want_err: Option<DagError<usize>>) {
        assert_eq!(Dag::new(edges).err(), want_err);
    }

    #[test_case(vec![]; "empty")]
    #[test_case(nodes([vec![1], vec![]]); "one edge")]
    #[test_case(nodes([vec![1], vec![2, 3], vec![], vec![]]); "tree")]
    #[test_case(nodes([vec![1], vec![2, 3], vec![], vec![],
                       vec![5], vec![6, 7], vec![], vec![]]); "trees")]
    fn test_bottom_up(edges: Vec<TestGraphNode>) {
        let all_nodes: HashSet<usize, RandomState> = HashSet::from_iter(0..edges.len());
        let dag = Dag::new(edges).unwrap();
        let order = dag.bottom_up();
        // For bottom_up, the order of iteration is only stable within a
        // connected component so we need to be slightly clever here instead of
        // asserting hard-coded values.
        assert_eq!(
            all_nodes,
            HashSet::from_iter(order.clone().map(|node| node.id)),
            "Not all nodes visited"
        );
        let mut seen: HashSet<usize> = HashSet::new();
        for node in order {
            for child_id in node.child_ids() {
                assert!(
                    seen.contains(child_id.borrow()),
                    "Parent visited before child"
                );
            }
            seen.insert(*node.id().borrow());
        }
    }

    #[test_case(nodes([vec![1], vec![]]), 0, vec![0, 1]; "one edge")]
    // Most of the "want" values here are just one of many possible valid
    // orders, but unlike for bottom_up we have a stable algorithm and I think
    // it would be easier to just rewrite all the test cases if the algorithm
    // changes, than have a clever (a.k.a buggy) test that tries to really just
    // assert what mattters.
    #[test_case(nodes([vec![1], vec![2, 3], vec![], vec![]]),
                0, vec![0, 1, 3, 2]; "tree")]
    #[test_case(nodes([vec![1], vec![2, 3], vec![], vec![]]),
                1, vec![1, 3, 2]; "tree non root")]
    #[test_case(nodes([vec![1], vec![2, 3], vec![], vec![],
                       vec![5], vec![6, 7], vec![], vec![]]),
                0, vec![0, 1, 3, 2]; "trees 1")]
    #[test_case(nodes([vec![1], vec![2, 3], vec![], vec![],
                       vec![5], vec![6, 7], vec![], vec![]]),
                4, vec![4, 5, 7, 6]; "trees 2")]
    fn test_top_down(edges: Vec<TestGraphNode>, from: usize, want_order: Vec<usize>) {
        let dag = Dag::new(edges).unwrap();
        let order = dag.top_down_from(&from).unwrap();
        assert_eq!(order.map(|node| node.id).collect::<Vec<_>>(), want_order);
    }
}
