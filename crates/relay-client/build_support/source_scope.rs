//! Declared client source envelope, not a complete workspace or compiler input trace.
use super::{invalid, reject_redirects};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Component, Path, PathBuf},
};
use toml_edit::{Document, Item};

pub struct Scope {
    pub directories: BTreeSet<PathBuf>,
    pub files: BTreeSet<PathBuf>,
    pub parsed: BTreeMap<PathBuf, [u8; 32]>,
}

fn document(path: &Path, parsed: &mut BTreeMap<PathBuf, [u8; 32]>) -> io::Result<Document> {
    reject_redirects(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() > 4 * 1024 * 1024 {
        return Err(invalid("Cargo metadata is not a bounded regular file"));
    }
    let text = fs::read_to_string(path)?;
    let after = fs::symlink_metadata(path)?;
    if metadata.len() != after.len() || metadata.modified()? != after.modified()? {
        return Err(invalid(
            "Cargo metadata changed while resolving client inputs",
        ));
    }
    let digest: [u8; 32] = Sha256::digest(text.as_bytes()).into();
    if parsed
        .insert(path.to_owned(), digest)
        .is_some_and(|previous| previous != digest)
    {
        return Err(invalid(
            "Cargo metadata changed while resolving client inputs",
        ));
    }
    text.parse()
        .map_err(|_| invalid("Invalid Cargo TOML in client source envelope"))
}

pub(super) fn inside(root: &Path, base: &Path, relative: &str) -> io::Result<PathBuf> {
    let mut path = base.to_path_buf();
    let requested = Path::new(relative);
    if requested.is_absolute() {
        return Err(invalid(
            "Local Cargo inputs must remain inside the workspace",
        ));
    }
    for part in requested.components() {
        match part {
            Component::Normal(name) => {
                path.push(name);
                reject_redirects(&path)?;
            }
            Component::CurDir => {}
            Component::ParentDir if path.starts_with(root) && path != root => {
                path.pop();
            }
            _ => return Err(invalid("Local Cargo input escaped the workspace")),
        }
    }
    if !path.starts_with(root) {
        return Err(invalid("Local Cargo input escaped the workspace"));
    }
    reject_redirects(&path)?;
    Ok(path)
}

fn dependency_paths(
    root: &Path,
    package: &Path,
    doc: &Document,
    workspace: &Document,
) -> io::Result<Vec<PathBuf>> {
    let mut result = Vec::new();
    let mut sections = vec![doc.as_table()];
    if let Some(targets) = doc.get("target").and_then(Item::as_table) {
        sections.extend(targets.iter().filter_map(|(_, item)| item.as_table()));
    }
    for section in sections {
        for kind in ["dependencies", "build-dependencies", "dev-dependencies"] {
            let Some(dependencies) = section.get(kind).and_then(Item::as_table_like) else {
                continue;
            };
            for (name, declaration) in dependencies.iter() {
                let inherited = declaration.get("workspace").and_then(Item::as_bool) == Some(true);
                let (base, declaration) = if inherited {
                    (
                        root,
                        workspace
                            .get("workspace")
                            .and_then(|v| v.get("dependencies"))
                            .and_then(|v| v.get(name))
                            .ok_or_else(|| invalid("Missing inherited workspace dependency"))?,
                    )
                } else {
                    (package, declaration)
                };
                if let Some(path) = declaration.get("path").and_then(Item::as_str) {
                    result.push(inside(root, base, path)?);
                }
            }
        }
    }
    Ok(result)
}

fn package_version(doc: &Document, workspace: &Document) -> io::Result<String> {
    let value = doc
        .get("package")
        .and_then(|v| v.get("version"))
        .ok_or_else(|| invalid("Client package version missing"))?;
    value
        .as_str()
        .or_else(|| {
            (value.get("workspace").and_then(Item::as_bool) == Some(true))
                .then(|| {
                    workspace
                        .get("workspace")?
                        .get("package")?
                        .get("version")?
                        .as_str()
                })
                .flatten()
        })
        .map(str::to_owned)
        .ok_or_else(|| invalid("Unsupported client package version"))
}

/// Resolve the locked registry graph only to select workspace path patches which
/// can reach this client. The declared source envelope intentionally includes
/// optional/other-target/test dependencies in that graph.
fn reachable_names(lock: &Document, name: &str, version: &str) -> io::Result<BTreeSet<String>> {
    let packages = lock
        .get("package")
        .and_then(Item::as_array_of_tables)
        .ok_or_else(|| invalid("Cargo.lock package inventory missing"))?;
    let mut pending = vec![(name.to_owned(), Some(version.to_owned()))];
    let mut seen = BTreeSet::new();
    let mut names = BTreeSet::new();
    while let Some((name, version)) = pending.pop() {
        if !seen.insert((name.clone(), version.clone())) {
            continue;
        }
        let matches = packages
            .iter()
            .filter(|package| {
                package.get("name").and_then(Item::as_str) == Some(&name)
                    && version.as_ref().is_none_or(|version| {
                        package.get("version").and_then(Item::as_str) == Some(version)
                    })
            })
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return Err(invalid("Ambiguous or absent locked client dependency"));
        }
        names.insert(name);
        if let Some(dependencies) = matches[0].get("dependencies").and_then(Item::as_array) {
            for dependency in dependencies.iter() {
                let mut words = dependency
                    .as_str()
                    .ok_or_else(|| invalid("Invalid locked dependency"))?
                    .split_whitespace();
                let name = words
                    .next()
                    .ok_or_else(|| invalid("Empty locked dependency"))?;
                let version = words
                    .next()
                    .filter(|word| !word.starts_with('('))
                    .map(str::to_owned);
                pending.push((name.to_owned(), version));
            }
        }
        if seen.len() > 20_000 {
            return Err(invalid("Client dependency graph exceeds its limit"));
        }
    }
    Ok(names)
}

