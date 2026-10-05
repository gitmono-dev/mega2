//! Immutable component-prefix routing for an already attested namespace.
//!
//! `S` is a caller-owned fixed source handle, not a registry path or ref selector.
//! This module neither encodes NamespaceView identity nor grants authorization,
//! retention or release-write permission. It is not wired into serving/publication.

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;

use super::{
    error::{SnapshotError, SnapshotErrorCode},
    resolver::FsKind,
    view::validate_scope_relative_path,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingPolicy {
    Mutable,
    ImmutableRelease,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Native,
    Import,
}

#[derive(Debug, Clone)]
pub struct FixedBinding<S> {
    pub mount_path: String,
    pub source: S,
    pub source_kind: SourceKind,
    /// Canonical source-root-relative path; the source root is the empty string.
    pub source_subpath: String,
    pub policy: BindingPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeClass {
    NativeTree,
    NativeCheckoutRoot,
    ImportRoot,
    ImportTree,
    Aggregate,
}

#[derive(Debug, Clone)]
pub struct SourceNode {
    pub fs_kind: FsKind,
    pub oid: String,
}

#[derive(Debug, Clone)]
pub struct SourceEntry {
    pub name: String,
    pub node: SourceNode,
}

#[derive(Debug, Clone)]
pub enum SourceOutcome {
    Found(SourceNode),
    /// Absence established by reading a fixed parent tree, never an I/O failure.
    AbsentProven,
}

/// An adapter must read only the attested fixed root held by `source`, verify
/// membership, and preserve storage errors. Paths are relative to that root;
/// `scope_path` must not be prepended again. Attestation/auth/retention are external.
/// The adapter also enforces the attested scope plus source-path byte budget.
pub trait FixedSourceReader<S> {
    fn resolve(&mut self, source: &S, source_path: &str) -> Result<SourceOutcome, SnapshotError>;
    fn list(&mut self, source: &S, source_path: &str) -> Result<Vec<SourceEntry>, SnapshotError>;
}

#[derive(Debug)]
pub struct SourceRoute<'a, S> {
    pub source: &'a S,
    pub source_path: String,
    pub source_kind: SourceKind,
    pub mount_path: Option<&'a str>,
    /// Selected binding provenance, not an inherited release protection or a
    /// write permission. A persistent release registry must fence every writer.
    pub policy: BindingPolicy,
}

#[derive(Debug)]
pub struct NamespaceEntry<'a, S> {
    pub name: String,
    pub fs_kind: FsKind,
    /// Synthetic ancestors have neither an object ID nor a source context.
    pub oid: Option<String>,
    pub source_context: Option<SourceRoute<'a, S>>,
}

#[derive(Debug)]
pub struct NamespaceDirectory<'a, S> {
    pub node_class: NodeClass,
    pub source_context: Option<SourceRoute<'a, S>>,
    pub entries: Vec<NamespaceEntry<'a, S>>,
}

#[derive(Debug, Error)]
pub enum NamespaceError {
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
    #[error("NAMESPACE_CONFLICT at {0}")]
    Conflict(String),
    #[error("duplicate mount path {0}")]
    DuplicateMount(String),
    #[error("binding source subpath is not a directory at {0}")]
    SourceSubpathNotDirectory(String),
    #[error("invalid source directory entry {0}")]
    InvalidSourceEntry(String),
}

#[derive(Debug, Default)]
struct PrefixNode {
    binding: Option<usize>,
    children: BTreeMap<String, PrefixNode>,
}

/// Construct a new index for each fixed view; no binding/source mutators exist.
/// The trie restricts each request to its path and immediate indexed children.
#[derive(Debug)]
pub struct FixedNamespaceIndex<S> {
    native: S,
    bindings: Vec<FixedBinding<S>>,
    root: PrefixNode,
    native_checkouts: BTreeSet<String>,
}

