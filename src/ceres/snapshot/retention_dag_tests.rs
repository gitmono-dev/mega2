use mst2_codec::metapage::{BranchChild, EntryKind};

use super::*;

fn payload(entries: &[Entry]) -> MetadataPagePayload {
    let bytes = Page::build(entries).unwrap();
    MetadataPagePayload {
        id: page_id(&bytes),
        size: bytes.len() as u64,
        bytes,
    }
}

fn candidate(dag: &ValidatedMetadataDag) -> MetadataDagCandidate {
    let ids: BTreeMap<_, _> = dag
        .payloads()
        .iter()
        .map(|payload| (node_id(&payload.id), payload.id))
        .collect();
    MetadataDagCandidate {
        metadata_codec: METADATA_CODEC,
        root: dag.root(),
        pages: dag.payloads().to_vec(),
        edges: dag
            .edges()
            .iter()
            .map(|edge| (ids[&edge.parent], ids[&edge.child]))
            .collect(),
    }
}

fn nested_shared() -> ValidatedMetadataDag {
    let empty = payload(&[]);
    let mut wide: Vec<_> = (0..129)
        .map(|index| {
            Entry::file(
                EntryKind::Regular,
                format!("f{index:03}").as_bytes(),
                index,
                [index as u8; 32],
            )
        })
        .collect();
    wide.push(Entry::dir(b"nested", empty.id));
    let shared = payload(&wide);
    let root_entries = vec![
        Entry::dir(b"alpha", shared.id),
        Entry::dir(b"beta", shared.id),
    ];
    let root = payload(&root_entries);
    let mut builder = MetadataDagBuilder::new(MetadataDagLimits::default());
    builder.add_directory(&root.bytes, &root_entries).unwrap();
    builder.add_directory(&shared.bytes, &wide).unwrap();
    builder.add_directory(&empty.bytes, &[]).unwrap();
    builder.add_directory(&shared.bytes, &wide).unwrap();
    builder.finish(root.id).unwrap()
}

#[test]
fn complete_radix_nested_shared_closure_accounts_unique_payloads_and_edges() {
    let dag = nested_shared();
    assert!(
        dag.payloads()
            .iter()
            .any(|payload| matches!(Page::decode(&payload.bytes).unwrap().0, Page::Branch { .. }))
    );
    assert!(
        dag.payloads().len() > 3,
        "internal radix pages must be retained too"
    );
    assert_eq!(
        dag.payload_bytes(),
        dag.payloads()
            .iter()
            .map(|payload| payload.size)
            .sum::<u64>()
    );
    assert_eq!(dag.nodes().len(), dag.payloads().len());
    let mut ids = BTreeSet::new();
    for (node, payload) in dag.nodes().iter().zip(dag.payloads()) {
        assert_eq!(node.kind, RetainedKind::Page);
        assert_eq!(node.state, NodeState::Live);
        assert_eq!(node.bytes, payload.bytes.len() as u64);
        assert_eq!(node.id, node_id(&payload.id));
        assert!(ids.insert(node.id.clone()));
    }
    let edges: BTreeSet<_> = dag
        .edges()
        .iter()
        .map(|edge| (&edge.parent, &edge.child))
        .collect();
    assert_eq!(edges.len(), dag.edges().len());
    assert!(
        dag.edges()
            .iter()
            .all(|edge| ids.contains(&edge.parent) && ids.contains(&edge.child))
    );
    let root_id = node_id(&dag.root());
    assert_eq!(
        dag.edges()
            .iter()
            .filter(|edge| edge.parent == root_id)
            .count(),
        1,
        "two names sharing one child acquire one edge"
    );
}

#[test]
fn digest_and_advertised_size_are_verified_before_exposing_group() {
    let dag = nested_shared();
    let mut wrong_id = candidate(&dag);
    wrong_id.pages[0].id[0] ^= 1;
    assert_eq!(
        ValidatedMetadataDag::validate(wrong_id, MetadataDagLimits::default())
            .unwrap_err()
            .code,
        SnapshotErrorCode::DigestMismatch
    );
    let mut wrong_size = candidate(&dag);
    wrong_size.pages[0].size += 1;
    assert_eq!(
        ValidatedMetadataDag::validate(wrong_size, MetadataDagLimits::default())
            .unwrap_err()
            .code,
        SnapshotErrorCode::DigestMismatch
    );
    let mut corrupt = candidate(&dag);
    corrupt.pages[0].bytes[0] ^= 1;
    assert_eq!(
        ValidatedMetadataDag::validate(corrupt, MetadataDagLimits::default())
            .unwrap_err()
            .code,
        SnapshotErrorCode::DigestMismatch
    );
}

#[test]
fn missing_child_and_missing_root_are_unavailable() {
    let dag = nested_shared();
    let mut missing = candidate(&dag);
    let index = missing
        .pages
        .iter()
        .position(|payload| payload.id != missing.root)
        .unwrap();
    missing.pages.remove(index);
    assert_eq!(
        ValidatedMetadataDag::validate(missing, MetadataDagLimits::default())
            .unwrap_err()
            .code,
        SnapshotErrorCode::ObjectUnavailable
    );
    let mut missing_root = candidate(&dag);
    missing_root.root = [255; 32];
    assert_eq!(
        ValidatedMetadataDag::validate(missing_root, MetadataDagLimits::default())
            .unwrap_err()
            .code,
        SnapshotErrorCode::ObjectUnavailable
    );
}