fn paths(item: Option<&Item>) -> io::Result<Vec<&str>> {
    let Some(item) = item else {
        return Ok(Vec::new());
    };
    item.as_array()
        .ok_or_else(|| invalid("Envelope paths must be arrays"))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| invalid("Envelope paths must be UTF-8 strings"))
        })
        .collect()
}

fn declared(scope: &mut Scope, path: PathBuf, directory: bool) -> io::Result<()> {
    reject_redirects(&path)?;
    let metadata = fs::symlink_metadata(&path)?;
    if (directory && !metadata.is_dir()) || (!directory && !metadata.is_file()) {
        return Err(invalid(
            "Declared client-envelope input is not the required direct file/directory",
        ));
    }
    if directory {
        scope.directories.insert(path);
    } else {
        scope.files.insert(path);
    }
    Ok(())
}

pub fn inputs(root: &Path) -> io::Result<Scope> {
    let mut parsed = BTreeMap::new();
    let workspace = document(&root.join("Cargo.toml"), &mut parsed)?;
    let lock = document(&root.join("Cargo.lock"), &mut parsed)?;
    let inventory_path = root.join("crates/relay-client/build_support/client-inputs.toml");
    let inventory = document(&inventory_path, &mut parsed)?;
    if inventory.get("schema").and_then(Item::as_integer) != Some(1) {
        return Err(invalid(
            "Unsupported explicit client-envelope inventory schema",
        ));
    }
    let client = root.join("crates/relay-client");
    let client_manifest = document(&client.join("Cargo.toml"), &mut parsed)?;
    let client_name = client_manifest
        .get("package")
        .and_then(|v| v.get("name"))
        .and_then(Item::as_str)
        .ok_or_else(|| invalid("Client package name missing"))?;
    let names = reachable_names(
        &lock,
        client_name,
        &package_version(&client_manifest, &workspace)?,
    )?;
    let mut pending = vec![client];
    if let Some(patches) = workspace
        .get("patch")
        .and_then(|v| v.get("crates-io"))
        .and_then(Item::as_table_like)
    {
        for (name, declaration) in patches.iter() {
            let name = declaration
                .get("package")
                .and_then(Item::as_str)
                .unwrap_or(name);
            if names.contains(name) {
                if let Some(path) = declaration.get("path").and_then(Item::as_str) {
                    pending.push(inside(root, root, path)?);
                }
            }
        }
    }
    let mut packages = BTreeSet::new();
    let mut scope = Scope {
        directories: BTreeSet::new(),
        files: BTreeSet::new(),
        parsed: BTreeMap::new(),
    };
    for name in ["Cargo.toml", "Cargo.lock"] {
        declared(&mut scope, root.join(name), false)?;
    }
    declared(&mut scope, inventory_path, false)?;
    for name in paths(inventory.get("workspace_files"))? {
        declared(&mut scope, inside(root, root, name)?, false)?;
    }
    while let Some(package) = pending.pop() {
        if !packages.insert(package.clone()) {
            continue;
        }
        if packages.len() > 512 {
            return Err(invalid("Local client dependency count exceeds its limit"));
        }
        let doc = document(&package.join("Cargo.toml"), &mut parsed)?;
        pending.extend(dependency_paths(root, &package, &doc, &workspace)?);
        let relative = package
            .strip_prefix(root)
            .map_err(|_| invalid("Envelope package escaped workspace"))?
            .to_str()
            .ok_or_else(|| invalid("Non-UTF-8 package path"))?
            .replace('\\', "/");
        let declaration = inventory
            .get("packages")
            .and_then(|packages| packages.get(&relative))
            .ok_or_else(|| {
                invalid(
                    "Local dependency is missing from client-inputs.toml; declare its source roots",
                )
            })?;
        declared(&mut scope, package.join("Cargo.toml"), false)?;
        for name in paths(declaration.get("directories"))? {
            declared(&mut scope, inside(root, &package, name)?, true)?;
        }
        for name in paths(declaration.get("files"))? {
            declared(&mut scope, inside(root, &package, name)?, false)?;
        }
        // A manifest cannot silently add a nonstandard target/build script
        // outside the explicit envelope. Register that input before building.
        let covered = |path: &Path| {
            scope.files.contains(path)
                || scope
                    .directories
                    .iter()
                    .any(|directory| path.starts_with(directory))
        };
        for name in ["lib", "bin", "example", "test", "bench"] {
            if let Some(item) = doc.get(name) {
                let tables = item
                    .as_array_of_tables()
                    .map(|tables| tables.iter().collect::<Vec<_>>())
                    .or_else(|| item.as_table().map(|table| vec![table]))
                    .unwrap_or_default();
                for table in tables {
                    if let Some(path) = table.get("path").and_then(Item::as_str) {
                        if !covered(&inside(root, &package, path)?) {
                            return Err(invalid("Rust target is outside declared client envelope"));
                        }
                    }
                }
            }
        }
        if let Some(path) = doc
            .get("package")
            .and_then(|v| v.get("build"))
            .and_then(Item::as_str)
        {
            if !covered(&inside(root, &package, path)?) {
                return Err(invalid("Build script is outside declared client envelope"));
            }
        }
        // Cargo auto-discovers a conventional build.rs even without package.build.
        let build = package.join("build.rs");
        if fs::symlink_metadata(&build).is_ok() && !covered(&build) {
            return Err(invalid(
                "Conventional build.rs must be declared in client-inputs.toml",
            ));
        }
    }
    scope.parsed = parsed;
    Ok(scope)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manifest_changed_during_graph_resolution_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("Cargo.toml");
        fs::write(&path, "[package]\nname='first'\n").unwrap();
        let mut parsed = BTreeMap::new();
        document(&path, &mut parsed).unwrap();
        fs::write(&path, "[package]\nname='second'\n").unwrap();
        assert!(document(&path, &mut parsed).is_err());
    }
}
