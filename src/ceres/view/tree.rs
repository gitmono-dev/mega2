use std::collections::{BTreeMap, HashMap};

use git_internal::{
    hash::{HashKind, ObjectHash},
    internal::object::{
        ObjectTrait,
        tree::{Tree, TreeItem, TreeItemMode},
    },
};

use super::{
    filter::{Filter, Selector, print},
    tree_source::{
        MissingObject, PathLookup, TreeSource, build_tree, empty_tree_id, lookup_path,
        parse_tree_bytes, read_tree,
    },
};

pub(crate) const FILTER_TREE_MEMO_CAPACITY_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FilterOutput {
    pub tree_id: String,
    pub trees: BTreeMap<String, Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum FilterTreeError {
    #[error("tree object is unavailable: {0:?}")]
    Missing(MissingObject),
    #[error("tree filtering invariant failed")]
    Invariant,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct MemoKey {
    filter: String,
    input_tree: String,
}

#[derive(Clone)]
struct MemoValue {
    output: FilterOutput,
    bytes: usize,
}

pub(crate) struct FilterMemo {
    capacity_bytes: usize,
    used_bytes: usize,
    entries: HashMap<MemoKey, MemoValue>,
}

impl Default for FilterMemo {
    fn default() -> Self {
        Self::with_capacity(FILTER_TREE_MEMO_CAPACITY_BYTES)
    }
}

impl FilterMemo {
    pub(crate) fn with_capacity(capacity_bytes: usize) -> Self {
        Self {
            capacity_bytes,
            used_bytes: 0,
            entries: HashMap::new(),
        }
    }

    pub(crate) fn used_bytes(&self) -> usize {
        self.used_bytes
    }

    pub(crate) fn contains(&self, filter: &str, input_tree: &str) -> bool {
        self.entries.contains_key(&MemoKey {
            filter: filter.to_owned(),
            input_tree: input_tree.to_owned(),
        })
    }

    fn get(&self, key: &MemoKey) -> Option<&MemoValue> {
        self.entries.get(key)
    }

    fn insert(&mut self, key: MemoKey, output: FilterOutput) {
        let bytes = key.filter.len()
            + key.input_tree.len()
            + output.tree_id.len()
            + output.trees.values().map(Vec::len).sum::<usize>();
        if bytes > self.capacity_bytes {
            return;
        }

        if let Some(previous) = self.entries.remove(&key) {
            self.used_bytes = self.used_bytes.saturating_sub(previous.bytes);
        }

        while self.used_bytes.saturating_add(bytes) > self.capacity_bytes {
            let Some(evicted) = self.entries.keys().next().cloned() else {
                break;
            };
            if let Some(value) = self.entries.remove(&evicted) {
                self.used_bytes = self.used_bytes.saturating_sub(value.bytes);
            }
        }
        self.used_bytes = self.used_bytes.saturating_add(bytes);
        self.entries.insert(key, MemoValue { output, bytes });
    }
}

pub(crate) fn filter_tree<S: TreeSource + ?Sized>(
    kind: HashKind,
    source: &S,
    memo: &mut FilterMemo,
    filter: &Filter,
    tree_id: &str,
) -> Result<FilterOutput, FilterTreeError> {
    let mut context = EvalContext {
        kind,
        source,
        memo,
        available: BTreeMap::new(),
    };
    let tree_id = context.eval(filter, tree_id)?;
    context.output_for(tree_id)
}

struct EvalContext<'a, S: TreeSource + ?Sized> {
    kind: HashKind,
    source: &'a S,
    memo: &'a mut FilterMemo,
    available: BTreeMap<String, Vec<u8>>,
}

impl<S: TreeSource + ?Sized> EvalContext<'_, S> {
    fn eval(&mut self, filter: &Filter, input_tree: &str) -> Result<String, FilterTreeError> {
        match filter {
            Filter::Nop => return Ok(input_tree.to_owned()),
            Filter::Empty => return self.empty_id(),
            _ => {}
        }

        let key = MemoKey {
            filter: print(filter),
            input_tree: input_tree.to_owned(),
        };
        if let Some(value) = self.memo.get(&key) {
            let output = value.output.clone();
            self.available.extend(output.trees.clone());
            return Ok(output.tree_id);
        }

        let tree_id = match filter {
            Filter::Subdir(path) => self.subdir(input_tree, path.segments())?,
            Filter::Prefix(path) => self.prefix(input_tree, path.segments())?,
            Filter::Exclude(selectors) => self.exclude(input_tree, selectors)?,
            Filter::Compose(members) => self.compose(input_tree, members)?,
            Filter::Chain(ops) => {
                let mut current = input_tree.to_owned();
                for op in ops {
                    current = self.eval(op, &current)?;
                }
                current
            }
            Filter::Nop => input_tree.to_owned(),
            Filter::Empty => self.empty_id()?,
        };
        let output = self.output_for(tree_id.clone())?;
        self.memo.insert(key, output);
        Ok(tree_id)
    }

    fn empty_id(&self) -> Result<String, FilterTreeError> {
        empty_tree_id(self.kind)
            .map(|id| id.to_string())
            .map_err(|_| FilterTreeError::Invariant)
    }

    fn is_empty(&self, tree_id: &str) -> Result<bool, FilterTreeError> {
        let empty = empty_tree_id(self.kind).map_err(|_| FilterTreeError::Invariant)?;
        Ok(ObjectHash::from_hex_for_kind(self.kind, tree_id).is_ok_and(|id| id == empty))
    }

    fn source_with_available(&self) -> OverlayTreeSource<'_, S> {
        OverlayTreeSource {
            kind: self.kind,
            available: &self.available,
            source: self.source,
        }
    }

    fn read(&self, tree_id: &str) -> Result<Tree, FilterTreeError> {
        read_tree(self.kind, &self.source_with_available(), tree_id)
            .map_err(FilterTreeError::Missing)
    }

    fn subdir(&self, input_tree: &str, path: &[String]) -> Result<String, FilterTreeError> {
        match lookup_path(self.kind, &self.source_with_available(), input_tree, path)
            .map_err(FilterTreeError::Missing)?
        {
            PathLookup::Tree(tree_id) => Ok(tree_id),
            PathLookup::Absent | PathLookup::NotTree => self.empty_id(),
        }
    }

    fn prefix(&mut self, input_tree: &str, path: &[String]) -> Result<String, FilterTreeError> {
        if self.is_empty(input_tree)? {
            return Ok(input_tree.to_owned());
        }
        let mut current = input_tree.to_owned();
        for segment in path.iter().rev() {
            current = self.make_tree(vec![tree_item(self.kind, segment, &current)?])?;
        }
        Ok(current)
    }

    fn exclude(
        &mut self,
        input_tree: &str,
        selectors: &[Selector],
    ) -> Result<String, FilterTreeError> {
        if selectors.is_empty() {
            return Ok(input_tree.to_owned());
        }
        let selectors = selectors
            .iter()
            .map(SelectorRef::from_selector)
            .collect::<Vec<_>>();
        Ok(self.exclude_inner(input_tree, &selectors)?.tree_id)
    }

    fn exclude_inner(
        &mut self,
        tree_id: &str,
        selectors: &[SelectorRef<'_>],
    ) -> Result<RewriteResult, FilterTreeError> {
        let tree = self.read(tree_id)?;
        let mut changed = false;
        let mut items = Vec::with_capacity(tree.tree_items.len());

        for item in tree.tree_items {
            let matches = selectors
                .iter()
                .filter(|selector| selector.segments.first() == Some(&item.name))
                .copied()
                .collect::<Vec<_>>();
            if matches.is_empty() {
                items.push(item);
                continue;
            }

            let remove = matches.iter().any(|selector| {
                selector.segments.len() == 1
                    && (!selector.tree_only || item.mode == TreeItemMode::Tree)
            });
            if remove {
                changed = true;
                continue;
            }

            let descendants = matches
                .into_iter()
                .filter(|selector| selector.segments.len() > 1)
                .map(|selector| SelectorRef {
                    segments: &selector.segments[1..],
                    tree_only: selector.tree_only,
                })
                .collect::<Vec<_>>();
            if descendants.is_empty() || item.mode != TreeItemMode::Tree {
                items.push(item);
                continue;
            }

            let child = self.exclude_inner(&item.id.to_string(), &descendants)?;
            if !child.changed {
                items.push(item);
                continue;
            }

            changed = true;
            if !self.is_empty(&child.tree_id)? {
                items.push(tree_item(self.kind, &item.name, &child.tree_id)?);
            }
        }

        if !changed {
            return Ok(RewriteResult {
                tree_id: tree_id.to_owned(),
                changed: false,
            });
        }

        let empty = empty_tree_id(self.kind).map_err(|_| FilterTreeError::Invariant)?;
        items.retain(|item| item.mode != TreeItemMode::Tree || item.id != empty);
        Ok(RewriteResult {
            tree_id: self.make_tree(items)?,
            changed: true,
        })
    }

    fn compose(&mut self, input_tree: &str, members: &[Filter]) -> Result<String, FilterTreeError> {
        let mut outputs = Vec::with_capacity(members.len());
        for member in members {
            let output = self.eval(member, input_tree)?;
            if !self.is_empty(&output)? {
                outputs.push(output);
            }
        }
        match outputs.len() {
            0 => self.empty_id(),
            1 => Ok(outputs.remove(0)),
            _ => self.overlay(&outputs),
        }
    }

    fn overlay(&mut self, tree_ids: &[String]) -> Result<String, FilterTreeError> {
        let mut non_empty = Vec::with_capacity(tree_ids.len());
        for tree_id in tree_ids {
            if !self.is_empty(tree_id)? {
                non_empty.push(tree_id.clone());
            }
        }
        match non_empty.len() {
            0 => return self.empty_id(),
            1 => return Ok(non_empty[0].clone()),
            _ => {}
        }

        let mut by_name = BTreeMap::<String, Vec<TreeItem>>::new();
        for tree_id in &non_empty {
            for item in self.read(tree_id)?.tree_items {
                by_name.entry(item.name.clone()).or_default().push(item);
            }
        }

        let mut items = Vec::with_capacity(by_name.len());
        for (name, candidates) in by_name {
            let item = if candidates.len() == 1 {
                candidates
                    .into_iter()
                    .next()
                    .ok_or(FilterTreeError::Invariant)?
            } else if candidates
                .iter()
                .all(|item| item.mode == TreeItemMode::Tree)
            {
                let children = candidates
                    .iter()
                    .map(|item| item.id.to_string())
                    .collect::<Vec<_>>();
                let child = self.overlay(&children)?;
                if self.is_empty(&child)? {
                    continue;
                }
                tree_item(self.kind, &name, &child)?
            } else {
                return Err(FilterTreeError::Invariant);
            };
            if item.mode != TreeItemMode::Tree || !self.is_empty(&item.id.to_string())? {
                items.push(item);
            }
        }
        self.make_tree(items)
    }

    fn make_tree(&mut self, items: Vec<TreeItem>) -> Result<String, FilterTreeError> {
        let tree = build_tree(self.kind, items).map_err(|_| FilterTreeError::Invariant)?;
        let tree_id = tree.id.to_string();
        let bytes = tree.to_data().map_err(|_| FilterTreeError::Invariant)?;
        self.available.entry(tree_id.clone()).or_insert(bytes);
        Ok(tree_id)
    }

    fn output_for(&self, tree_id: String) -> Result<FilterOutput, FilterTreeError> {
        let mut trees = BTreeMap::new();
        if self.is_empty(&tree_id)? {
            trees.insert(tree_id.clone(), Vec::new());
        } else {
            self.collect_output_trees(&tree_id, &mut trees)?;
        }
        Ok(FilterOutput { tree_id, trees })
    }

    fn collect_output_trees(
        &self,
        tree_id: &str,
        trees: &mut BTreeMap<String, Vec<u8>>,
    ) -> Result<(), FilterTreeError> {
        let Some(bytes) = self.available.get(tree_id) else {
            return Ok(());
        };
        if trees.insert(tree_id.to_owned(), bytes.clone()).is_some() {
            return Ok(());
        }
        let tree = parse_tree_bytes(self.kind, tree_id, bytes).map_err(FilterTreeError::Missing)?;
        for item in tree.tree_items {
            if item.mode == TreeItemMode::Tree {
                self.collect_output_trees(&item.id.to_string(), trees)?;
            }
        }
        Ok(())
    }
}