#[test]
fn duplicate_payloads_and_edges_are_rejected() {
    let dag = nested_shared();
    let mut duplicate = candidate(&dag);
    duplicate.pages.push(duplicate.pages[0].clone());
    assert_eq!(
        ValidatedMetadataDag::validate(duplicate, MetadataDagLimits::default())
            .unwrap_err()
            .code,
        SnapshotErrorCode::IntegrityError
    );
    let mut duplicate_edge = candidate(&dag);
    duplicate_edge.edges.push(duplicate_edge.edges[0]);
    assert_eq!(
        ValidatedMetadataDag::validate(duplicate_edge, MetadataDagLimits::default())
            .unwrap_err()
            .code,
        SnapshotErrorCode::IntegrityError
    );
}

#[test]
fn cycles_wrong_edges_and_unreachable_objects_are_rejected() {
    let dag = nested_shared();
    let mut cyclic = candidate(&dag);
    cyclic.edges.push((cyclic.root, cyclic.root));
    let error = ValidatedMetadataDag::validate(cyclic, MetadataDagLimits::default()).unwrap_err();
    assert_eq!(error.code, SnapshotErrorCode::IntegrityError);
    assert!(error.message.contains("cycle"));
    let mut missing_edge = candidate(&dag);
    missing_edge.edges.pop();
    assert_eq!(
        ValidatedMetadataDag::validate(missing_edge, MetadataDagLimits::default())
            .unwrap_err()
            .code,
        SnapshotErrorCode::IntegrityError
    );
    let mut orphan = candidate(&dag);
    orphan.pages.push(payload(&[Entry::file(
        EntryKind::Regular,
        b"orphan",
        1,
        [91; 32],
    )]));
    assert_eq!(
        ValidatedMetadataDag::validate(orphan, MetadataDagLimits::default())
            .unwrap_err()
            .code,
        SnapshotErrorCode::IntegrityError
    );
}

#[test]
fn correct_hashes_do_not_make_a_noncanonical_directory_valid() {
    let left_entry = Entry::file(EntryKind::Regular, b"a", 1, [1; 32]);
    let right_entry = Entry::file(EntryKind::Regular, b"b", 1, [2; 32]);
    let left = payload(&[left_entry]);
    let right = payload(&[right_entry]);
    let bytes = Page::Branch {
        prefix: Vec::new(),
        terminal: None,
        children: vec![
            BranchChild {
                label: b'a',
                subtree_entries: 1,
                child_page_id: left.id,
            },
            BranchChild {
                label: b'b',
                subtree_entries: 1,
                child_page_id: right.id,
            },
        ],
    }
    .encode()
    .unwrap();
    let root = page_id(&bytes);
    let group = MetadataDagCandidate {
        metadata_codec: METADATA_CODEC,
        root,
        edges: vec![(root, left.id), (root, right.id)],
        pages: vec![
            MetadataPagePayload {
                id: root,
                size: bytes.len() as u64,
                bytes,
            },
            left,
            right,
        ],
    };
    let error = ValidatedMetadataDag::validate(group, MetadataDagLimits::default()).unwrap_err();
    assert_eq!(error.code, SnapshotErrorCode::IntegrityError);
    assert!(error.message.contains("canonical"));
}

#[test]
fn node_edge_payload_entry_and_work_budgets_reject_complete_groups() {
    let dag = nested_shared();
    for limits in [
        MetadataDagLimits {
            nodes: dag.nodes().len() - 1,
            ..MetadataDagLimits::default()
        },
        MetadataDagLimits {
            edges: dag.edges().len() - 1,
            ..MetadataDagLimits::default()
        },
        MetadataDagLimits {
            payload_bytes: dag.payload_bytes() - 1,
            ..MetadataDagLimits::default()
        },
        MetadataDagLimits {
            entries: 1,
            ..MetadataDagLimits::default()
        },
        MetadataDagLimits {
            prepare_entry_visits: 0,
            ..MetadataDagLimits::default()
        },
    ] {
        assert_eq!(
            ValidatedMetadataDag::validate(candidate(&dag), limits)
                .unwrap_err()
                .code,
            SnapshotErrorCode::LimitExceeded
        );
    }
}

#[test]
fn failed_preparation_cannot_expose_a_partial_group() {
    let empty = payload(&[]);
    let root_entries = vec![Entry::dir(b"child", empty.id)];
    let root = payload(&root_entries);
    let mut builder = MetadataDagBuilder::new(MetadataDagLimits {
        nodes: 1,
        ..MetadataDagLimits::default()
    });
    assert_eq!(
        builder
            .add_directory(&root.bytes, &root_entries)
            .unwrap_err()
            .code,
        SnapshotErrorCode::LimitExceeded
    );
    assert_eq!(
        builder.finish(root.id).unwrap_err().code,
        SnapshotErrorCode::IntegrityError
    );
    let mut builder = MetadataDagBuilder::new(MetadataDagLimits::default());
    builder.add_directory(&root.bytes, &root_entries).unwrap();
    assert_eq!(
        builder.finish(root.id).unwrap_err().code,
        SnapshotErrorCode::ObjectUnavailable
    );
}

#[test]
fn codec_identity_and_canonical_entry_source_are_fixed() {
    let dag = nested_shared();
    let mut wrong_codec = candidate(&dag);
    wrong_codec.metadata_codec += 1;
    assert_eq!(
        ValidatedMetadataDag::validate(wrong_codec, MetadataDagLimits::default())
            .unwrap_err()
            .code,
        SnapshotErrorCode::ScopeInvalid
    );
    let empty = payload(&[]);
    let mut builder = MetadataDagBuilder::new(MetadataDagLimits::default());
    assert_eq!(
        builder
            .add_directory(
                &empty.bytes,
                &[Entry::file(EntryKind::Regular, b"different", 1, [1; 32])]
            )
            .unwrap_err()
            .code,
        SnapshotErrorCode::IntegrityError
    );
}
