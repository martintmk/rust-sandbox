use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use cargo_metadata::{DependencyKind, Metadata, Package, PackageId, TargetKind};

use crate::config::Config;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UnstablePackage {
    pub(crate) name: String,
    pub(crate) version: cargo_metadata::semver::Version,
}

#[derive(Debug)]
pub(crate) struct Policy {
    allowed_crate_names: BTreeSet<String>,
    blocked_crates: BTreeMap<String, Vec<UnstablePackage>>,
}

impl Policy {
    pub(crate) fn for_package(metadata: &Metadata, root: &Package, config: &Config) -> Self {
        let workspace_members = metadata.workspace_members.iter().cloned().collect::<HashSet<_>>();
        let (reachable, aliases) = reachable_packages(metadata, &root.id);
        let mut candidates = BTreeMap::<String, Vec<(&Package, bool)>>::new();

        for package in metadata
            .packages
            .iter()
            .filter(|package| package.id != root.id && reachable.contains(&package.id))
        {
            let Some(lib_target) = package.targets.iter().find(|target| target.kind.contains(&TargetKind::Lib)) else {
                continue;
            };

            let allowed = workspace_members.contains(&package.id) || is_stable_version(&package.version) || config.allows(&package.name);
            let mut crate_names = BTreeSet::from([lib_target.name.clone()]);
            if let Some(package_aliases) = aliases.get(&package.id) {
                crate_names.extend(package_aliases.iter().cloned());
            }

            for crate_name in crate_names {
                candidates.entry(crate_name).or_default().push((package, allowed));
            }
        }

        let mut allowed_crate_names = BTreeSet::new();
        let mut blocked_crates = BTreeMap::new();
        for (crate_name, packages) in candidates {
            if packages.iter().all(|(_, allowed)| *allowed) {
                allowed_crate_names.insert(crate_name);
            } else {
                let mut unstable = packages
                    .into_iter()
                    .filter(|(_, allowed)| !allowed)
                    .map(|(package, _)| UnstablePackage {
                        name: package.name.clone(),
                        version: package.version.clone(),
                    })
                    .collect::<Vec<_>>();
                unstable.sort_by(|left, right| (&left.name, &left.version).cmp(&(&right.name, &right.version)));
                unstable.dedup();
                blocked_crates.insert(crate_name, unstable);
            }
        }

        Self {
            allowed_crate_names,
            blocked_crates,
        }
    }

    pub(crate) fn external_types_config(&self) -> cargo_check_external_types::config::Config {
        cargo_check_external_types::config::Config {
            allowed_external_types: self
                .allowed_crate_names
                .iter()
                .map(|crate_name| wildmatch::WildMatch::new(&format!("{crate_name}::*")))
                .collect(),
            ..Default::default()
        }
    }

    pub(crate) fn unstable_packages(&self, type_name: &str) -> &[UnstablePackage] {
        let crate_name = type_name.split("::").next().unwrap_or(type_name);
        self.blocked_crates.get(crate_name).map_or(&[], Vec::as_slice)
    }
}

fn is_stable_version(version: &cargo_metadata::semver::Version) -> bool {
    version.major >= 1 && version.pre.is_empty()
}

fn reachable_packages(metadata: &Metadata, root: &PackageId) -> (HashSet<PackageId>, HashMap<PackageId, BTreeSet<String>>) {
    let Some(resolve) = &metadata.resolve else {
        return (HashSet::new(), HashMap::new());
    };
    let nodes = resolve.nodes.iter().map(|node| (&node.id, node)).collect::<HashMap<_, _>>();
    let mut reachable = HashSet::from([root.clone()]);
    let mut aliases = HashMap::<PackageId, BTreeSet<String>>::new();
    let mut pending = VecDeque::from([root.clone()]);

    while let Some(package_id) = pending.pop_front() {
        let Some(node) = nodes.get(&package_id) else {
            continue;
        };
        for dependency in &node.deps {
            let is_runtime_dependency =
                dependency.dep_kinds.is_empty() || dependency.dep_kinds.iter().any(|kind| kind.kind == DependencyKind::Normal);
            if !is_runtime_dependency {
                continue;
            }

            aliases.entry(dependency.pkg.clone()).or_default().insert(dependency.name.clone());
            if reachable.insert(dependency.pkg.clone()) {
                pending.push_back(dependency.pkg.clone());
            }
        }
    }

    (reachable, aliases)
}

#[cfg(test)]
mod tests {
    use cargo_metadata::semver::{Prerelease, Version};

    use super::is_stable_version;

    #[test]
    fn stable_versions_start_at_one_without_prerelease() {
        assert!(!is_stable_version(&Version::new(0, 99, 0)));
        assert!(is_stable_version(&Version::new(1, 0, 0)));
        assert!(is_stable_version(&Version::new(2, 3, 4)));

        let mut prerelease = Version::new(1, 0, 0);
        prerelease.pre = Prerelease::new("rc.1").expect("valid prerelease");
        assert!(!is_stable_version(&prerelease));
    }
}