struct OverlayTreeSource<'a, S: TreeSource + ?Sized> {
    kind: HashKind,
    available: &'a BTreeMap<String, Vec<u8>>,
    source: &'a S,
}

impl<S: TreeSource + ?Sized> TreeSource for OverlayTreeSource<'_, S> {
    fn read_tree(&self, tree_id: &str) -> Result<Tree, MissingObject> {
        match self.available.get(tree_id) {
            Some(bytes) => parse_tree_bytes(self.kind, tree_id, bytes),
            None => self.source.read_tree(tree_id),
        }
    }
}

#[derive(Clone, Copy)]
struct SelectorRef<'a> {
    segments: &'a [String],
    tree_only: bool,
}

impl<'a> SelectorRef<'a> {
    fn from_selector(selector: &'a Selector) -> Self {
        match selector {
            Selector::Entry(path) => Self {
                segments: path.segments(),
                tree_only: false,
            },
            Selector::Tree(path) => Self {
                segments: path.segments(),
                tree_only: true,
            },
        }
    }
}

struct RewriteResult {
    tree_id: String,
    changed: bool,
}

fn tree_item(kind: HashKind, name: &str, tree_id: &str) -> Result<TreeItem, FilterTreeError> {
    let id =
        ObjectHash::from_hex_for_kind(kind, tree_id).map_err(|_| FilterTreeError::Invariant)?;
    Ok(TreeItem {
        mode: TreeItemMode::Tree,
        id,
        name: name.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap, HashSet};

    use git_internal::{
        hash::{HashKind, ObjectHash, set_hash_kind_for_test},
        internal::object::{
            ObjectTrait,
            tree::{Tree, TreeItem, TreeItemMode},
            types::ObjectType,
        },
    };
    use rand::{RngExt, SeedableRng, rngs::StdRng};

    use super::{FilterMemo, FilterTreeError, filter_tree};
    use crate::ceres::view::{
        filter::{
            Filter, Selector, ViewPath, canonicalize, parse, print,
            semantics::{dst_paths, is_prefix, path_segments, src_paths},
        },
        tree_source::{
            InMemoryTreeSource, MissingObjectReason, build_tree, empty_tree_id, read_tree,
        },
    };

    fn blob(kind: HashKind, label: &[u8]) -> ObjectHash {
        ObjectHash::from_type_and_data_for_kind(kind, ObjectType::Blob, label).unwrap()
    }

    fn item(kind: HashKind, mode: TreeItemMode, name: &str, label: &[u8]) -> TreeItem {
        TreeItem {
            mode,
            id: blob(kind, label),
            name: name.to_owned(),
        }
    }

    fn tree_item(name: &str, tree: &Tree) -> TreeItem {
        TreeItem {
            mode: TreeItemMode::Tree,
            id: tree.id,
            name: name.to_owned(),
        }
    }

    fn bytes(tree: &Tree) -> Vec<u8> {
        tree.to_data().unwrap()
    }

    fn tree_source(kind: HashKind, trees: &[Tree], missing: &[String]) -> InMemoryTreeSource {
        InMemoryTreeSource::new(
            kind,
            trees
                .iter()
                .map(|tree| (tree.id.to_string(), bytes(tree)))
                .collect::<HashMap<_, _>>(),
            missing.iter().cloned().collect::<HashSet<_>>(),
        )
    }

    fn parse_filter(text: &str) -> Filter {
        canonicalize(parse(text).unwrap())
    }

    fn output(
        kind: HashKind,
        source: &InMemoryTreeSource,
        memo: &mut FilterMemo,
        filter: &Filter,
        root: &str,
    ) -> super::FilterOutput {
        filter_tree(kind, source, memo, filter, root).unwrap()
    }

    fn root_fixture(kind: HashKind) -> (Tree, Tree, Tree, Tree) {
        let b = build_tree(kind, vec![item(kind, TreeItemMode::Blob, "x", b"x")]).unwrap();
        let a = build_tree(kind, vec![tree_item("b", &b)]).unwrap();
        let c = build_tree(kind, vec![item(kind, TreeItemMode::Blob, "y", b"y")]).unwrap();
        let root = build_tree(
            kind,
            vec![
                tree_item("a", &a),
                tree_item("c", &c),
                item(kind, TreeItemMode::Blob, "file", b"file"),
            ],
        )
        .unwrap();
        (root, a, b, c)
    }

    fn assert_twice(
        kind: HashKind,
        source: &InMemoryTreeSource,
        filter: &Filter,
        root: &str,
        expected_tree: &str,
        expected_trees: BTreeMap<String, Vec<u8>>,
    ) {
        let mut memo = FilterMemo::default();
        let first = output(kind, source, &mut memo, filter, root);
        let second = output(kind, source, &mut memo, filter, root);
        assert_eq!(first.tree_id, expected_tree);
        assert_eq!(first.trees, expected_trees);
        assert_eq!(second, first);
    }

    fn assert_missing(
        kind: HashKind,
        source: &InMemoryTreeSource,
        filter: &Filter,
        root: &str,
        missing_tree: &str,
        reason: MissingObjectReason,
    ) {
        let expected = read_tree(kind, source, missing_tree).unwrap_err();
        assert_eq!(expected.reason, reason);
        let mut memo = FilterMemo::default();
        assert_eq!(
            filter_tree(kind, source, &mut memo, filter, root),
            Err(FilterTreeError::Missing(expected))
        );
        assert!(!memo.contains(&print(filter), root));
    }

    fn assert_missing_then_repaired(
        kind: HashKind,
        filter: &Filter,
        root: &str,
        missing_tree: &str,
        failed: &InMemoryTreeSource,
        complete: &InMemoryTreeSource,
        cached_before_failure: Option<(&Filter, &str)>,
    ) {
        let expected = read_tree(kind, failed, missing_tree).unwrap_err();
        let mut memo = FilterMemo::default();
        assert_eq!(
            filter_tree(kind, failed, &mut memo, filter, root),
            Err(FilterTreeError::Missing(expected))
        );
        assert!(!memo.contains(&print(filter), root));
        if let Some((cached_filter, cached_input)) = cached_before_failure {
            assert!(memo.contains(&print(cached_filter), cached_input));
        }
        let retried = output(kind, complete, &mut memo, filter, root);
        let fresh = output(kind, complete, &mut FilterMemo::default(), filter, root);
        assert_eq!(retried, fresh);
    }

    fn random_tree(
        kind: HashKind,
        rng: &mut StdRng,
        depth: usize,
        next: &mut usize,
        all: &mut Vec<Tree>,
        modes: &mut HashSet<TreeItemMode>,
        empty_tree_entries: &mut usize,
    ) -> Tree {
        const LEAF_MODE_SLOTS: usize = 4;
        const EMPTY_TREE_SLOTS: usize = 1;
        const CHILD_TREE_SLOTS: usize = 1;

        let count = rng.random_range(2..=4);
        let mut names = HashSet::with_capacity(count);
        let mut items = Vec::with_capacity(count);
        for _ in 0..count {
            let preferred = ["a", "b", "c"][rng.random_range(0..3)];
            let mut name = if rng.random_range(0..3) == 0 {
                preferred.to_owned()
            } else {
                let generated = format!("n{next}");
                *next += 1;
                generated
            };
            while !names.insert(name.clone()) {
                name = format!("n{next}");
                *next += 1;
            }
            let slots =
                LEAF_MODE_SLOTS + EMPTY_TREE_SLOTS + usize::from(depth > 0) * CHILD_TREE_SLOTS;
            let choice = rng.random_range(0..slots);
            let item = if choice < LEAF_MODE_SLOTS {
                let mode = [
                    TreeItemMode::Blob,
                    TreeItemMode::BlobExecutable,
                    TreeItemMode::Link,
                    TreeItemMode::Commit,
                ][choice];
                modes.insert(mode);
                item(kind, mode, &name, name.as_bytes())
            } else if choice < LEAF_MODE_SLOTS + EMPTY_TREE_SLOTS {
                modes.insert(TreeItemMode::Tree);
                *empty_tree_entries += 1;
                TreeItem {
                    mode: TreeItemMode::Tree,
                    id: empty_tree_id(kind).unwrap(),
                    name,
                }
            } else {
                let child = random_tree(kind, rng, depth - 1, next, all, modes, empty_tree_entries);
                modes.insert(TreeItemMode::Tree);
                tree_item(&name, &child)
            };
            items.push(item);
        }
        let tree = build_tree(kind, items).unwrap();
        all.push(tree.clone());
        tree
    }

    fn random_path(rng: &mut StdRng) -> ViewPath {
        const SEGMENTS: [&str; 3] = ["a", "b", "c"];
        let depth = rng.random_range(1..=2);
        ViewPath::new(
            (0..depth)
                .map(|_| SEGMENTS[rng.random_range(0..SEGMENTS.len())].to_owned())
                .collect(),
        )
    }

    fn random_selector(rng: &mut StdRng) -> Selector {
        let path = random_path(rng);
        if rng.random_range(0..2) == 0 {
            Selector::Entry(path)
        } else {
            Selector::Tree(path)
        }
    }

    fn random_leaf_filter(rng: &mut StdRng) -> Filter {
        match rng.random_range(0..5) {
            0 => Filter::Subdir(random_path(rng)),
            1 => Filter::Prefix(random_path(rng)),
            2 => Filter::Exclude(
                (0..rng.random_range(0..=2))
                    .map(|_| random_selector(rng))
                    .collect(),
            ),
            3 => Filter::Nop,
            _ => Filter::Empty,
        }
    }

    fn random_filter(rng: &mut StdRng, depth: usize) -> Filter {
        if depth == 0 {
            return random_leaf_filter(rng);
        }
        match rng.random_range(0..7) {
            0..=4 => random_leaf_filter(rng),
            5 => Filter::Compose(vec![
                random_filter(rng, depth - 1),
                random_filter(rng, depth - 1),
            ]),
            _ => Filter::Chain(vec![
                random_filter(rng, depth - 1),
                random_filter(rng, depth - 1),
            ]),
        }
    }

    fn path_overlap(left: &str, right: &str) -> bool {
        let left = path_segments(left);
        let right = path_segments(right);
        is_prefix(&left, &right) || is_prefix(&right, &left)
    }

    fn compose_members_are_disjoint(filter: &Filter) -> bool {
        match filter {
            Filter::Compose(members) => {
                for (index, left) in members.iter().enumerate() {
                    for right in &members[index + 1..] {
                        for (left_paths, right_paths) in [
                            (src_paths(left), src_paths(right)),
                            (dst_paths(left), dst_paths(right)),
                        ] {
                            if left_paths.iter().any(|left_path| {
                                right_paths
                                    .iter()
                                    .any(|right_path| path_overlap(left_path, right_path))
                            }) {
                                return false;
                            }
                        }
                    }
                }
                members.iter().all(compose_members_are_disjoint)
            }
            Filter::Chain(ops) => ops.iter().all(compose_members_are_disjoint),
            _ => true,
        }
    }

    fn assert_same_tree_id(kind: HashKind, source: &InMemoryTreeSource, raw: &Filter, root: &str) {
        let canonical = canonicalize(raw.clone());
        let raw_output = output(kind, source, &mut FilterMemo::default(), raw, root);
        let canonical_output = output(kind, source, &mut FilterMemo::default(), &canonical, root);
        assert_eq!(
            raw_output.tree_id,
            canonical_output.tree_id,
            "{}",
            print(raw)
        );
    }

    #[test]
    fn operators_match_design() {
        let kind = HashKind::Sha1;
        let _guard = set_hash_kind_for_test(HashKind::Sha256);
        let (root, a, b, c) = root_fixture(kind);
        let source = tree_source(kind, &[root.clone(), a.clone(), b.clone(), c.clone()], &[]);
        let empty = empty_tree_id(kind).unwrap().to_string();

        assert_twice(
            kind,
            &source,
            &Filter::Nop,
            &root.id.to_string(),
            &root.id.to_string(),
            BTreeMap::new(),
        );
        assert_twice(
            kind,
            &source,
            &Filter::Empty,
            &root.id.to_string(),
            &empty,
            BTreeMap::from([(empty.clone(), Vec::new())]),
        );
        for (filter, expected) in [
            (parse_filter(":/a"), a.id.to_string()),
            (parse_filter(":/a/b"), b.id.to_string()),
        ] {
            assert_twice(
                kind,
                &source,
                &filter,
                &root.id.to_string(),
                &expected,
                BTreeMap::new(),
            );
        }
        assert_twice(
            kind,
            &source,
            &parse_filter(":/missing"),
            &root.id.to_string(),
            &empty,
            BTreeMap::from([(empty.clone(), Vec::new())]),
        );

        let prefixed = parse_filter(":/a:prefix=x/y");
        let inner = build_tree(kind, vec![tree_item("y", &a)]).unwrap();
        let prefixed_expected = build_tree(kind, vec![tree_item("x", &inner)]).unwrap();
        assert_twice(
            kind,
            &source,
            &prefixed,
            &root.id.to_string(),
            &prefixed_expected.id.to_string(),
            BTreeMap::from([
                (inner.id.to_string(), bytes(&inner)),
                (prefixed_expected.id.to_string(), bytes(&prefixed_expected)),
            ]),
        );

        let left = build_tree(kind, vec![item(kind, TreeItemMode::Blob, "l", b"l")]).unwrap();
        let right = build_tree(kind, vec![item(kind, TreeItemMode::Blob, "r", b"r")]).unwrap();
        let compose_root =
            build_tree(kind, vec![tree_item("a", &left), tree_item("b", &right)]).unwrap();
        let compose_source = tree_source(
            kind,
            &[compose_root.clone(), left.clone(), right.clone()],
            &[],
        );
        let composed = parse_filter(":[ :/a:prefix=x/p, :/b:prefix=x/q ]");
        let merged = build_tree(kind, vec![tree_item("p", &left), tree_item("q", &right)]).unwrap();
        let compose_expected = build_tree(kind, vec![tree_item("x", &merged)]).unwrap();
        assert_twice(
            kind,
            &compose_source,
            &composed,
            &compose_root.id.to_string(),
            &compose_expected.id.to_string(),
            BTreeMap::from([
                (merged.id.to_string(), bytes(&merged)),
                (compose_expected.id.to_string(), bytes(&compose_expected)),
            ]),
        );

        let chain = Filter::Chain(vec![
            Filter::Prefix(ViewPath::new(vec!["a".to_owned(), "b".to_owned()])),
            Filter::Subdir(ViewPath::new(vec!["a".to_owned()])),
        ]);
        let chain_expected = build_tree(kind, vec![tree_item("b", &a)]).unwrap();
        assert_twice(
            kind,
            &source,
            &chain,
            &a.id.to_string(),
            &chain_expected.id.to_string(),
            BTreeMap::from([(chain_expected.id.to_string(), bytes(&chain_expected))]),
        );

        for mode in [
            TreeItemMode::Blob,
            TreeItemMode::BlobExecutable,
            TreeItemMode::Link,
            TreeItemMode::Commit,
        ] {
            let root = build_tree(kind, vec![item(kind, mode, "not-tree", b"not-tree")]).unwrap();
            let source = tree_source(kind, std::slice::from_ref(&root), &[]);
            assert_twice(
                kind,
                &source,
                &parse_filter(":/not-tree"),
                &root.id.to_string(),
                &empty,
                BTreeMap::from([(empty.clone(), Vec::new())]),
            );
        }

        let directory = build_tree(kind, vec![item(kind, TreeItemMode::Blob, "x", b"x")]).unwrap();
        let selector_root = build_tree(
            kind,
            vec![
                item(kind, TreeItemMode::Blob, "file", b"file"),
                tree_item("dir", &directory),
            ],
        )
        .unwrap();
        let selector_source = tree_source(kind, &[selector_root.clone(), directory.clone()], &[]);
        assert_twice(
            kind,
            &selector_source,
            &parse_filter(":exclude[::file/]"),
            &selector_root.id.to_string(),
            &selector_root.id.to_string(),
            BTreeMap::new(),
        );
        let without_directory =
            build_tree(kind, vec![item(kind, TreeItemMode::Blob, "file", b"file")]).unwrap();
        assert_twice(
            kind,
            &selector_source,
            &parse_filter(":exclude[::dir/]"),
            &selector_root.id.to_string(),
            &without_directory.id.to_string(),
            BTreeMap::from([(without_directory.id.to_string(), bytes(&without_directory))]),
        );

        let kept = item(kind, TreeItemMode::Blob, "kept", b"kept");
        let entry_root = build_tree(
            kind,
            vec![
                item(kind, TreeItemMode::Blob, "file", b"file"),
                tree_item("dir", &directory),
                kept.clone(),
            ],
        )
        .unwrap();
        let entry_source = tree_source(kind, &[entry_root.clone(), directory], &[]);
        let entry_expected = build_tree(kind, vec![kept]).unwrap();
        assert_twice(
            kind,
            &entry_source,
            &parse_filter(":exclude[::file,::dir]"),
            &entry_root.id.to_string(),
            &entry_expected.id.to_string(),
            BTreeMap::from([(entry_expected.id.to_string(), bytes(&entry_expected))]),
        );

        let prefixed_a = parse_filter(":/a:prefix=x");
        let prefixed_a_expected = build_tree(kind, vec![tree_item("x", &a)]).unwrap();
        assert_twice(
            kind,
            &source,
            &prefixed_a,
            &root.id.to_string(),
            &prefixed_a_expected.id.to_string(),
            BTreeMap::from([(
                prefixed_a_expected.id.to_string(),
                bytes(&prefixed_a_expected),
            )]),
        );
    }

    #[test]
    fn empty_tree_normalization() {
        let kind = HashKind::Sha1;
        let _guard = set_hash_kind_for_test(HashKind::Sha256);
        let empty = empty_tree_id(kind).unwrap();

        let b = build_tree(kind, vec![item(kind, TreeItemMode::Blob, "x", b"x")]).unwrap();
        let a = build_tree(kind, vec![tree_item("b", &b)]).unwrap();
        let root = build_tree(
            kind,
            vec![
                tree_item("a", &a),
                item(kind, TreeItemMode::Blob, "c", b"c"),
            ],
        )
        .unwrap();
        let source = tree_source(kind, &[root.clone(), a, b], &[]);
        let expected = build_tree(kind, vec![item(kind, TreeItemMode::Blob, "c", b"c")]).unwrap();
        assert_twice(
            kind,
            &source,
            &parse_filter(":exclude[::a/b/x]"),
            &root.id.to_string(),
            &expected.id.to_string(),
            BTreeMap::from([(expected.id.to_string(), bytes(&expected))]),
        );

        let a = build_tree(
            kind,
            vec![
                item(kind, TreeItemMode::Blob, "x", b"x"),
                item(kind, TreeItemMode::Blob, "y", b"y"),
            ],
        )
        .unwrap();
        let root = build_tree(
            kind,
            vec![
                tree_item("a", &a),
                TreeItem {
                    mode: TreeItemMode::Tree,
                    id: empty,
                    name: "e".to_owned(),
                },
            ],
        )
        .unwrap();
        let source = tree_source(kind, &[root.clone(), a], &[]);
        let a_without_x =
            build_tree(kind, vec![item(kind, TreeItemMode::Blob, "y", b"y")]).unwrap();
        let expected = build_tree(kind, vec![tree_item("a", &a_without_x)]).unwrap();
        assert_twice(
            kind,
            &source,
            &parse_filter(":exclude[::a/x]"),
            &root.id.to_string(),
            &expected.id.to_string(),
            BTreeMap::from([
                (a_without_x.id.to_string(), bytes(&a_without_x)),
                (expected.id.to_string(), bytes(&expected)),
            ]),
        );
        assert_twice(
            kind,
            &source,
            &parse_filter(":exclude[::nothere]"),
            &root.id.to_string(),
            &root.id.to_string(),
            BTreeMap::new(),
        );

        let leaf = build_tree(kind, vec![item(kind, TreeItemMode::Blob, "x", b"x")]).unwrap();
        let root = build_tree(
            kind,
            vec![
                tree_item("a", &leaf),
                TreeItem {
                    mode: TreeItemMode::Tree,
                    id: empty,
                    name: "e".to_owned(),
                },
            ],
        )
        .unwrap();
        let source = tree_source(kind, &[root.clone(), leaf], &[]);
        assert_twice(
            kind,
            &source,
            &parse_filter(":exclude[::a/x]"),
            &root.id.to_string(),
            &empty.to_string(),
            BTreeMap::from([(empty.to_string(), Vec::new())]),
        );

        let empty_source = tree_source(kind, &[], &[]);
        assert_twice(
            kind,
            &empty_source,
            &parse_filter(":prefix=p"),
            &empty.to_string(),
            &empty.to_string(),
            BTreeMap::from([(empty.to_string(), Vec::new())]),
        );
        for filter in [
            Filter::Compose(vec![
                Filter::Empty,
                Filter::Subdir(ViewPath::new(vec!["missing".to_owned()])),
            ]),
            Filter::Compose(vec![Filter::Empty, Filter::Empty]),
        ] {
            assert_twice(
                kind,
                &source,
                &filter,
                &root.id.to_string(),
                &empty.to_string(),
                BTreeMap::from([(empty.to_string(), Vec::new())]),
            );
        }

        let mut rng = StdRng::seed_from_u64(0x27_e17e);
        for _ in 0..32 {
            let filter = random_filter(&mut rng, 2);
            assert_twice(
                kind,
                &empty_source,
                &filter,
                &empty.to_string(),
                &empty.to_string(),
                BTreeMap::from([(empty.to_string(), Vec::new())]),
            );
        }
    }

    #[test]
    fn read_errors_match_design() {
        let kind = HashKind::Sha1;
        let _guard = set_hash_kind_for_test(HashKind::Sha256);
        let (root, a, b, c) = root_fixture(kind);
        let root_id = root.id.to_string();

        for (source, reason) in [
            (
                tree_source(kind, &[], std::slice::from_ref(&root_id)),
                MissingObjectReason::Absent,
            ),
            (
                tree_source(kind, &[], &[]),
                MissingObjectReason::Unprefetched,
            ),
        ] {
            for filter in [
                parse_filter(":/a"),
                parse_filter(":exclude[::a/x]"),
                parse_filter(":[ :/a:prefix=x, :/c:prefix=y ]"),
            ] {
                assert_missing(kind, &source, &filter, &root_id, &root_id, reason.clone());
            }
        }

        let a_id = a.id.to_string();
        for (source, reason) in [
            (
                tree_source(
                    kind,
                    std::slice::from_ref(&root),
                    std::slice::from_ref(&a_id),
                ),
                MissingObjectReason::Absent,
            ),
            (
                tree_source(kind, std::slice::from_ref(&root), &[]),
                MissingObjectReason::Unprefetched,
            ),
        ] {
            assert_missing(
                kind,
                &source,
                &parse_filter(":/a/b"),
                &root_id,
                &a_id,
                reason.clone(),
            );
            assert_missing(
                kind,
                &source,
                &parse_filter(":[ :/a/b:prefix=x, :/c:prefix=y ]"),
                &root_id,
                &a_id,
                reason.clone(),
            );
            assert_missing(
                kind,
                &source,
                &parse_filter(":/a:exclude[::c]"),
                &root_id,
                &a_id,
                reason,
            );
        }

        let b_id = b.id.to_string();
        for (source, reason) in [
            (
                tree_source(
                    kind,
                    &[root.clone(), a.clone()],
                    std::slice::from_ref(&b_id),
                ),
                MissingObjectReason::Absent,
            ),
            (
                tree_source(kind, &[root.clone(), a.clone()], &[]),
                MissingObjectReason::Unprefetched,
            ),
        ] {
            assert_missing(
                kind,
                &source,
                &parse_filter(":exclude[::a/b/x]"),
                &root_id,
                &b_id,
                reason,
            );
        }

        let sha256_source = InMemoryTreeSource::new(
            HashKind::Sha256,
            HashMap::from([(root_id.clone(), bytes(&root))]),
            HashSet::new(),
        );
        assert_missing(
            HashKind::Sha256,
            &sha256_source,
            &parse_filter(":/a"),
            &root_id,
            &root_id,
            MissingObjectReason::Malformed,
        );
        let mut truncated = bytes(&a);
        truncated.pop();
        let truncated_source = InMemoryTreeSource::new(
            kind,
            HashMap::from([(root_id.clone(), bytes(&root)), (a_id.clone(), truncated)]),
            HashSet::new(),
        );
        assert_missing(
            kind,
            &truncated_source,
            &parse_filter(":/a/b"),
            &root_id,
            &a_id,
            MissingObjectReason::Malformed,
        );

        let complete = tree_source(kind, &[root.clone(), a.clone(), b.clone(), c.clone()], &[]);
        for filter in [
            parse_filter(":/a"),
            parse_filter(":exclude[::a/x]"),
            parse_filter(":[ :/a:prefix=x, :/c:prefix=y ]"),
        ] {
            for missing in [true, false] {
                let failed = if missing {
                    tree_source(kind, &[], std::slice::from_ref(&root_id))
                } else {
                    tree_source(kind, &[], &[])
                };
                assert_missing_then_repaired(
                    kind, &filter, &root_id, &root_id, &failed, &complete, None,
                );
            }
        }
        let subdir = parse_filter(":/a/b");
        for missing in [true, false] {
            let failed = if missing {
                tree_source(
                    kind,
                    std::slice::from_ref(&root),
                    std::slice::from_ref(&a_id),
                )
            } else {
                tree_source(kind, std::slice::from_ref(&root), &[])
            };
            assert_missing_then_repaired(kind, &subdir, &root_id, &a_id, &failed, &complete, None);
        }
        let deep_exclude = parse_filter(":exclude[::a/b/x]");
        for missing in [true, false] {
            let failed = if missing {
                tree_source(
                    kind,
                    &[root.clone(), a.clone()],
                    std::slice::from_ref(&b_id),
                )
            } else {
                tree_source(kind, &[root.clone(), a.clone()], &[])
            };
            assert_missing_then_repaired(
                kind,
                &deep_exclude,
                &root_id,
                &b_id,
                &failed,
                &complete,
                None,
            );
        }
        let compose_left = parse_filter(":/a/b:prefix=x");
        let compose_right = parse_filter(":/c:prefix=y");
        let compose = Filter::Compose(vec![compose_right.clone(), compose_left.clone()]);
        for missing in [true, false] {
            let failed = if missing {
                tree_source(
                    kind,
                    &[root.clone(), b.clone(), c.clone()],
                    std::slice::from_ref(&a_id),
                )
            } else {
                tree_source(kind, &[root.clone(), b.clone(), c.clone()], &[])
            };
            assert_missing_then_repaired(
                kind,
                &compose,
                &root_id,
                &a_id,
                &failed,
                &complete,
                Some((&compose_right, &root_id)),
            );
        }
        let chain = parse_filter(":/a:exclude[::c]");
        let first_chain_node = parse_filter(":/a");
        for missing in [true, false] {
            let failed = if missing {
                tree_source(
                    kind,
                    std::slice::from_ref(&root),
                    std::slice::from_ref(&a_id),
                )
            } else {
                tree_source(kind, std::slice::from_ref(&root), &[])
            };
            assert_missing_then_repaired(
                kind,
                &chain,
                &root_id,
                &a_id,
                &failed,
                &complete,
                Some((&first_chain_node, &root_id)),
            );
        }
        assert_missing_then_repaired(
            kind,
            &subdir,
            &root_id,
            &a_id,
            &truncated_source,
            &complete,
            None,
        );

        let complete = tree_source(kind, &[root.clone(), a.clone(), b.clone(), c.clone()], &[]);
        let subdir = parse_filter(":/a/b");
        let expected = output(
            kind,
            &complete,
            &mut FilterMemo::default(),
            &subdir,
            &root_id,
        );
        for source in [
            tree_source(
                kind,
                &[root.clone(), a.clone(), c.clone()],
                std::slice::from_ref(&b_id),
            ),
            tree_source(kind, &[root.clone(), a.clone(), c.clone()], &[]),
        ] {
            assert_eq!(
                output(kind, &source, &mut FilterMemo::default(), &subdir, &root_id),
                expected
            );
        }

        let prefix = parse_filter(":prefix=p");
        let prefix_expected = build_tree(kind, vec![tree_item("p", &root)]).unwrap();
        let prefix_output = super::FilterOutput {
            tree_id: prefix_expected.id.to_string(),
            trees: BTreeMap::from([(prefix_expected.id.to_string(), bytes(&prefix_expected))]),
        };
        for source in [
            tree_source(kind, &[], &[]),
            tree_source(kind, &[], std::slice::from_ref(&root_id)),
        ] {
            assert_eq!(
                output(kind, &source, &mut FilterMemo::default(), &prefix, &root_id),
                prefix_output
            );
        }

        let deleted =
            build_tree(kind, vec![item(kind, TreeItemMode::Blob, "gone", b"gone")]).unwrap();
        let a = build_tree(kind, vec![tree_item("x", &deleted)]).unwrap();
        let root = build_tree(kind, vec![tree_item("a", &a)]).unwrap();
        let root_id = root.id.to_string();
        let deleted_id = deleted.id.to_string();
        let filter = parse_filter(":exclude[::a/x]");
        let expected = output(
            kind,
            &tree_source(kind, &[root.clone(), a.clone(), deleted], &[]),
            &mut FilterMemo::default(),
            &filter,
            &root_id,
        );
        for source in [
            tree_source(
                kind,
                &[root.clone(), a.clone()],
                std::slice::from_ref(&deleted_id),
            ),
            tree_source(kind, &[root.clone(), a], &[]),
        ] {
            assert_eq!(
                output(kind, &source, &mut FilterMemo::default(), &filter, &root_id),
                expected
            );
        }

        let selected = build_tree(
            kind,
            vec![
                item(kind, TreeItemMode::Blob, "x", b"x"),
                item(kind, TreeItemMode::Blob, "y", b"y"),
            ],
        )
        .unwrap();
        let sibling = build_tree(kind, vec![item(kind, TreeItemMode::Blob, "z", b"z")]).unwrap();
        let root = build_tree(
            kind,
            vec![tree_item("a", &selected), tree_item("c", &sibling)],
        )
        .unwrap();
        let root_id = root.id.to_string();
        let sibling_id = sibling.id.to_string();
        let filter = parse_filter(":exclude[::a/x]");
        let complete = tree_source(
            kind,
            &[root.clone(), selected.clone(), sibling.clone()],
            &[],
        );
        let expected = output(
            kind,
            &complete,
            &mut FilterMemo::default(),
            &filter,
            &root_id,
        );
        for source in [
            tree_source(
                kind,
                &[root.clone(), selected.clone()],
                std::slice::from_ref(&sibling_id),
            ),
            tree_source(kind, &[root.clone(), selected], &[]),
        ] {
            assert_eq!(
                output(kind, &source, &mut FilterMemo::default(), &filter, &root_id),
                expected
            );
        }

        let left = build_tree(kind, vec![item(kind, TreeItemMode::Blob, "l", b"l")]).unwrap();
        let right = build_tree(kind, vec![item(kind, TreeItemMode::Blob, "r", b"r")]).unwrap();
        let root = build_tree(kind, vec![tree_item("a", &left), tree_item("b", &right)]).unwrap();
        let root_id = root.id.to_string();
        let filter = parse_filter(":[ :/a:prefix=x, :/b:prefix=y ]");
        let expected = output(
            kind,
            &tree_source(kind, std::slice::from_ref(&root), &[]),
            &mut FilterMemo::default(),
            &filter,
            &root_id,
        );
        for source in [
            tree_source(
                kind,
                std::slice::from_ref(&root),
                &[left.id.to_string(), right.id.to_string()],
            ),
            tree_source(kind, std::slice::from_ref(&root), &[]),
        ] {
            assert_eq!(
                output(kind, &source, &mut FilterMemo::default(), &filter, &root_id),
                expected
            );
        }

        let empty = empty_tree_id(kind).unwrap().to_string();
        let root = build_tree(
            kind,
            vec![item(kind, TreeItemMode::Blob, "other", b"other")],
        )
        .unwrap();
        let root_id = root.id.to_string();
        let filter = parse_filter(":/nothere:exclude[::x]");
        for source in [
            tree_source(kind, std::slice::from_ref(&root), &[]),
            tree_source(
                kind,
                std::slice::from_ref(&root),
                std::slice::from_ref(&empty),
            ),
        ] {
            assert_eq!(
                output(kind, &source, &mut FilterMemo::default(), &filter, &root_id,),
                super::FilterOutput {
                    tree_id: empty.clone(),
                    trees: BTreeMap::from([(empty.clone(), Vec::new())]),
                }
            );
        }
    }

    #[test]
    fn invariant_violation_is_error() {
        let kind = HashKind::Sha1;
        let _guard = set_hash_kind_for_test(HashKind::Sha256);
        let a = build_tree(kind, vec![item(kind, TreeItemMode::Blob, "f", b"a")]).unwrap();
        let b = build_tree(kind, vec![item(kind, TreeItemMode::Blob, "f", b"b")]).unwrap();
        let root = build_tree(kind, vec![tree_item("a", &a), tree_item("b", &b)]).unwrap();
        let source = tree_source(kind, &[root.clone(), a, b], &[]);
        let conflict = Filter::Compose(vec![
            Filter::Chain(vec![
                Filter::Subdir(ViewPath::new(vec!["a".to_owned()])),
                Filter::Prefix(ViewPath::new(vec!["x".to_owned()])),
            ]),
            Filter::Chain(vec![
                Filter::Subdir(ViewPath::new(vec!["b".to_owned()])),
                Filter::Prefix(ViewPath::new(vec!["x".to_owned()])),
            ]),
        ]);
        assert_eq!(
            filter_tree(
                kind,
                &source,
                &mut FilterMemo::default(),
                &conflict,
                &root.id.to_string(),
            ),
            Err(FilterTreeError::Invariant)
        );

        assert_eq!(
            filter_tree(
                HashKind::Sha256,
                &source,
                &mut FilterMemo::default(),
                &parse_filter(":prefix=p"),
                &root.id.to_string(),
            ),
            Err(FilterTreeError::Invariant)
        );

        let nested =
            build_tree(kind, vec![item(kind, TreeItemMode::Blob, "leaf", b"leaf")]).unwrap();
        let left = build_tree(kind, vec![item(kind, TreeItemMode::Blob, "f", b"left")]).unwrap();
        let right = build_tree(kind, vec![tree_item("f", &nested)]).unwrap();
        let root = build_tree(kind, vec![tree_item("a", &left), tree_item("b", &right)]).unwrap();
        let source = tree_source(kind, &[root.clone(), left, right, nested], &[]);
        assert_eq!(
            filter_tree(
                kind,
                &source,
                &mut FilterMemo::default(),
                &parse_filter(":[:/a:prefix=x,:/b:prefix=x/f]"),
                &root.id.to_string(),
            ),
            Err(FilterTreeError::Invariant)
        );
    }

    #[test]
    fn canonicalization_sound_random_trees() {
        const SEED: u64 = 0x27ca_1102;
        const DIRECTED_TREE_SAMPLES: usize = 32;
        const RANDOM_ATTEMPTS: usize = 256;
        const MIN_ACCEPTED: usize = 48;
        let kind = HashKind::Sha1;
        let _guard = set_hash_kind_for_test(HashKind::Sha256);
        let mut rng = StdRng::seed_from_u64(SEED);
        let mut modes = HashSet::new();
        let mut empty_tree_entries = 0;
        let directed = vec![
            parse(":/a:/b").unwrap(),
            parse(":prefix=a:prefix=b").unwrap(),
            parse(":prefix=a:prefix=b:prefix=c").unwrap(),
            parse(":prefix=p/q:/p/q").unwrap(),
            parse(":prefix=p/q:/p").unwrap(),
            parse(":prefix=p:/p/q").unwrap(),
            parse(":prefix=p:/q").unwrap(),
            parse(":nop:/a").unwrap(),
            parse(":/a:empty").unwrap(),
            parse(":[ :/a, :empty ]").unwrap(),
            parse(":[ :empty, :empty ]").unwrap(),
            parse(":[ :/b:prefix=y, :[ :/c:prefix=z, :/a:prefix=x ] ]").unwrap(),
            parse(":[ :/a:prefix=x, :empty, :/b:prefix=y ]").unwrap(),
            parse(":exclude[::b/,::a,::a]").unwrap(),
            Filter::Exclude(Vec::new()),
            parse(":empty:prefix=a").unwrap(),
        ];

        for _ in 0..DIRECTED_TREE_SAMPLES {
            let mut trees = Vec::new();
            let mut next = 0;
            let a = random_tree(
                kind,
                &mut rng,
                2,
                &mut next,
                &mut trees,
                &mut modes,
                &mut empty_tree_entries,
            );
            let b = random_tree(
                kind,
                &mut rng,
                2,
                &mut next,
                &mut trees,
                &mut modes,
                &mut empty_tree_entries,
            );
            let c = random_tree(
                kind,
                &mut rng,
                2,
                &mut next,
                &mut trees,
                &mut modes,
                &mut empty_tree_entries,
            );
            let root = build_tree(
                kind,
                vec![
                    tree_item("a", &a),
                    tree_item("b", &b),
                    tree_item("c", &c),
                    item(kind, TreeItemMode::Blob, "file", b"file"),
                ],
            )
            .unwrap();
            trees.push(root.clone());
            let source = tree_source(kind, &trees, &[]);
            for raw in &directed {
                assert_same_tree_id(kind, &source, raw, &root.id.to_string());
            }
        }

        let empty = empty_tree_id(kind).unwrap();
        let a = build_tree(
            kind,
            vec![
                TreeItem {
                    mode: TreeItemMode::Tree,
                    id: empty,
                    name: "empty-child".to_owned(),
                },
                item(kind, TreeItemMode::Blob, "kept", b"kept"),
            ],
        )
        .unwrap();
        let root = build_tree(kind, vec![tree_item("a", &a)]).unwrap();
        let source = tree_source(kind, &[root.clone(), a], &[]);
        assert_same_tree_id(
            kind,
            &source,
            &parse(":[ :/a, :empty ]").unwrap(),
            &root.id.to_string(),
        );

        let mut accepted = 0;
        for _ in 0..RANDOM_ATTEMPTS {
            let raw = random_filter(&mut rng, 3);
            let canonical = canonicalize(raw.clone());
            if !compose_members_are_disjoint(&raw) || !compose_members_are_disjoint(&canonical) {
                continue;
            }
            let mut trees = Vec::new();
            let mut next = 0;
            let a = random_tree(
                kind,
                &mut rng,
                2,
                &mut next,
                &mut trees,
                &mut modes,
                &mut empty_tree_entries,
            );
            let b = random_tree(
                kind,
                &mut rng,
                2,
                &mut next,
                &mut trees,
                &mut modes,
                &mut empty_tree_entries,
            );
            let c = random_tree(
                kind,
                &mut rng,
                2,
                &mut next,
                &mut trees,
                &mut modes,
                &mut empty_tree_entries,
            );
            let root = build_tree(
                kind,
                vec![
                    tree_item("a", &a),
                    tree_item("b", &b),
                    tree_item("c", &c),
                    item(kind, TreeItemMode::Blob, "file", b"file"),
                ],
            )
            .unwrap();
            trees.push(root.clone());
            let source = tree_source(kind, &trees, &[]);
            assert_same_tree_id(kind, &source, &raw, &root.id.to_string());
            accepted += 1;
        }

        assert!(accepted >= MIN_ACCEPTED, "accepted {accepted}");
        assert_eq!(
            modes,
            HashSet::from([
                TreeItemMode::Blob,
                TreeItemMode::BlobExecutable,
                TreeItemMode::Link,
                TreeItemMode::Commit,
                TreeItemMode::Tree,
            ])
        );
        assert!(empty_tree_entries > 0);
    }

    #[test]
    fn src_paths_read_property() {
        const SEED: u64 = 0x27_5ac;
        const RANDOM_ATTEMPTS: usize = 512;
        const MIN_ACCEPTED: usize = 64;
        const MIN_DISTINCT: usize = 32;
        let kind = HashKind::Sha1;
        let _guard = set_hash_kind_for_test(HashKind::Sha256);
        let mut rng = StdRng::seed_from_u64(SEED);
        let mut modes = HashSet::new();
        let mut empty_tree_entries = 0;
        let mut accepted = 0;
        let mut distinct = 0;

        for sample in 0..RANDOM_ATTEMPTS {
            let filter = random_filter(&mut rng, 3);
            let canonical = canonicalize(filter.clone());
            if !compose_members_are_disjoint(&filter) || !compose_members_are_disjoint(&canonical) {
                continue;
            }
            let source_paths = src_paths(&filter);
            assert!(
                source_paths.iter().all(|path| {
                    path == "/"
                        || path == "/a"
                        || path.starts_with("/a/")
                        || path == "/b"
                        || path.starts_with("/b/")
                        || path == "/c"
                        || path.starts_with("/c/")
                }),
                "unexpected source paths: {source_paths:?}"
            );

            let mut source_trees = Vec::new();
            let mut next = 0;
            let a = random_tree(
                kind,
                &mut rng,
                2,
                &mut next,
                &mut source_trees,
                &mut modes,
                &mut empty_tree_entries,
            );
            let b = random_tree(
                kind,
                &mut rng,
                2,
                &mut next,
                &mut source_trees,
                &mut modes,
                &mut empty_tree_entries,
            );
            let c = random_tree(
                kind,
                &mut rng,
                2,
                &mut next,
                &mut source_trees,
                &mut modes,
                &mut empty_tree_entries,
            );
            let mut outside_trees = Vec::new();
            let outside_one = random_tree(
                kind,
                &mut rng,
                1,
                &mut next,
                &mut outside_trees,
                &mut modes,
                &mut empty_tree_entries,
            );
            let outside_two = random_tree(
                kind,
                &mut rng,
                1,
                &mut next,
                &mut outside_trees,
                &mut modes,
                &mut empty_tree_entries,
            );
            let external_items = vec![
                tree_item("outside-tree", &outside_one),
                item(
                    kind,
                    TreeItemMode::Blob,
                    "outside-blob",
                    format!("one-{sample}").as_bytes(),
                ),
                item(
                    kind,
                    TreeItemMode::BlobExecutable,
                    "outside-exec",
                    format!("one-exec-{sample}").as_bytes(),
                ),
                item(
                    kind,
                    TreeItemMode::Link,
                    "outside-link",
                    format!("one-link-{sample}").as_bytes(),
                ),
                item(
                    kind,
                    TreeItemMode::Commit,
                    "outside-commit",
                    format!("one-commit-{sample}").as_bytes(),
                ),
            ];
            let mut root_one_items =
                vec![tree_item("a", &a), tree_item("b", &b), tree_item("c", &c)];
            root_one_items.extend(external_items.clone());
            let root_one = build_tree(kind, root_one_items).unwrap();
            let root_two = if source_paths.contains(&"/".to_owned()) {
                root_one.clone()
            } else {
                assert!(
                    source_paths
                        .iter()
                        .all(|path| !path_overlap(path, "/outside-tree")),
                    "outside mutation overlaps source: {source_paths:?}"
                );
                let mut root_two_items =
                    vec![tree_item("a", &a), tree_item("b", &b), tree_item("c", &c)];
                let mut changed_external = external_items;
                match rng.random_range(0..4) {
                    0 => changed_external
                        .iter_mut()
                        .find(|item| item.name == "outside-blob")
                        .map(|item| item.id = blob(kind, format!("changed-{sample}").as_bytes()))
                        .unwrap(),
                    1 => changed_external.retain(|item| item.name != "outside-link"),
                    2 => changed_external.push(item(
                        kind,
                        TreeItemMode::Blob,
                        "outside-added",
                        format!("added-{sample}").as_bytes(),
                    )),
                    _ => changed_external
                        .iter_mut()
                        .find(|item| item.name == "outside-tree")
                        .map(|item| item.id = outside_two.id)
                        .unwrap(),
                }
                root_two_items.extend(changed_external);
                build_tree(kind, root_two_items).unwrap()
            };
            let mut first_trees = vec![root_one.clone()];
            first_trees.extend(source_trees.clone());
            first_trees.extend(outside_trees);
            let first = tree_source(kind, &first_trees, &[]);
            let mut second_trees = vec![root_two.clone()];
            second_trees.extend(source_trees);
            let second = tree_source(kind, &second_trees, &[]);
            assert_eq!(
                output(
                    kind,
                    &first,
                    &mut FilterMemo::default(),
                    &filter,
                    &root_one.id.to_string(),
                )
                .tree_id,
                output(
                    kind,
                    &second,
                    &mut FilterMemo::default(),
                    &filter,
                    &root_two.id.to_string(),
                )
                .tree_id,
                "{}",
                print(&filter)
            );
            if root_one.id != root_two.id {
                distinct += 1;
            }
            accepted += 1;
        }

        assert!(accepted >= MIN_ACCEPTED, "accepted {accepted}");
        assert!(distinct >= MIN_DISTINCT, "distinct {distinct}");
        assert_eq!(
            modes,
            HashSet::from([
                TreeItemMode::Blob,
                TreeItemMode::BlobExecutable,
                TreeItemMode::Link,
                TreeItemMode::Commit,
                TreeItemMode::Tree,
            ])
        );
        assert!(empty_tree_entries > 0);
    }
}
