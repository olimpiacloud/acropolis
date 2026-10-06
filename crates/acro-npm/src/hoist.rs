use std::collections::{BTreeMap, VecDeque};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PkgId(pub String);

#[derive(Clone, Debug, Default)]
pub struct Graph {
    pub roots: BTreeMap<String, PkgId>,
    pub deps: BTreeMap<PkgId, BTreeMap<String, PkgId>>,
}

#[derive(Debug, Default)]
struct Node {
    id: Option<PkgId>,
    parent: Option<usize>,
    children: BTreeMap<String, usize>,
}

pub fn hoist(graph: &Graph) -> BTreeMap<String, PkgId> {
    let mut nodes: Vec<Node> = vec![Node::default()];
    let mut queue: VecDeque<(usize, String, PkgId)> = VecDeque::new();
    for (name, id) in &graph.roots {
        queue.push_back((0, name.clone(), id.clone()));
    }
    while let Some((requester, name, id)) = queue.pop_front() {
        let mut chain = Vec::new();
        let mut cur = Some(requester);
        while let Some(c) = cur {
            chain.push(c);
            cur = nodes[c].parent;
        }
        chain.reverse();
        let mut start = 0;
        for (i, &n) in chain.iter().enumerate() {
            if let Some(&child) = nodes[n].children.get(&name)
                && nodes[child].id.as_ref() != Some(&id)
            {
                start = i + 1;
            }
        }
        let mut placed: Option<(usize, bool)> = None;
        for &n in &chain[start.min(chain.len())..] {
            match nodes[n].children.get(&name) {
                Some(&child) if nodes[child].id.as_ref() == Some(&id) => {
                    placed = Some((child, false));
                    break;
                }
                Some(_) => continue,
                None => {
                    let idx = nodes.len();
                    nodes.push(Node { id: Some(id.clone()), parent: Some(n), children: BTreeMap::new() });
                    nodes[n].children.insert(name.clone(), idx);
                    placed = Some((idx, true));
                    break;
                }
            }
        }
        let (idx, fresh) = match placed {
            Some(p) => p,
            None => {
                let idx = nodes.len();
                nodes.push(Node { id: Some(id.clone()), parent: Some(requester), children: BTreeMap::new() });
                nodes[requester].children.insert(name.clone(), idx);
                (idx, true)
            }
        };
        if fresh && let Some(deps) = graph.deps.get(&id) {
            for (dn, did) in deps {
                queue.push_back((idx, dn.clone(), did.clone()));
            }
        }
    }
    let mut out = BTreeMap::new();
    let mut stack: Vec<(usize, String)> = vec![(0, String::new())];
    while let Some((n, prefix)) = stack.pop() {
        for (name, &child) in &nodes[n].children {
            let path = if prefix.is_empty() {
                format!("node_modules/{name}")
            } else {
                format!("{prefix}/node_modules/{name}")
            };
            if let Some(id) = &nodes[child].id {
                out.insert(path.clone(), id.clone());
            }
            stack.push((child, path));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> PkgId {
        PkgId(s.to_string())
    }

    #[test]
    fn hoists_and_nests_conflicts() {
        let mut g = Graph::default();
        g.roots.insert("a".into(), id("a@1"));
        g.roots.insert("b".into(), id("b@1"));
        g.deps.insert(id("a@1"), [("c".to_string(), id("c@1"))].into());
        g.deps.insert(id("b@1"), [("c".to_string(), id("c@2")), ("d".to_string(), id("d@1"))].into());
        g.deps.insert(id("d@1"), [("c".to_string(), id("c@1"))].into());
        let layout = hoist(&g);
        assert_eq!(layout["node_modules/a"], id("a@1"));
        assert_eq!(layout["node_modules/c"], id("c@1"));
        assert_eq!(layout["node_modules/b/node_modules/c"], id("c@2"));
        assert_eq!(layout["node_modules/d"], id("d@1"));
        assert!(!layout.contains_key("node_modules/d/node_modules/c"));
    }

    #[test]
    fn cycles_terminate() {
        let mut g = Graph::default();
        g.roots.insert("a".into(), id("a@1"));
        g.deps.insert(id("a@1"), [("b".to_string(), id("b@1"))].into());
        g.deps.insert(id("b@1"), [("a".to_string(), id("a@1"))].into());
        let layout = hoist(&g);
        assert_eq!(layout.len(), 2);
    }
}
