use std::collections::BTreeMap;

use git_internal::{
    hash::{HashKind, ObjectHash},
    internal::object::tree::{Tree, TreeItemMode},
};

use super::{
    commit::{RewriteError, rewrite_commit},
    filter::{Filter, filter_id, print},
    tree::{FilterMemo, FilterOutput, FilterTreeError, filter_tree},
    tree_source::{MissingObject, TreeSource, parse_tree_bytes, read_tree},
};
use crate::callisto::mega_commit;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProjectionInput<'a> {
    pub seq: i64,
    pub commit_id: &'a str,
    pub tree_id: &'a str,
    pub row: Option<&'a mega_commit::Model>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PreviousProjection {
    pub view_commit: Option<String>,
    pub view_tree: String,
}

#[derive(Clone, Debug)]
pub(crate) struct ProjectionRequest<'input, 'state> {
    pub input: ProjectionInput<'input>,
    pub parent_tree: &'state str,
    pub prev: &'state PreviousProjection,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProjectedCommit {
    pub id: ObjectHash,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Segment {
    pub seq: i64,
    pub view_commit: Option<ProjectedCommit>,
    pub view_tree: String,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ProjectError {
    #[error(transparent)]
    Premise(RewriteError),
    #[error("tree object is unavailable: {0:?}")]
    MissingObject(MissingObject),
    #[error("root-chain commit row is missing: {commit_id}")]
    MissingCommit { commit_id: String },
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ProjectFailure {
    #[error(transparent)]
    Data(ProjectError),
    #[error("projection invariant failed")]
    Internal,
}

pub(crate) fn project_commit<S: TreeSource + ?Sized>(
    kind: HashKind,
    source: &S,
    memo: &mut FilterMemo,
    filter: &Filter,
    input: ProjectionInput<'_>,
    parent_tree: &str,
    prev: &PreviousProjection,
) -> Result<Option<Segment>, ProjectFailure> {
    let filtered =
        filter_tree(kind, source, memo, filter, input.tree_id).map_err(map_filter_error)?;
    project_with_output(
        kind,
        source,
        memo,
        filter,
        ProjectionRequest {
            input,
            parent_tree,
            prev,
        },
        &filtered,
    )
}

pub(crate) fn project_with_output<S: TreeSource + ?Sized>(
    kind: HashKind,
    source: &S,
    memo: &mut FilterMemo,
    filter: &Filter,
    request: ProjectionRequest<'_, '_>,
    filtered: &FilterOutput,
) -> Result<Option<Segment>, ProjectFailure> {
    let result = project_from_output(
        kind,
        source,
        request.input.clone(),
        request.parent_tree,
        request.prev,
        filtered,
    )?;
    if should_detect_r8(filter, request.prev, &result) {
        detect_r8(kind, source, memo, filter, &request.input);
    }
    Ok(result)
}

pub(crate) fn is_empty_root<S: TreeSource + ?Sized>(
    kind: HashKind,
    source: &S,
    tree_id: &str,
) -> Result<bool, MissingObject> {
    let tree = read_tree(kind, source, tree_id)?;
    if tree
        .tree_items
        .iter()
        .any(|item| item.mode != TreeItemMode::Tree)
    {
        return Ok(false);
    }
    for item in tree.tree_items {
        if !is_empty_root(kind, source, &item.id.to_string())? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn project_from_output<S: TreeSource + ?Sized>(
    kind: HashKind,
    source: &S,
    input: ProjectionInput<'_>,
    parent_tree: &str,
    prev: &PreviousProjection,
    filtered: &FilterOutput,
) -> Result<Option<Segment>, ProjectFailure> {
    if prev.view_commit.is_none() {
        if input.seq == 1 && is_empty_root(kind, source, input.tree_id).map_err(data_missing)? {
            return write_segment(kind, input, filtered.tree_id.clone(), Vec::new()).map(Some);
        }
        if is_empty_tree(kind, &filtered.tree_id) {
            return Ok((input.seq == 1).then(|| Segment {
                seq: 1,
                view_commit: None,
                view_tree: filtered.tree_id.clone(),
            }));
        }
        return write_segment(kind, input, filtered.tree_id.clone(), Vec::new()).map(Some);
    }

    if input.tree_id == parent_tree {
        return write_segment(
            kind,
            input,
            filtered.tree_id.clone(),
            prev.view_commit.clone().into_iter().collect(),
        )
        .map(Some);
    }
    if filtered.tree_id == prev.view_tree {
        return Ok(None);
    }
    write_segment(
        kind,
        input,
        filtered.tree_id.clone(),
        prev.view_commit.clone().into_iter().collect(),
    )
    .map(Some)
}

fn write_segment(
    kind: HashKind,
    input: ProjectionInput<'_>,
    tree_id: String,
    parents: Vec<String>,
) -> Result<Segment, ProjectFailure> {
    let row = input.row.ok_or_else(|| {
        ProjectFailure::Data(ProjectError::MissingCommit {
            commit_id: input.commit_id.to_owned(),
        })
    })?;
    let (id, bytes) = rewrite_commit(kind, row, &tree_id, &parents)
        .map_err(|error| ProjectFailure::Data(ProjectError::Premise(error)))?;
    Ok(Segment {
        seq: input.seq,
        view_commit: Some(ProjectedCommit { id, bytes }),
        view_tree: tree_id,
    })
}

fn map_filter_error(error: FilterTreeError) -> ProjectFailure {
    match error {
        FilterTreeError::Missing(error) => data_missing(error),
        FilterTreeError::Invariant => ProjectFailure::Internal,
    }
}

fn data_missing(error: MissingObject) -> ProjectFailure {
    ProjectFailure::Data(ProjectError::MissingObject(error))
}

fn is_empty_tree(kind: HashKind, tree_id: &str) -> bool {
    super::tree_source::empty_tree_id(kind).is_ok_and(|empty| tree_id == empty.to_string())
}

fn should_detect_r8(filter: &Filter, prev: &PreviousProjection, result: &Option<Segment>) -> bool {
    prev.view_commit.is_none()
        && matches!(filter, Filter::Chain(ops) if ops.len() >= 2)
        && matches!(
            result,
            None | Some(Segment {
                view_commit: None,
                ..
            })
        )
}

fn detect_r8<S: TreeSource + ?Sized>(
    kind: HashKind,
    source: &S,
    memo: &mut FilterMemo,
    filter: &Filter,
    input: &ProjectionInput<'_>,
) {
    let Filter::Chain(ops) = filter else {
        return;
    };
    let filter_id = filter_id(&print(filter));
    let mut available = BTreeMap::new();
    let mut tree_id = input.tree_id.to_owned();

    for (index, op) in ops.iter().take(ops.len() - 1).enumerate() {
        let level = index + 1;
        let output = {
            let overlay = OverlayTreeSource {
                kind,
                available: &available,
                source,
            };
            filter_tree(kind, &overlay, memo, op, &tree_id)
        };
        let output = match output {
            Ok(output) => output,
            Err(_) => return r8_detection_skipped(&filter_id, input.seq, level),
        };
        available.extend(output.trees);
        tree_id = output.tree_id;

        let empty = {
            let overlay = OverlayTreeSource {
                kind,
                available: &available,
                source,
            };
            is_empty_root(kind, &overlay, &tree_id)
        };
        match empty {
            Ok(true) => {
                if !is_empty_tree(kind, &tree_id) {
                    tracing::warn!(
                        metric = "view_r8_intermediate_empty_root_total",
                        filter_id = %filter_id,
                        seq = input.seq,
                        level,
                        tree_id = %tree_id,
                        "view R8 intermediate empty root detected"
                    );
                    return;
                }
            }
            Ok(false) => {}
            Err(_) => return r8_detection_skipped(&filter_id, input.seq, level),
        }
    }
}

fn r8_detection_skipped(filter_id: &str, seq: i64, level: usize) {
    tracing::debug!(
        check = "view_r8_detection_skipped",
        filter_id = %filter_id,
        seq,
        level,
        "view R8 detection skipped"
    );
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

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use chrono::Utc;
    use git_internal::{
        hash::{HashKind, ObjectHash, set_hash_kind_for_test},
        internal::object::{
            ObjectTrait,
            tree::{Tree, TreeItem, TreeItemMode},
            types::ObjectType,
        },
    };

    use super::{
        PreviousProjection, ProjectionInput, ProjectionRequest, Segment, project_commit,
        project_with_output,
    };
    use crate::{
        callisto::mega_commit,
        ceres::view::{
            filter::{canonicalize, parse},
            tree::{FilterMemo, FilterOutput, filter_tree},
            tree_source::{InMemoryTreeSource, build_tree, empty_tree_id},
        },
    };

    const AUTHOR: &str = "author Alice <alice@example.com> 1 +0000";
    const COMMITTER: &str = "committer Bob <bob@example.com> 2 +0000";
    const ROOT_PARENT: &str = "1111111111111111111111111111111111111111";
    const VIEW_PARENT: &str = "2222222222222222222222222222222222222222";

    fn blob(kind: HashKind, label: &[u8]) -> ObjectHash {
        ObjectHash::from_type_and_data_for_kind(kind, ObjectType::Blob, label).unwrap()
    }

    fn blob_item(kind: HashKind, name: &str, label: &[u8]) -> TreeItem {
        TreeItem {
            mode: TreeItemMode::Blob,
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

    fn make_source(kind: HashKind, trees: &[Tree], missing: &[String]) -> InMemoryTreeSource {
        InMemoryTreeSource::new(
            kind,
            trees
                .iter()
                .map(|tree| (tree.id.to_string(), tree.to_data().unwrap()))
                .collect::<HashMap<_, _>>(),
            missing.iter().cloned().collect::<HashSet<_>>(),
        )
    }

    fn make_row(kind: HashKind, tree: &str, parents: &[&str]) -> mega_commit::Model {
        make_row_with_content(kind, tree, parents, "\nmessage\n")
    }

    fn make_row_with_content(
        kind: HashKind,
        tree: &str,
        parents: &[&str],
        content: &str,
    ) -> mega_commit::Model {
        let mut raw = format!("tree {tree}\n");
        for parent in parents {
            raw.push_str(&format!("parent {parent}\n"));
        }
        raw.push_str(AUTHOR);
        raw.push('\n');
        raw.push_str(COMMITTER);
        raw.push('\n');
        raw.push_str(content);
        let commit_id =
            ObjectHash::from_type_and_data_for_kind(kind, ObjectType::Commit, raw.as_bytes())
                .unwrap()
                .to_string();
        mega_commit::Model {
            id: 1,
            commit_id,
            tree: tree.to_owned(),
            parents_id: serde_json::json!(parents),
            author: Some(AUTHOR.to_owned()),
            committer: Some(COMMITTER.to_owned()),
            content: Some(content.to_owned()),
            created_at: Utc::now().naive_utc(),
            pack_id: String::new(),
            pack_offset: 0,
        }
    }

    fn input<'a>(seq: i64, row: Option<&'a mega_commit::Model>) -> ProjectionInput<'a> {
        let (commit_id, tree_id) = row
            .map(|row| (row.commit_id.as_str(), row.tree.as_str()))
            .unwrap_or(("missing", "missing"));
        ProjectionInput {
            seq,
            commit_id,
            tree_id,
            row,
        }
    }

    fn no_prev(empty: &str) -> PreviousProjection {
        PreviousProjection {
            view_commit: None,
            view_tree: empty.to_owned(),
        }
    }

    fn prev(tree: &str) -> PreviousProjection {
        PreviousProjection {
            view_commit: Some(VIEW_PARENT.to_owned()),
            view_tree: tree.to_owned(),
        }
    }

    fn filter(text: &str) -> crate::ceres::view::filter::Filter {
        canonicalize(parse(text).unwrap())
    }

    fn assert_equivalent(
        kind: HashKind,
        source: &InMemoryTreeSource,
        filter: &crate::ceres::view::filter::Filter,
        row: &mega_commit::Model,
        seq: i64,
        parent_tree: &str,
        prev: &PreviousProjection,
    ) -> Option<Segment> {
        let mut direct_memo = FilterMemo::default();
        let direct = project_commit(
            kind,
            source,
            &mut direct_memo,
            filter,
            input(seq, Some(row)),
            parent_tree,
            prev,
        )
        .unwrap();
        let mut output_memo = FilterMemo::default();
        let output = filter_tree(kind, source, &mut output_memo, filter, &row.tree).unwrap();
        let from_output = project_with_output(
            kind,
            source,
            &mut output_memo,
            filter,
            ProjectionRequest {
                input: input(seq, Some(row)),
                parent_tree,
                prev,
            },
            &output,
        )
        .unwrap();
        assert_eq!(from_output, direct);
        direct
    }

    fn expected_segment(
        kind: HashKind,
        row: &mega_commit::Model,
        seq: i64,
        tree_id: &str,
        parents: &[String],
    ) -> Segment {
        let (id, bytes) = super::rewrite_commit(kind, row, tree_id, parents).unwrap();
        Segment {
            seq,
            view_commit: Some(super::ProjectedCommit { id, bytes }),
            view_tree: tree_id.to_owned(),
        }
    }

    #[test]
    fn rule_branches() {
        let _hash_kind_guard = set_hash_kind_for_test(HashKind::Sha256);
        let kind = HashKind::Sha1;
        let empty = empty_tree_id(kind).unwrap().to_string();
        let root = build_tree(kind, vec![blob_item(kind, "README", b"readme")]).unwrap();
        let source = make_source(kind, std::slice::from_ref(&root), &[]);
        let row = make_row(kind, &root.id.to_string(), &[ROOT_PARENT]);
        let nop = filter(":nop");

        assert_eq!(
            assert_equivalent(kind, &source, &nop, &row, 1, &empty, &no_prev(&empty)),
            Some(expected_segment(kind, &row, 1, &root.id.to_string(), &[]))
        );

        let empty_filter = filter(":empty");
        assert_eq!(
            assert_equivalent(
                kind,
                &source,
                &empty_filter,
                &row,
                1,
                &empty,
                &no_prev(&empty),
            ),
            Some(Segment {
                seq: 1,
                view_commit: None,
                view_tree: empty.clone(),
            })
        );
        assert_eq!(
            assert_equivalent(
                kind,
                &source,
                &empty_filter,
                &row,
                2,
                &empty,
                &no_prev(&empty),
            ),
            None
        );

        let unchanged = assert_equivalent(
            kind,
            &source,
            &nop,
            &row,
            2,
            &root.id.to_string(),
            &prev(&root.id.to_string()),
        );
        assert_eq!(
            unchanged,
            Some(expected_segment(
                kind,
                &row,
                2,
                &root.id.to_string(),
                &[VIEW_PARENT.to_owned()],
            ))
        );

        let j4 = assert_equivalent(
            kind,
            &source,
            &nop,
            &row,
            2,
            "different-parent-tree",
            &prev(&root.id.to_string()),
        );
        assert_eq!(j4, None);

        let cleared = assert_equivalent(
            kind,
            &source,
            &empty_filter,
            &row,
            2,
            "different-parent-tree",
            &prev(&root.id.to_string()),
        );
        assert_eq!(
            cleared,
            Some(expected_segment(
                kind,
                &row,
                2,
                &empty,
                &[VIEW_PARENT.to_owned()],
            ))
        );

        let empty_row = make_row(kind, &empty, &[]);
        let empty_root = assert_equivalent(
            kind,
            &make_source(kind, &[], &[]),
            &nop,
            &empty_row,
            1,
            &empty,
            &no_prev(&empty),
        );
        assert_eq!(
            empty_root,
            Some(expected_segment(kind, &empty_row, 1, &empty, &[]))
        );

        assert_equivalent_filters_project_identical_history();
        assert_nested_empty_root_and_subdir_do_not_overread();
        assert_prefix_short_circuits_empty_root();
    }

    fn assert_equivalent_filters_project_identical_history() {
        let _hash_kind_guard = set_hash_kind_for_test(HashKind::Sha256);
        let kind = HashKind::Sha1;
        let empty = empty_tree_id(kind).unwrap().to_string();
        let b1 = build_tree(kind, vec![blob_item(kind, "file", b"first")]).unwrap();
        let b2 = build_tree(kind, vec![blob_item(kind, "file", b"second")]).unwrap();
        let a1 = build_tree(
            kind,
            vec![
                tree_item("b", &b1),
                blob_item(kind, "kept", b"first kept value"),
            ],
        )
        .unwrap();
        let a2 = build_tree(
            kind,
            vec![
                tree_item("b", &b2),
                blob_item(kind, "kept", b"first kept value"),
            ],
        )
        .unwrap();
        let a3 = build_tree(
            kind,
            vec![
                tree_item("b", &b2),
                blob_item(kind, "kept", b"second kept value"),
            ],
        )
        .unwrap();
        let roots = [
            build_tree(
                kind,
                vec![tree_item("a", &a1), blob_item(kind, "outside", b"outside")],
            )
            .unwrap(),
            build_tree(
                kind,
                vec![tree_item("a", &a2), blob_item(kind, "outside", b"outside")],
            )
            .unwrap(),
            build_tree(
                kind,
                vec![
                    tree_item("a", &a2),
                    blob_item(kind, "outside", b"changed outside"),
                ],
            )
            .unwrap(),
            build_tree(
                kind,
                vec![
                    tree_item("a", &a3),
                    blob_item(kind, "outside", b"changed outside"),
                ],
            )
            .unwrap(),
        ];
        let rows = roots
            .iter()
            .map(|root| make_row(kind, &root.id.to_string(), &[ROOT_PARENT]))
            .collect::<Vec<_>>();
        let source = make_source(
            kind,
            &[
                roots[0].clone(),
                roots[1].clone(),
                roots[2].clone(),
                roots[3].clone(),
                a1,
                a2,
                a3,
                b1,
                b2,
            ],
            &[],
        );
        let left = filter(":/a:exclude[::b/]");
        let right = filter(":exclude[::a/b/]:/a");
        assert_ne!(
            crate::ceres::view::filter::print(&left),
            crate::ceres::view::filter::print(&right)
        );

        let mut left_prev = no_prev(&empty);
        let mut right_prev = no_prev(&empty);
        for (index, row) in rows.iter().enumerate() {
            let seq = i64::try_from(index + 1).unwrap();
            let parent_tree = if index == 0 {
                empty.as_str()
            } else {
                rows[index - 1].tree.as_str()
            };
            let left_segment = project_commit(
                kind,
                &source,
                &mut FilterMemo::default(),
                &left,
                input(seq, Some(row)),
                parent_tree,
                &left_prev,
            )
            .unwrap();
            let right_segment = project_commit(
                kind,
                &source,
                &mut FilterMemo::default(),
                &right,
                input(seq, Some(row)),
                parent_tree,
                &right_prev,
            )
            .unwrap();
            assert_eq!(left_segment, right_segment, "seq={seq}");
            if let Some(segment) = left_segment {
                left_prev = PreviousProjection {
                    view_commit: segment.view_commit.map(|commit| commit.id.to_string()),
                    view_tree: segment.view_tree,
                };
                right_prev = left_prev.clone();
            }
        }
    }

    fn assert_nested_empty_root_and_subdir_do_not_overread() {
        let _hash_kind_guard = set_hash_kind_for_test(HashKind::Sha256);
        let kind = HashKind::Sha1;
        let empty = empty_tree_id(kind).unwrap().to_string();
        let leaf = build_tree(kind, Vec::new()).unwrap();
        let nested = build_tree(kind, vec![tree_item("leaf", &leaf)]).unwrap();
        let root = build_tree(kind, vec![tree_item("nested", &nested)]).unwrap();
        let root_row = make_row(kind, &root.id.to_string(), &[ROOT_PARENT]);
        let root_source = make_source(kind, &[root.clone(), nested.clone()], &[]);
        let result = assert_equivalent(
            kind,
            &root_source,
            &filter(":nop"),
            &root_row,
            1,
            &empty,
            &no_prev(&empty),
        );
        assert_eq!(
            result,
            Some(expected_segment(
                kind,
                &root_row,
                1,
                &root.id.to_string(),
                &[]
            ))
        );

        let target = build_tree(kind, vec![blob_item(kind, "file", b"x")]).unwrap();
        let branch = build_tree(kind, vec![tree_item("target", &target)]).unwrap();
        let unrelated = build_tree(kind, vec![blob_item(kind, "hidden", b"hidden")]).unwrap();
        let selected_root = build_tree(
            kind,
            vec![
                blob_item(kind, "README", b"readme"),
                tree_item("branch", &branch),
                tree_item("unrelated", &unrelated),
            ],
        )
        .unwrap();
        let selected_row = make_row(kind, &selected_root.id.to_string(), &[ROOT_PARENT]);
        let source = make_source(
            kind,
            &[selected_root.clone(), branch.clone()],
            &[target.id.to_string(), unrelated.id.to_string()],
        );
        let result = assert_equivalent(
            kind,
            &source,
            &filter(":/branch/target"),
            &selected_row,
            1,
            &empty,
            &no_prev(&empty),
        );
        assert_eq!(
            result,
            Some(expected_segment(
                kind,
                &selected_row,
                1,
                &target.id.to_string(),
                &[]
            ))
        );

        let root = build_tree(
            kind,
            vec![
                blob_item(kind, "README", b"readme"),
                tree_item("branch", &branch),
            ],
        )
        .unwrap();
        let row = make_row(kind, &root.id.to_string(), &[ROOT_PARENT]);
        let source = make_source(
            kind,
            std::slice::from_ref(&root),
            &[branch.id.to_string(), target.id.to_string()],
        );
        let expected = expected_segment(kind, &row, 1, &target.id.to_string(), &[]);
        let result = project_with_output(
            kind,
            &source,
            &mut FilterMemo::default(),
            &filter(":/branch/target"),
            ProjectionRequest {
                input: input(1, Some(&row)),
                parent_tree: &empty,
                prev: &no_prev(&empty),
            },
            &FilterOutput {
                tree_id: target.id.to_string(),
                trees: Default::default(),
            },
        );
        assert_eq!(result, Ok(Some(expected)));
    }

    fn assert_prefix_short_circuits_empty_root() {
        let _hash_kind_guard = set_hash_kind_for_test(HashKind::Sha256);
        let kind = HashKind::Sha1;
        let empty = empty_tree_id(kind).unwrap().to_string();
        let unread = blob(kind, b"unread tree");
        let root = build_tree(
            kind,
            vec![
                TreeItem {
                    mode: TreeItemMode::Tree,
                    id: unread,
                    name: "a".to_owned(),
                },
                blob_item(kind, "z", b"visible blob"),
            ],
        )
        .unwrap();
        let source = make_source(kind, std::slice::from_ref(&root), &[unread.to_string()]);
        assert_eq!(
            super::is_empty_root(kind, &source, &root.id.to_string()),
            Ok(false)
        );
        let row = make_row(kind, &root.id.to_string(), &[ROOT_PARENT]);
        let prefix = filter(":prefix=p");
        let prefixed =
            assert_equivalent(kind, &source, &prefix, &row, 1, &empty, &no_prev(&empty)).unwrap();
        let filtered = filter_tree(
            kind,
            &source,
            &mut FilterMemo::default(),
            &prefix,
            &root.id.to_string(),
        )
        .unwrap();
        assert_eq!(
            prefixed,
            expected_segment(kind, &row, 1, &filtered.tree_id, &[])
        );

        let nonempty_child = build_tree(kind, vec![blob_item(kind, "f", b"f")]).unwrap();
        let unread_sibling = blob(kind, b"unread sibling");
        let root = build_tree(
            kind,
            vec![
                tree_item("a", &nonempty_child),
                TreeItem {
                    mode: TreeItemMode::Tree,
                    id: unread_sibling,
                    name: "b".to_owned(),
                },
            ],
        )
        .unwrap();
        let source = make_source(
            kind,
            &[root.clone(), nonempty_child],
            &[unread_sibling.to_string()],
        );
        assert_eq!(
            super::is_empty_root(kind, &source, &root.id.to_string()),
            Ok(false)
        );
        let row = make_row(kind, &root.id.to_string(), &[ROOT_PARENT]);
        let prefixed =
            assert_equivalent(kind, &source, &prefix, &row, 1, &empty, &no_prev(&empty)).unwrap();
        let filtered = filter_tree(
            kind,
            &source,
            &mut FilterMemo::default(),
            &prefix,
            &root.id.to_string(),
        )
        .unwrap();
        assert_eq!(
            prefixed,
            expected_segment(kind, &row, 1, &filtered.tree_id, &[])
        );

        let chain_child = build_tree(kind, vec![blob_item(kind, "file", b"child")]).unwrap();
        let chain_root = build_tree(
            kind,
            vec![
                blob_item(kind, "README", b"readme"),
                tree_item("a", &chain_child),
            ],
        )
        .unwrap();
        let chain_source = make_source(kind, &[chain_root.clone(), chain_child], &[]);
        let row = make_row(kind, &chain_root.id.to_string(), &[ROOT_PARENT]);
        let chain = filter(":/a:prefix=b");
        let chained = assert_equivalent(
            kind,
            &chain_source,
            &chain,
            &row,
            1,
            &empty,
            &no_prev(&empty),
        )
        .unwrap();
        let filtered = filter_tree(
            kind,
            &chain_source,
            &mut FilterMemo::default(),
            &chain,
            &chain_root.id.to_string(),
        )
        .unwrap();
        assert_eq!(
            chained,
            expected_segment(kind, &row, 1, &filtered.tree_id, &[])
        );
    }

    fn segment_snapshot(segment: Option<Segment>) -> String {
        match segment {
            None => "segment=none\n".to_owned(),
            Some(Segment {
                seq,
                view_commit: None,
                view_tree,
            }) => format!("seq={seq}\ncommit=null\ntree={view_tree}\n"),
            Some(Segment {
                seq,
                view_commit: Some(commit),
                view_tree,
            }) => format!(
                "seq={seq}\ncommit={}\ntree={view_tree}\nbytes={}",
                commit.id,
                String::from_utf8(commit.bytes).unwrap()
            ),
        }
    }

    fn history_snapshot(
        kind: HashKind,
        source: &InMemoryTreeSource,
        filter: &crate::ceres::view::filter::Filter,
        rows: &[mega_commit::Model],
    ) -> String {
        let empty = empty_tree_id(kind).unwrap().to_string();
        let mut prev = no_prev(&empty);
        let mut memo = FilterMemo::default();
        let mut segments = Vec::with_capacity(rows.len());
        for (index, row) in rows.iter().enumerate() {
            let seq = i64::try_from(index + 1).unwrap();
            let parent_tree = if index == 0 {
                empty.as_str()
            } else {
                rows[index - 1].tree.as_str()
            };
            let segment = project_commit(
                kind,
                source,
                &mut memo,
                filter,
                input(seq, Some(row)),
                parent_tree,
                &prev,
            )
            .unwrap();
            if let Some(projected) = &segment {
                prev = PreviousProjection {
                    view_commit: projected
                        .view_commit
                        .as_ref()
                        .map(|commit| commit.id.to_string()),
                    view_tree: projected.view_tree.clone(),
                };
            }
            segments.push(segment);
        }

        let mut rendered = String::from("git log --graph:\n");
        for segment in segments.iter().rev().flatten() {
            match &segment.view_commit {
                Some(commit) => {
                    rendered.push_str(&format!("* seq={} {}\n", segment.seq, commit.id))
                }
                None => rendered.push_str(&format!("* seq={} NULL\n", segment.seq)),
            }
        }
        rendered.push_str("tree list:\n");
        for (index, segment) in segments.into_iter().enumerate() {
            rendered.push_str(&format!("root-seq={}\n", index + 1));
            rendered.push_str(&segment_snapshot(segment));
        }
        rendered
    }

    #[test]
    fn josh_filter_id_canonical_rules() {
        let _ = include_str!("testdata/josh/filter/README.md");
        for (source_case, group) in [
            ("filter_id.t `:/a:/b`", [":/a:/b", ":/a/b"]),
            (
                "filter_id.t `:prefix=a/b:prefix=c`",
                [":prefix=a/b:prefix=c", ":prefix=c/a/b"],
            ),
            (
                "filter_id.t `:prefix=x/y:/x`",
                [":prefix=x/y:/x", ":prefix=y"],
            ),
            ("filter_id.t `:[:empty,:/a]`", [":[:empty,:/a]", ":/a"]),
            (
                "mega2 §2.3 rule 6 selector sorting",
                [":exclude[::b/,::a/]", ":exclude[::a/,::b/]"],
            ),
            (
                "mega2 §2.1 Compose ordering golden vector",
                [
                    ":[:/b:prefix=y,:/a:prefix=x]",
                    ":[:/a:prefix=x,:/b:prefix=y]",
                ],
            ),
        ] {
            let canonical = group
                .map(|text| crate::ceres::view::filter::print(&canonicalize(parse(text).unwrap())));
            assert_eq!(canonical[0], canonical[1], "{source_case}: {group:?}");
        }
    }

    #[test]
    fn josh_filter_linear_ports() {
        let _hash_kind_guard = set_hash_kind_for_test(HashKind::Sha256);
        let kind = HashKind::Sha1;
        let sub1 = build_tree(kind, vec![blob_item(kind, "file", b"sub1")]).unwrap();
        let sub2 = build_tree(kind, vec![blob_item(kind, "file", b"sub2")]).unwrap();
        let subtree = build_tree(kind, vec![blob_item(kind, "file", b"subtree")]).unwrap();
        let root = build_tree(
            kind,
            vec![
                blob_item(kind, "README", b"readme"),
                blob_item(kind, "secret", b"secret"),
                tree_item("sub1", &sub1),
                tree_item("sub2", &sub2),
                tree_item("subtree", &subtree),
            ],
        )
        .unwrap();
        let row = make_row(kind, &root.id.to_string(), &[ROOT_PARENT]);
        let source = make_source(kind, &[root.clone(), sub1, sub2, subtree], &[]);
        let prefix_snapshot = history_snapshot(
            kind,
            &source,
            &filter(":prefix=subtree"),
            std::slice::from_ref(&row),
        );
        let subtree_snapshot = history_snapshot(
            kind,
            &source,
            &filter(":/subtree"),
            std::slice::from_ref(&row),
        );
        let signed = make_row_with_content(
            kind,
            &root.id.to_string(),
            &[ROOT_PARENT],
            "gpgsig fixture-signature\n continuation\n\nmessage\n",
        );
        let gpgsig_snapshot = history_snapshot(kind, &source, &filter(":nop"), &[signed]);
        let signed_sha256 = make_row_with_content(
            kind,
            &root.id.to_string(),
            &[ROOT_PARENT],
            "gpgsig-sha256 fixture-signature\n continuation\n\nmessage\n",
        );
        let gpgsig_sha256_snapshot =
            history_snapshot(kind, &source, &filter(":nop"), &[signed_sha256]);

        let sub1_v1 = build_tree(kind, vec![blob_item(kind, "file1", b"one")]).unwrap();
        let sub1_v2 = build_tree(
            kind,
            vec![
                blob_item(kind, "file1", b"one"),
                blob_item(kind, "file2", b"two"),
            ],
        )
        .unwrap();
        let sub1_v3 = build_tree(
            kind,
            vec![
                blob_item(kind, "file1", b"one"),
                blob_item(kind, "file2", b"two"),
                blob_item(kind, "file5", b"five"),
            ],
        )
        .unwrap();
        let sub2 = build_tree(kind, vec![blob_item(kind, "file3", b"three")]).unwrap();
        let sub3 = build_tree(kind, vec![blob_item(kind, "file1", b"three")]).unwrap();
        let root1 = build_tree(kind, vec![tree_item("sub1", &sub1_v1)]).unwrap();
        let root2 = build_tree(kind, vec![tree_item("sub1", &sub1_v2)]).unwrap();
        let deleted = build_tree(kind, Vec::new()).unwrap();
        let moved = build_tree(kind, vec![tree_item("sub1_new", &sub1_v2)]).unwrap();
        let moved_unrelated = build_tree(
            kind,
            vec![
                blob_item(kind, "unrelated_file", b"unrelated"),
                tree_item("sub1_new", &sub1_v2),
            ],
        )
        .unwrap();
        let root3 = build_tree(
            kind,
            vec![tree_item("sub1", &sub1_v2), tree_item("sub2", &sub2)],
        )
        .unwrap();
        let empty_head_final = build_tree(
            kind,
            vec![tree_item("sub1", &sub1_v3), tree_item("sub2", &sub2)],
        )
        .unwrap();
        let exclude_root1 = build_tree(kind, vec![tree_item("sub1", &sub1_v1)]).unwrap();
        let exclude_root2 = build_tree(
            kind,
            vec![tree_item("sub1", &sub1_v1), tree_item("sub2", &sub2)],
        )
        .unwrap();
        let exclude_root3 = build_tree(
            kind,
            vec![
                tree_item("sub1", &sub1_v1),
                tree_item("sub2", &sub2),
                tree_item("sub3", &sub3),
            ],
        )
        .unwrap();
        let history_source = make_source(
            kind,
            &[
                root1.clone(),
                root2.clone(),
                deleted.clone(),
                moved.clone(),
                moved_unrelated.clone(),
                root3.clone(),
                empty_head_final.clone(),
                exclude_root1.clone(),
                exclude_root2.clone(),
                exclude_root3.clone(),
                sub1_v1,
                sub1_v2,
                sub1_v3,
                sub2,
                sub3,
            ],
            &[],
        );
        let rows = |roots: &[&Tree]| {
            roots
                .iter()
                .map(|root| make_row(kind, &root.id.to_string(), &[ROOT_PARENT]))
                .collect::<Vec<_>>()
        };
        let deleted_snapshot = history_snapshot(
            kind,
            &history_source,
            &filter(":/sub1:prefix=c"),
            &rows(&[&root1, &root2, &deleted]),
        );
        let moved_snapshot = history_snapshot(
            kind,
            &history_source,
            &filter(":/sub1:prefix=c"),
            &rows(&[&root1, &root2, &moved, &moved_unrelated]),
        );
        let empty_head_snapshot = history_snapshot(
            kind,
            &history_source,
            &filter(":/sub2"),
            &rows(&[&root1, &root2, &root3, &empty_head_final]),
        );
        let exclude_sub2_snapshot = history_snapshot(
            kind,
            &history_source,
            &filter(":exclude[::sub2/]"),
            &rows(&[&exclude_root1, &exclude_root2, &exclude_root3]),
        );
        let exclude_sub1_sub2_snapshot = history_snapshot(
            kind,
            &history_source,
            &filter(":exclude[::sub1/,::sub2/]"),
            &rows(&[&exclude_root1, &exclude_root2, &exclude_root3]),
        );
        let exclude_snapshot = format!(
            "EXCLUDE-SUB2:\n{exclude_sub2_snapshot}EXCLUDE-SUB1-SUB2:\n{exclude_sub1_sub2_snapshot}"
        );
        assert_eq!(
            prefix_snapshot,
            include_str!("testdata/josh/filter/prefix.expected")
        );
        assert_eq!(
            subtree_snapshot,
            include_str!("testdata/josh/filter/subtree_prefix.expected")
        );
        assert_eq!(
            gpgsig_snapshot,
            include_str!("testdata/josh/filter/gpgsig.expected")
        );
        assert_eq!(
            gpgsig_sha256_snapshot,
            include_str!("testdata/josh/filter/gpgsig-sha256.expected")
        );
        assert!(!gpgsig_snapshot.contains("gpgsig"));
        assert!(!gpgsig_sha256_snapshot.contains("gpgsig"));
        assert_eq!(
            deleted_snapshot,
            include_str!("testdata/josh/filter/deleted_dir.expected")
        );
        assert_eq!(
            moved_snapshot,
            include_str!("testdata/josh/filter/moved_dir.expected")
        );
        assert_eq!(
            empty_head_snapshot,
            include_str!("testdata/josh/filter/empty_head.expected")
        );
        assert_eq!(
            exclude_snapshot,
            include_str!("testdata/josh/filter/exclude_compose.expected")
        );
    }

    #[test]
    fn project_errors() {
        let _hash_kind_guard = set_hash_kind_for_test(HashKind::Sha256);
        let kind = HashKind::Sha1;
        let empty = empty_tree_id(kind).unwrap().to_string();
        let root = build_tree(kind, vec![blob_item(kind, "README", b"readme")]).unwrap();
        let source = make_source(kind, std::slice::from_ref(&root), &[]);
        let valid = make_row(kind, &root.id.to_string(), &[ROOT_PARENT]);
        let nop = filter(":nop");

        let mut premise = valid.clone();
        premise.author = Some("author  <alice@example.com> 1 +0000".to_owned());
        assert!(matches!(
            project_commit(
                kind,
                &source,
                &mut FilterMemo::default(),
                &nop,
                input(1, Some(&premise)),
                &empty,
                &no_prev(&empty),
            ),
            Err(super::ProjectFailure::Data(super::ProjectError::Premise(
                super::RewriteError::PremiseMismatch { .. }
            )))
        ));

        let mut corrupt = valid.clone();
        corrupt.parents_id = serde_json::json!({"parent": ROOT_PARENT});
        assert!(matches!(
            project_commit(
                kind,
                &source,
                &mut FilterMemo::default(),
                &nop,
                input(1, Some(&corrupt)),
                &empty,
                &no_prev(&empty),
            ),
            Err(super::ProjectFailure::Data(super::ProjectError::Premise(
                super::RewriteError::CorruptRow { .. }
            )))
        ));

        assert_eq!(
            project_commit(
                kind,
                &source,
                &mut FilterMemo::default(),
                &nop,
                ProjectionInput {
                    seq: 1,
                    commit_id: "missing-commit",
                    tree_id: &root.id.to_string(),
                    row: None,
                },
                &empty,
                &no_prev(&empty),
            ),
            Err(super::ProjectFailure::Data(
                super::ProjectError::MissingCommit {
                    commit_id: "missing-commit".to_owned(),
                }
            ))
        );

        assert_eq!(
            project_commit(
                kind,
                &source,
                &mut FilterMemo::default(),
                &nop,
                ProjectionInput {
                    seq: 2,
                    commit_id: "missing-commit",
                    tree_id: &root.id.to_string(),
                    row: None,
                },
                "changed-parent",
                &prev(&root.id.to_string()),
            )
            .unwrap(),
            None
        );

        let missing_root = make_source(kind, &[], &[root.id.to_string()]);
        assert_eq!(
            project_commit(
                kind,
                &missing_root,
                &mut FilterMemo::default(),
                &filter(":/a"),
                input(1, Some(&valid)),
                &empty,
                &no_prev(&empty),
            ),
            Err(super::ProjectFailure::Data(
                super::ProjectError::MissingObject(
                    crate::ceres::view::tree_source::MissingObject {
                        tree_id: root.id.to_string(),
                        reason: crate::ceres::view::tree_source::MissingObjectReason::Absent,
                    }
                )
            ))
        );
        let mut malformed_bytes = root.to_data().unwrap();
        malformed_bytes.pop();
        let malformed = InMemoryTreeSource::new(
            kind,
            [(root.id.to_string(), malformed_bytes)]
                .into_iter()
                .collect(),
            HashSet::new(),
        );
        assert_eq!(
            project_commit(
                kind,
                &malformed,
                &mut FilterMemo::default(),
                &filter(":/a"),
                input(1, Some(&valid)),
                &empty,
                &no_prev(&empty),
            ),
            Err(super::ProjectFailure::Data(
                super::ProjectError::MissingObject(
                    crate::ceres::view::tree_source::MissingObject {
                        tree_id: root.id.to_string(),
                        reason: crate::ceres::view::tree_source::MissingObjectReason::Malformed,
                    }
                )
            ))
        );

        let nested_child = build_tree(kind, vec![blob_item(kind, "b", b"nested")]).unwrap();
        let nested_root = build_tree(kind, vec![tree_item("a", &nested_child)]).unwrap();
        let nested_row = make_row(kind, &nested_root.id.to_string(), &[ROOT_PARENT]);
        for (source, reason) in [
            (
                make_source(
                    kind,
                    std::slice::from_ref(&nested_root),
                    &[nested_child.id.to_string()],
                ),
                crate::ceres::view::tree_source::MissingObjectReason::Absent,
            ),
            (
                make_source(kind, std::slice::from_ref(&nested_root), &[]),
                crate::ceres::view::tree_source::MissingObjectReason::Unprefetched,
            ),
        ] {
            assert_eq!(
                project_commit(
                    kind,
                    &source,
                    &mut FilterMemo::default(),
                    &filter(":/a/b"),
                    input(1, Some(&nested_row)),
                    &empty,
                    &no_prev(&empty),
                ),
                Err(super::ProjectFailure::Data(
                    super::ProjectError::MissingObject(
                        crate::ceres::view::tree_source::MissingObject {
                            tree_id: nested_child.id.to_string(),
                            reason,
                        }
                    )
                ))
            );
        }
        let unprefetched = make_source(kind, &[], &[]);
        assert_eq!(
            project_commit(
                kind,
                &unprefetched,
                &mut FilterMemo::default(),
                &filter(":/a"),
                input(1, Some(&valid)),
                &empty,
                &no_prev(&empty),
            ),
            Err(super::ProjectFailure::Data(
                super::ProjectError::MissingObject(
                    crate::ceres::view::tree_source::MissingObject {
                        tree_id: root.id.to_string(),
                        reason: crate::ceres::view::tree_source::MissingObjectReason::Unprefetched,
                    }
                )
            ))
        );

        let child_id = blob(kind, b"unavailable-child");
        let only_tree_root = build_tree(
            kind,
            vec![TreeItem {
                mode: TreeItemMode::Tree,
                id: child_id,
                name: "child".to_owned(),
            }],
        )
        .unwrap();
        let prefix_row = make_row(kind, &only_tree_root.id.to_string(), &[ROOT_PARENT]);
        let prefix_source = make_source(
            kind,
            std::slice::from_ref(&only_tree_root),
            &[child_id.to_string()],
        );
        assert_eq!(
            project_commit(
                kind,
                &prefix_source,
                &mut FilterMemo::default(),
                &filter(":prefix=p"),
                input(1, Some(&prefix_row)),
                &empty,
                &no_prev(&empty),
            ),
            Err(super::ProjectFailure::Data(
                super::ProjectError::MissingObject(
                    crate::ceres::view::tree_source::MissingObject {
                        tree_id: child_id.to_string(),
                        reason: crate::ceres::view::tree_source::MissingObjectReason::Absent,
                    }
                )
            ))
        );

        let prefix_unprefetched = make_source(kind, std::slice::from_ref(&only_tree_root), &[]);
        assert_eq!(
            project_commit(
                kind,
                &prefix_unprefetched,
                &mut FilterMemo::default(),
                &filter(":prefix=p"),
                input(1, Some(&prefix_row)),
                &empty,
                &no_prev(&empty),
            ),
            Err(super::ProjectFailure::Data(
                super::ProjectError::MissingObject(
                    crate::ceres::view::tree_source::MissingObject {
                        tree_id: child_id.to_string(),
                        reason: crate::ceres::view::tree_source::MissingObjectReason::Unprefetched,
                    }
                )
            ))
        );

        let left = build_tree(kind, vec![blob_item(kind, "f", b"left")]).unwrap();
        let right = build_tree(kind, vec![blob_item(kind, "f", b"right")]).unwrap();
        let conflict_root =
            build_tree(kind, vec![tree_item("a", &left), tree_item("b", &right)]).unwrap();
        let conflict_row = make_row(kind, &conflict_root.id.to_string(), &[ROOT_PARENT]);
        let conflict_source = make_source(kind, &[conflict_root.clone(), left, right], &[]);
        assert_eq!(
            project_commit(
                kind,
                &conflict_source,
                &mut FilterMemo::default(),
                &parse(":[:/a:prefix=x,:/b:prefix=x]").unwrap(),
                input(1, Some(&conflict_row)),
                &empty,
                &no_prev(&empty),
            ),
            Err(super::ProjectFailure::Internal)
        );
    }

    #[test]
    fn chain_intermediate_empty_root() {
        let _hash_kind_guard = set_hash_kind_for_test(HashKind::Sha256);
        let kind = HashKind::Sha1;
        let empty = empty_tree_id(kind).unwrap().to_string();
        let child = build_tree(
            kind,
            vec![tree_item("e", &build_tree(kind, Vec::new()).unwrap())],
        )
        .unwrap();
        let root = build_tree(
            kind,
            vec![blob_item(kind, "README", b"readme"), tree_item("a", &child)],
        )
        .unwrap();
        let row = make_row(kind, &root.id.to_string(), &[ROOT_PARENT]);
        let chain = filter(":/a:exclude[::e/]");
        let source = make_source(kind, &[root.clone(), child.clone()], &[]);
        let logs = capture_tracing(|| {
            let result = project_commit(
                kind,
                &source,
                &mut FilterMemo::default(),
                &chain,
                input(1, Some(&row)),
                &empty,
                &no_prev(&empty),
            )
            .unwrap();
            assert_eq!(
                result,
                Some(Segment {
                    seq: 1,
                    view_commit: None,
                    view_tree: empty.clone(),
                })
            );
        });
        let filter_id =
            crate::ceres::view::filter::filter_id(&crate::ceres::view::filter::print(&chain));
        let warn_lines = logs
            .lines()
            .filter(|line| line.contains("view_r8_intermediate_empty_root_total"))
            .collect::<Vec<_>>();
        assert_eq!(warn_lines.len(), 1, "{logs}");
        assert!(warn_lines[0].contains(" WARN "), "{logs}");
        assert!(warn_lines[0].contains(&format!("filter_id={filter_id}")));
        assert!(warn_lines[0].contains("seq=1"));
        assert!(warn_lines[0].contains("level=1"));
        assert!(warn_lines[0].contains(&format!("tree_id={}", child.id)));

        let logs = capture_tracing(|| {
            assert_eq!(
                project_commit(
                    kind,
                    &source,
                    &mut FilterMemo::default(),
                    &chain,
                    input(2, Some(&row)),
                    &empty,
                    &no_prev(&empty),
                )
                .unwrap(),
                None
            );
        });
        assert_r8_warn(&logs, &chain, 2, 1, &child.id.to_string());

        let logs = capture_tracing(|| {
            assert_eq!(
                project_commit(
                    kind,
                    &source,
                    &mut FilterMemo::default(),
                    &chain,
                    input(2, Some(&row)),
                    "different-parent-tree",
                    &prev(&empty),
                )
                .unwrap(),
                None
            );
        });
        assert_no_r8_event(&logs);

        let missing_chain = filter(":/missing:exclude[::e/]");
        let logs = capture_tracing(|| {
            assert_eq!(
                project_commit(
                    kind,
                    &source,
                    &mut FilterMemo::default(),
                    &missing_chain,
                    input(1, Some(&row)),
                    &empty,
                    &no_prev(&empty),
                )
                .unwrap(),
                Some(Segment {
                    seq: 1,
                    view_commit: None,
                    view_tree: empty.clone(),
                })
            );
        });
        assert_no_r8_event(&logs);

        let prefix_chain = filter(":/a:prefix=x");
        let prefix_tree = filter_tree(
            kind,
            &source,
            &mut FilterMemo::default(),
            &prefix_chain,
            &root.id.to_string(),
        )
        .unwrap()
        .tree_id;
        let logs = capture_tracing(|| {
            assert_eq!(
                project_commit(
                    kind,
                    &source,
                    &mut FilterMemo::default(),
                    &prefix_chain,
                    input(1, Some(&row)),
                    &empty,
                    &no_prev(&empty),
                )
                .unwrap(),
                Some(expected_segment(kind, &row, 1, &prefix_tree, &[]))
            );
        });
        assert_no_r8_event(&logs);

        let nonempty_child = build_tree(kind, vec![blob_item(kind, "f", b"f")]).unwrap();
        let nonempty_root = build_tree(
            kind,
            vec![
                blob_item(kind, "README", b"readme"),
                tree_item("a", &nonempty_child),
            ],
        )
        .unwrap();
        let nonempty_row = make_row(kind, &nonempty_root.id.to_string(), &[ROOT_PARENT]);
        let nonempty_source = make_source(kind, &[nonempty_root, nonempty_child], &[]);
        let nonempty_chain = filter(":/a:exclude[::f]");
        let logs = capture_tracing(|| {
            assert_eq!(
                project_commit(
                    kind,
                    &nonempty_source,
                    &mut FilterMemo::default(),
                    &nonempty_chain,
                    input(1, Some(&nonempty_row)),
                    &empty,
                    &no_prev(&empty),
                )
                .unwrap(),
                Some(Segment {
                    seq: 1,
                    view_commit: None,
                    view_tree: empty.clone(),
                })
            );
        });
        assert_no_r8_event(&logs);

        let root_only_tree = build_tree(kind, vec![tree_item("a", &child)]).unwrap();
        let root_only_row = make_row(kind, &root_only_tree.id.to_string(), &[ROOT_PARENT]);
        let root_only_source = make_source(kind, &[root_only_tree, child.clone()], &[]);
        let logs = capture_tracing(|| {
            assert_eq!(
                project_commit(
                    kind,
                    &root_only_source,
                    &mut FilterMemo::default(),
                    &chain,
                    input(1, Some(&root_only_row)),
                    &empty,
                    &no_prev(&empty),
                )
                .unwrap(),
                Some(expected_segment(kind, &root_only_row, 1, &empty, &[]))
            );
        });
        assert_no_r8_event(&logs);

        let first_rewrite = filter(":exclude[::README]");
        let expected_first_rewrite = filter_tree(
            kind,
            &source,
            &mut FilterMemo::default(),
            &first_rewrite,
            &root.id.to_string(),
        )
        .unwrap()
        .tree_id;
        for full_chain in [
            filter(":exclude[::README]:exclude[::a/e/]"),
            filter(":exclude[::README]:/a:exclude[::e/]"),
        ] {
            let logs = capture_tracing(|| {
                assert_eq!(
                    project_commit(
                        kind,
                        &source,
                        &mut FilterMemo::default(),
                        &full_chain,
                        input(1, Some(&row)),
                        &empty,
                        &no_prev(&empty),
                    )
                    .unwrap(),
                    Some(Segment {
                        seq: 1,
                        view_commit: None,
                        view_tree: empty.clone(),
                    })
                );
            });
            assert_r8_warn(&logs, &full_chain, 1, 1, &expected_first_rewrite);
        }

        let unknown = blob(kind, b"unknown");
        let incomplete_child = build_tree(
            kind,
            vec![TreeItem {
                mode: TreeItemMode::Tree,
                id: unknown,
                name: "e".to_owned(),
            }],
        )
        .unwrap();
        let incomplete_root = build_tree(
            kind,
            vec![
                blob_item(kind, "README", b"readme"),
                tree_item("a", &incomplete_child),
            ],
        )
        .unwrap();
        let incomplete_row = make_row(kind, &incomplete_root.id.to_string(), &[ROOT_PARENT]);
        let source = make_source(
            kind,
            &[incomplete_root.clone(), incomplete_child.clone()],
            &[unknown.to_string()],
        );
        let logs = capture_tracing(|| {
            assert_eq!(
                project_commit(
                    kind,
                    &source,
                    &mut FilterMemo::default(),
                    &chain,
                    input(1, Some(&incomplete_row)),
                    &empty,
                    &no_prev(&empty),
                )
                .unwrap(),
                Some(Segment {
                    seq: 1,
                    view_commit: None,
                    view_tree: empty.clone(),
                })
            );
        });
        assert_r8_debug(&logs, &chain, 1, 1);

        let source = make_source(kind, &[incomplete_root, incomplete_child], &[]);
        let logs = capture_tracing(|| {
            assert_eq!(
                project_commit(
                    kind,
                    &source,
                    &mut FilterMemo::default(),
                    &chain,
                    input(1, Some(&incomplete_row)),
                    &empty,
                    &no_prev(&empty),
                )
                .unwrap(),
                Some(Segment {
                    seq: 1,
                    view_commit: None,
                    view_tree: empty.clone(),
                })
            );
        });
        assert_r8_debug(&logs, &chain, 1, 1);

        let dense_child = build_tree(kind, vec![blob_item(kind, "x", b"x")]).unwrap();
        let dense_root = build_tree(
            kind,
            vec![
                blob_item(kind, "README", b"readme"),
                tree_item("d1", &dense_child),
                tree_item("d2", &dense_child),
                tree_item("d3", &dense_child),
                tree_item("d4", &dense_child),
                tree_item("d5", &dense_child),
            ],
        )
        .unwrap();
        let dense_row = make_row(kind, &dense_root.id.to_string(), &[ROOT_PARENT]);
        let dense_source = make_source(kind, &[dense_root.clone(), dense_child], &[]);
        let first_op = filter(":exclude[::README]");
        let memo_chain = filter(":exclude[::README]:/a");
        let first_text = crate::ceres::view::filter::print(&first_op);
        let chain_text = crate::ceres::view::filter::print(&memo_chain);
        let mut memo = (0..=4096)
            .find_map(|capacity| {
                let mut candidate = FilterMemo::with_capacity(capacity);
                filter_tree(
                    kind,
                    &dense_source,
                    &mut candidate,
                    &memo_chain,
                    &dense_root.id.to_string(),
                )
                .ok()?;
                (candidate.contains(&chain_text, &dense_root.id.to_string())
                    && !candidate.contains(&first_text, &dense_root.id.to_string()))
                .then_some(candidate)
            })
            .expect("a capacity must retain only the composite filter result");
        let logs = capture_tracing(|| {
            assert_eq!(
                project_commit(
                    kind,
                    &make_source(kind, &[], &[]),
                    &mut memo,
                    &memo_chain,
                    input(2, Some(&dense_row)),
                    &empty,
                    &no_prev(&empty),
                )
                .unwrap(),
                None
            );
        });
        assert_r8_debug(&logs, &memo_chain, 2, 1);
    }

    fn assert_r8_warn(
        logs: &str,
        filter: &crate::ceres::view::filter::Filter,
        seq: i64,
        level: usize,
        tree_id: &str,
    ) {
        let lines = logs
            .lines()
            .filter(|line| line.contains("view_r8_intermediate_empty_root_total"))
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), 1, "{logs}");
        let filter_id =
            crate::ceres::view::filter::filter_id(&crate::ceres::view::filter::print(filter));
        assert!(lines[0].contains(" WARN "), "{logs}");
        assert!(
            lines[0].contains(&format!("filter_id={filter_id}")),
            "{logs}"
        );
        assert!(lines[0].contains(&format!("seq={seq}")), "{logs}");
        assert!(lines[0].contains(&format!("level={level}")), "{logs}");
        assert!(lines[0].contains(&format!("tree_id={tree_id}")), "{logs}");
    }

    fn assert_r8_debug(
        logs: &str,
        filter: &crate::ceres::view::filter::Filter,
        seq: i64,
        level: usize,
    ) {
        assert!(
            !logs.contains("view_r8_intermediate_empty_root_total"),
            "{logs}"
        );
        let lines = logs
            .lines()
            .filter(|line| line.contains("view_r8_detection_skipped"))
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), 1, "{logs}");
        let filter_id =
            crate::ceres::view::filter::filter_id(&crate::ceres::view::filter::print(filter));
        assert!(lines[0].contains(" DEBUG "), "{logs}");
        assert!(
            lines[0].contains(&format!("filter_id={filter_id}")),
            "{logs}"
        );
        assert!(lines[0].contains(&format!("seq={seq}")), "{logs}");
        assert!(lines[0].contains(&format!("level={level}")), "{logs}");
    }

    fn assert_no_r8_event(logs: &str) {
        assert!(
            !logs.contains("view_r8_intermediate_empty_root_total"),
            "{logs}"
        );
        assert!(!logs.contains("view_r8_detection_skipped"), "{logs}");
    }

    fn capture_tracing(f: impl FnOnce()) -> String {
        use std::{
            io::{self, Write},
            sync::{Arc, Mutex},
        };

        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone)]
        struct TestWriter(Arc<Mutex<Vec<u8>>>);

        impl Write for TestWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        impl MakeWriter<'_> for TestWriter {
            type Writer = TestWriter;

            fn make_writer(&self) -> Self::Writer {
                self.clone()
            }
        }

        let _pin_registry = tracing::Dispatch::new(
            tracing_subscriber::fmt()
                .with_writer(io::sink)
                .with_max_level(tracing::Level::DEBUG)
                .finish(),
        );
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(TestWriter(bytes.clone()))
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        String::from_utf8(bytes.lock().unwrap().clone()).unwrap()
    }
}