impl<S> FixedNamespaceIndex<S> {
    pub fn new<R: FixedSourceReader<S>>(
        native: S,
        bindings: Vec<FixedBinding<S>>,
        native_checkouts: BTreeSet<String>,
        reader: &mut R,
    ) -> Result<Self, NamespaceError> {
        let mut root = PrefixNode::default();
        for (id, binding) in bindings.iter().enumerate() {
            validate_scope_relative_path(&binding.mount_path)?;
            validate_relative_path(&binding.source_subpath)?;
            let mut node = &mut root;
            for component in components(&binding.mount_path) {
                node = node.children.entry(component.to_string()).or_default();
            }
            if node.binding.replace(id).is_some() {
                return Err(NamespaceError::DuplicateMount(binding.mount_path.clone()));
            }
        }
        for checkout in &native_checkouts {
            validate_scope_relative_path(checkout)?;
        }
        let index = Self {
            native,
            bindings,
            root,
            native_checkouts,
        };
        index.validate_sources(reader)?;
        Ok(index)
    }

    pub fn route(&self, path: &str) -> Result<SourceRoute<'_, S>, NamespaceError> {
        self.route_inner(path, false)
    }

    fn route_inner(
        &self,
        path: &str,
        exclude_exact: bool,
    ) -> Result<SourceRoute<'_, S>, NamespaceError> {
        validate_scope_relative_path(path)?;
        let comps = components(path);
        let mut prefix = &self.root;
        let mut selected = prefix
            .binding
            .filter(|_| !(exclude_exact && comps.is_empty()));
        let mut selected_depth = 0;
        for (depth, component) in comps.iter().enumerate() {
            let Some(child) = prefix.children.get(*component) else {
                break;
            };
            prefix = child;
            if let Some(id) = prefix.binding
                && !(exclude_exact && depth + 1 == comps.len())
            {
                selected = Some(id);
                selected_depth = depth + 1;
            }
        }
        let route = if let Some(id) = selected {
            let binding = &self.bindings[id];
            let suffix = comps[selected_depth..].join("/");
            let source_path = join_relative(&binding.source_subpath, &suffix);
            SourceRoute {
                source: &binding.source,
                source_path,
                source_kind: binding.source_kind,
                mount_path: Some(&binding.mount_path),
                policy: binding.policy,
            }
        } else {
            SourceRoute {
                source: &self.native,
                source_path: comps.join("/"),
                source_kind: SourceKind::Native,
                mount_path: None,
                policy: BindingPolicy::Mutable,
            }
        };
        validate_relative_path(&route.source_path)?;
        Ok(route)
    }

    fn prefix(&self, path: &str) -> Option<&PrefixNode> {
        let mut node = &self.root;
        for component in components(path) {
            node = node.children.get(component)?;
        }
        Some(node)
    }

    /// Validate all mount crossings once when constructing the index. The
    /// binding at the endpoint is excluded so it cannot hide a source file.
    fn validate_sources<R: FixedSourceReader<S>>(
        &self,
        reader: &mut R,
    ) -> Result<(), NamespaceError> {
        match reader.resolve(&self.native, "")? {
            SourceOutcome::Found(node) if node.fs_kind == FsKind::Directory => {}
            _ => return Err(NamespaceError::Conflict("/".to_string())),
        }
        for binding in &self.bindings {
            match reader.resolve(&binding.source, &binding.source_subpath)? {
                SourceOutcome::Found(node) if node.fs_kind == FsKind::Directory => {}
                _ => {
                    return Err(NamespaceError::SourceSubpathNotDirectory(
                        binding.mount_path.clone(),
                    ));
                }
            }
            let comps = components(&binding.mount_path);
            for length in 0..=comps.len() {
                let path = format!("/{}", comps[..length].join("/"));
                let route = self.route_inner(&path, length == comps.len())?;
                if let SourceOutcome::Found(node) =
                    reader.resolve(route.source, &route.source_path)?
                    && node.fs_kind != FsKind::Directory
                {
                    return Err(NamespaceError::Conflict(path));
                }
            }
        }
        Ok(())
    }

    /// Return proven absence as None; failures remain errors. Exact mounts
    /// replace the underlying source subtree, with only explicit descendants
    /// added. No registry enumeration or implicit native/import union occurs.
    pub fn list<R: FixedSourceReader<S>>(
        &self,
        path: &str,
        reader: &mut R,
    ) -> Result<Option<NamespaceDirectory<'_, S>>, NamespaceError> {
        let route = self.route(path)?;
        let indexed = self.prefix(path);
        let has_children = indexed.is_some_and(|node| !node.children.is_empty());
        let mut entries = BTreeMap::new();
        let mut aggregate = false;
        let source_context = match reader.resolve(route.source, &route.source_path)? {
            SourceOutcome::AbsentProven if !has_children => return Ok(None),
            SourceOutcome::AbsentProven => {
                aggregate = true;
                None
            }
            SourceOutcome::Found(node) if node.fs_kind != FsKind::Directory => {
                return Err(SnapshotError::new(
                    SnapshotErrorCode::NotDirectory,
                    "namespace path is not a directory",
                )
                .into());
            }
            SourceOutcome::Found(_) => {
                for entry in reader.list(route.source, &route.source_path)? {
                    validate_entry_name(&entry.name)?;
                    let child_path = append_absolute(path, &entry.name);
                    let context = self.route(&child_path)?;
                    let name = entry.name;
                    if entries
                        .insert(
                            name.clone(),
                            NamespaceEntry {
                                name: name.clone(),
                                fs_kind: entry.node.fs_kind,
                                oid: Some(entry.node.oid),
                                source_context: Some(context),
                            },
                        )
                        .is_some()
                    {
                        return Err(NamespaceError::InvalidSourceEntry(name));
                    }
                }
                Some(route)
            }
        };
        if let Some(prefix) = indexed {
            for (name, child) in &prefix.children {
                let child_path = append_absolute(path, name);
                if child.binding.is_some() {
                    let route = self.route(&child_path)?;
                    let node = match reader.resolve(route.source, &route.source_path)? {
                        SourceOutcome::Found(node) if node.fs_kind == FsKind::Directory => node,
                        _ => return Err(NamespaceError::SourceSubpathNotDirectory(child_path)),
                    };
                    aggregate = true;
                    entries.insert(
                        name.clone(),
                        NamespaceEntry {
                            name: name.clone(),
                            fs_kind: node.fs_kind,
                            oid: Some(node.oid),
                            source_context: Some(route),
                        },
                    );
                } else if let Some(entry) = entries.get(name) {
                    if entry.fs_kind != FsKind::Directory {
                        return Err(NamespaceError::Conflict(child_path));
                    }
                } else {
                    aggregate = true;
                    entries.insert(
                        name.clone(),
                        NamespaceEntry {
                            name: name.clone(),
                            fs_kind: FsKind::Directory,
                            oid: None,
                            source_context: None,
                        },
                    );
                }
            }
        }
        let node_class = if aggregate {
            NodeClass::Aggregate
        } else if let Some(route) = &source_context {
            match route.source_kind {
                SourceKind::Import if route.mount_path == Some(path) => NodeClass::ImportRoot,
                SourceKind::Import => NodeClass::ImportTree,
                SourceKind::Native if self.native_checkouts.contains(path) => {
                    NodeClass::NativeCheckoutRoot
                }
                SourceKind::Native => NodeClass::NativeTree,
            }
        } else {
            NodeClass::Aggregate
        };
        Ok(Some(NamespaceDirectory {
            node_class,
            source_context,
            entries: entries.into_values().collect(),
        }))
    }
}

fn components(path: &str) -> Vec<&str> {
    if path == "/" {
        Vec::new()
    } else {
        path[1..].split('/').collect()
    }
}

fn join_relative(parent: &str, suffix: &str) -> String {
    match (parent.is_empty(), suffix.is_empty()) {
        (true, _) => suffix.to_string(),
        (_, true) => parent.to_string(),
        _ => format!("{parent}/{suffix}"),
    }
}

fn append_absolute(parent: &str, name: &str) -> String {
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("{parent}/{name}")
    }
}

fn validate_relative_path(path: &str) -> Result<(), SnapshotError> {
    validate_scope_relative_path(&format!("/{path}"))
}

fn validate_entry_name(name: &str) -> Result<(), NamespaceError> {
    if name.contains('/') || validate_relative_path(name).is_err() || name.is_empty() {
        return Err(NamespaceError::InvalidSourceEntry(name.to_string()));
    }
    Ok(())
}
