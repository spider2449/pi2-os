//! Service dependency graph: validation and start order.

use std::collections::{BTreeMap, BTreeSet};

use pios_core::service::ServiceSpec;
use pios_core::{Error, Result};

/// Return service indices in an order where every dependency precedes its
/// dependents. Fails on duplicate names, unknown dependencies or cycles.
pub fn start_order(services: &[ServiceSpec]) -> Result<Vec<usize>> {
    let mut index = BTreeMap::new();
    for (i, s) in services.iter().enumerate() {
        if index.insert(s.name.as_str(), i).is_some() {
            return Err(Error::Config(format!("duplicate service '{}'", s.name)));
        }
    }
    let mut indegree = vec![0usize; services.len()];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); services.len()];
    for (i, s) in services.iter().enumerate() {
        for dep in &s.after {
            let &d = index
                .get(dep.as_str())
                .ok_or_else(|| Error::Config(format!("service '{}' depends on unknown '{}'", s.name, dep)))?;
            indegree[i] += 1;
            dependents[d].push(i);
        }
    }
    // Kahn's algorithm; BTreeSet keeps the order deterministic (by index).
    let mut ready: BTreeSet<usize> = (0..services.len()).filter(|&i| indegree[i] == 0).collect();
    let mut order = Vec::with_capacity(services.len());
    while let Some(i) = ready.pop_first() {
        order.push(i);
        for &j in &dependents[i] {
            indegree[j] -= 1;
            if indegree[j] == 0 {
                ready.insert(j);
            }
        }
    }
    if order.len() != services.len() {
        let stuck: Vec<&str> =
            (0..services.len()).filter(|i| indegree[*i] > 0).map(|i| services[i].name.as_str()).collect();
        return Err(Error::Config(format!("dependency cycle among: {}", stuck.join(", "))));
    }
    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(name: &str, after: &[&str]) -> ServiceSpec {
        ServiceSpec::new(name, &["/bin/true"]).after(after)
    }

    #[test]
    fn orders_dependencies_first() {
        let s = vec![svc("dockerd", &["containerd", "net"]), svc("containerd", &["net"]), svc("net", &[])];
        let order: Vec<&str> = start_order(&s).unwrap().into_iter().map(|i| s[i].name.as_str()).collect();
        assert_eq!(order, ["net", "containerd", "dockerd"]);
    }

    #[test]
    fn detects_cycle() {
        let s = vec![svc("a", &["b"]), svc("b", &["a"]), svc("c", &[])];
        let err = start_order(&s).unwrap_err().to_string();
        assert!(err.contains("cycle") && err.contains('a') && err.contains('b'));
    }

    #[test]
    fn detects_unknown_and_duplicate() {
        assert!(start_order(&[svc("a", &["x"])]).is_err());
        assert!(start_order(&[svc("a", &[]), svc("a", &[])]).is_err());
    }
}
