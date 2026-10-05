use git_internal::{
    hash::{HashKind, ObjectHash},
    internal::object::types::ObjectType,
};

use crate::{
    callisto::mega_commit,
    ceres::merge_checker::gpg_signature_checker::rebuild_canonical_commit_bytes,
    common::utils::is_signature_header,
};

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum RewriteError {
    #[error("commit premise does not match persisted bytes: {commit_id}")]
    PremiseMismatch { commit_id: String },
    #[error("persisted commit row is corrupt: {commit_id}")]
    CorruptRow { commit_id: String },
}

pub(crate) fn rewrite_commit(
    kind: HashKind,
    row: &mega_commit::Model,
    tree: &str,
    parents: &[String],
) -> Result<(ObjectHash, Vec<u8>), RewriteError> {
    let original = rebuild_canonical_commit_bytes(row).map_err(|_| corrupt_row(row))?;
    let original_id =
        ObjectHash::from_type_and_data_for_kind(kind, ObjectType::Commit, original.as_bytes())
            .map_err(|_| premise_mismatch(row))?;
    if original_id.to_string() != row.commit_id {
        return Err(premise_mismatch(row));
    }

    let bytes = rebuild_rewritten_bytes(row, tree, parents);
    let id = ObjectHash::from_type_and_data_for_kind(kind, ObjectType::Commit, &bytes)
        .map_err(|_| premise_mismatch(row))?;
    Ok((id, bytes))
}

pub(crate) fn strip_signature_headers(input: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len());
    let mut cursor = 0;
    let mut skip = false;

    loop {
        let Some(relative_newline) = input[cursor..].iter().position(|byte| *byte == b'\n') else {
            return input.to_vec();
        };
        let line_end = cursor + relative_newline;
        let line = &input[cursor..line_end];
        let next = line_end + 1;

        if line.is_empty() {
            output.push(b'\n');
            output.extend_from_slice(&input[next..]);
            return output;
        }

        if !line.starts_with(b" ") {
            skip = signature_header_line(line);
        }
        if !skip {
            output.extend_from_slice(&input[cursor..next]);
        }
        cursor = next;
    }
}

fn rebuild_rewritten_bytes(row: &mega_commit::Model, tree: &str, parents: &[String]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"tree ");
    bytes.extend_from_slice(tree.as_bytes());
    bytes.push(b'\n');
    for parent in parents {
        bytes.extend_from_slice(b"parent ");
        bytes.extend_from_slice(parent.as_bytes());
        bytes.push(b'\n');
    }
    bytes.extend_from_slice(row.author.as_deref().unwrap_or_default().as_bytes());
    bytes.push(b'\n');
    bytes.extend_from_slice(row.committer.as_deref().unwrap_or_default().as_bytes());
    bytes.push(b'\n');
    bytes.extend_from_slice(&strip_signature_headers(
        row.content.as_deref().unwrap_or_default().as_bytes(),
    ));
    bytes
}

fn signature_header_line(line: &[u8]) -> bool {
    let Some(space) = line.iter().position(|byte| *byte == b' ') else {
        return false;
    };
    std::str::from_utf8(&line[..space]).is_ok_and(is_signature_header)
}

fn premise_mismatch(row: &mega_commit::Model) -> RewriteError {
    RewriteError::PremiseMismatch {
        commit_id: row.commit_id.clone(),
    }
}

fn corrupt_row(row: &mega_commit::Model) -> RewriteError {
    RewriteError::CorruptRow {
        commit_id: row.commit_id.clone(),
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use git_internal::{
        hash::{HashKind, ObjectHash, set_hash_kind_for_test},
        internal::{
            metadata::EntryMeta,
            object::{
                ObjectTrait,
                commit::Commit,
                signature::{Signature, SignatureType},
                types::ObjectType,
            },
        },
    };

    use super::{RewriteError, rewrite_commit, strip_signature_headers};
    use crate::{
        callisto::mega_commit,
        contract::vault::server_signing::{canonical_commit_payload, server_identity_signature},
        jupiter::utils::converter::IntoMegaModel,
    };

    const OLD_TREE: &str = "1111111111111111111111111111111111111111";
    const OLD_PARENT: &str = "2222222222222222222222222222222222222222";
    const NEW_TREE: &str = "3333333333333333333333333333333333333333";
    const NEW_PARENT: &str = "4444444444444444444444444444444444444444";
    const AUTHOR: &str = "author Alice <alice@example.com> 1 +0000";
    const COMMITTER: &str = "committer Bob <bob@example.com> 2 +0000";
    const GOLDEN_INPUT_CONTENT: &str = concat!(
        "encoding ISO-8859-1\n",
        "mergetag object deadbeef\n",
        " type commit\n",
        " -----BEGIN PGP SIGNATURE-----\n",
        " \n",
        " mergetag-signature\n",
        " -----END PGP SIGNATURE-----\n",
        "gpgsig placeholder\n",
        " continuation\n",
        " \n",
        " signature-blank-line\n",
        "gpgsig-sha256 second-placeholder\n",
        " continuation\n",
        "\n",
        "gpgsig body text is retained\n",
    );
    const GOLDEN_OUTPUT_CONTENT: &str = concat!(
        "encoding ISO-8859-1\n",
        "mergetag object deadbeef\n",
        " type commit\n",
        " -----BEGIN PGP SIGNATURE-----\n",
        " \n",
        " mergetag-signature\n",
        " -----END PGP SIGNATURE-----\n",
        "\n",
        "gpgsig body text is retained\n",
    );

    fn raw_commit(
        tree: &str,
        parents: &[&str],
        author: &str,
        committer: &str,
        content: &str,
    ) -> String {
        let mut raw = format!("tree {tree}\n");
        for parent in parents {
            raw.push_str(&format!("parent {parent}\n"));
        }
        raw.push_str(author);
        raw.push('\n');
        raw.push_str(committer);
        raw.push('\n');
        raw.push_str(content);
        raw
    }

    fn commit_hash(raw: &str) -> ObjectHash {
        ObjectHash::from_type_and_data_for_kind(HashKind::Sha1, ObjectType::Commit, raw.as_bytes())
            .unwrap()
    }

    fn row_from_parts(content: &str) -> mega_commit::Model {
        row_from_raw(&raw_commit(
            OLD_TREE,
            &[OLD_PARENT],
            AUTHOR,
            COMMITTER,
            content,
        ))
    }

    fn row_from_raw(raw: &str) -> mega_commit::Model {
        let tree = raw
            .lines()
            .next()
            .unwrap()
            .strip_prefix("tree ")
            .unwrap()
            .to_owned();
        let parents_id = raw
            .lines()
            .filter_map(|line| line.strip_prefix("parent "))
            .collect::<Vec<_>>();
        let author = raw
            .lines()
            .find(|line| line.starts_with("author "))
            .unwrap()
            .to_owned();
        let committer = raw
            .lines()
            .find(|line| line.starts_with("committer "))
            .unwrap()
            .to_owned();
        let content = raw
            .split_once(&(committer.clone() + "\n"))
            .unwrap()
            .1
            .to_owned();
        mega_commit::Model {
            id: 1,
            commit_id: commit_hash(raw).to_string(),
            tree,
            parents_id: serde_json::json!(parents_id),
            author: Some(author),
            committer: Some(committer),
            content: Some(content),
            created_at: Utc::now().naive_utc(),
            pack_id: String::new(),
            pack_offset: 0,
        }
    }

    fn row_through_l0_path(raw: &str) -> mega_commit::Model {
        let commit = <Commit as ObjectTrait>::from_bytes(raw.as_bytes(), commit_hash(raw)).unwrap();
        commit.into_mega_model(EntryMeta::default())
    }

    fn new_parents() -> [String; 1] {
        [NEW_PARENT.to_owned()]
    }

    fn expected_with(tree: &str, parents: &[&str], content: &str) -> Vec<u8> {
        raw_commit(tree, parents, AUTHOR, COMMITTER, content).into_bytes()
    }

    fn hash(hex: &str) -> ObjectHash {
        ObjectHash::from_hex_for_kind(HashKind::Sha1, hex).unwrap()
    }

    fn signature(data: &str) -> Signature {
        Signature::from_data(data.as_bytes().to_vec()).unwrap()
    }

    #[test]
    fn rewrite_matches_golden_bytes() {
        let row = row_from_parts(GOLDEN_INPUT_CONTENT);
        let parents = new_parents();
        let (_, actual) = rewrite_commit(HashKind::Sha1, &row, NEW_TREE, &parents).unwrap();
        let expected = expected_with(NEW_TREE, &[NEW_PARENT], GOLDEN_OUTPUT_CONTENT);
        assert_eq!(actual, expected);

        let (_, no_parent) = rewrite_commit(HashKind::Sha1, &row, NEW_TREE, &[]).unwrap();
        assert_eq!(
            no_parent,
            expected_with(NEW_TREE, &[], GOLDEN_OUTPUT_CONTENT)
        );

        let root = row_from_raw(&raw_commit(
            OLD_TREE,
            &[],
            AUTHOR,
            COMMITTER,
            "\nroot body\n",
        ));
        let (_, root_actual) = rewrite_commit(HashKind::Sha1, &root, NEW_TREE, &[]).unwrap();
        assert_eq!(root_actual, expected_with(NEW_TREE, &[], "\nroot body\n"));

        let no_trailing_newline =
            row_from_parts("gpgsig placeholder\n continuation\n\nbody without a trailing newline");
        let (_, actual) =
            rewrite_commit(HashKind::Sha1, &no_trailing_newline, NEW_TREE, &parents).unwrap();
        assert_eq!(
            actual,
            expected_with(NEW_TREE, &[NEW_PARENT], "\nbody without a trailing newline",)
        );
        assert!(!actual.ends_with(b"\n"));
    }

    #[test]
    fn roundtrip_pass_group() {
        let double_space_author = "author  <empty@example.com> 1 +0000";
        let group = [
            raw_commit(
                OLD_TREE,
                &[OLD_PARENT],
                AUTHOR,
                COMMITTER,
                concat!(
                    "mergetag object deadbeef\n",
                    " type commit\n",
                    " -----BEGIN PGP SIGNATURE-----\n",
                    " \n",
                    " mergetag-signature\n",
                    " -----END PGP SIGNATURE-----\n",
                    "\nmergetag body\n",
                ),
            ),
            raw_commit(
                OLD_TREE,
                &[OLD_PARENT],
                AUTHOR,
                COMMITTER,
                "\nplain unsigned body\n",
            ),
            raw_commit(
                OLD_TREE,
                &[OLD_PARENT],
                AUTHOR,
                COMMITTER,
                "encoding ISO-8859-1\n\nASCII body\n",
            ),
            raw_commit(
                OLD_TREE,
                &[OLD_PARENT],
                AUTHOR,
                COMMITTER,
                "gpgsig signature\n continuation\n\nbody\n",
            ),
            raw_commit(
                OLD_TREE,
                &[OLD_PARENT],
                AUTHOR,
                COMMITTER,
                "gpgsig-sha256 signature\n continuation\n\nbody\n",
            ),
            raw_commit(
                OLD_TREE,
                &[OLD_PARENT],
                double_space_author,
                COMMITTER,
                "\nempty author name\n",
            ),
            raw_commit(
                OLD_TREE,
                &[OLD_PARENT],
                AUTHOR,
                COMMITTER,
                "create new directory demo",
            ),
        ];

        for (case_index, raw) in group.into_iter().enumerate() {
            let row = row_through_l0_path(&raw);
            let parents = new_parents();
            assert!(
                rewrite_commit(HashKind::Sha1, &row, NEW_TREE, &parents).is_ok(),
                "case {case_index}",
            );
        }
    }

    #[test]
    fn premise_fail_group() {
        let uppercase_tree = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_uppercase();
        let uppercase_parent = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_uppercase();
        let group = [
            raw_commit(
                OLD_TREE,
                &[OLD_PARENT],
                "author Alice <alice@example.com> 0001 +0000",
                COMMITTER,
                "\nbody\n",
            ),
            raw_commit(
                OLD_TREE,
                &[OLD_PARENT],
                "author Alice <alice@example.com> +1 +0000",
                COMMITTER,
                "\nbody\n",
            ),
            raw_commit(
                OLD_TREE,
                &[OLD_PARENT],
                "author <alice@example.com> 1 +0000",
                COMMITTER,
                "\nbody\n",
            ),
            raw_commit(
                &uppercase_tree,
                &[OLD_PARENT],
                AUTHOR,
                COMMITTER,
                "\nbody\n",
            ),
            raw_commit(
                OLD_TREE,
                &[&uppercase_parent],
                AUTHOR,
                COMMITTER,
                "\nbody\n",
            ),
        ];

        for (case_index, raw) in group.into_iter().enumerate() {
            let row = row_through_l0_path(&raw);
            let parents = new_parents();
            assert_eq!(
                rewrite_commit(HashKind::Sha1, &row, NEW_TREE, &parents),
                Err(RewriteError::PremiseMismatch {
                    commit_id: row.commit_id.clone(),
                }),
                "case {case_index}",
            );
        }
    }

    #[test]
    fn corrupt_row_returns_error() {
        for parents_id in [
            serde_json::json!({"parent": OLD_PARENT}),
            serde_json::json!([7]),
        ] {
            let mut row = row_from_parts("\nbody\n");
            row.parents_id = parents_id;
            let parents = new_parents();
            assert_eq!(
                rewrite_commit(HashKind::Sha1, &row, NEW_TREE, &parents),
                Err(RewriteError::CorruptRow {
                    commit_id: row.commit_id.clone(),
                })
            );
        }
    }

    #[test]
    fn server_signed_strips_to_canonical_payload() {
        let identities = [
            (
                signature("author Alice <alice@example.com> 1 +0000"),
                signature("committer Bob <bob@example.com> 2 +0000"),
            ),
            (
                server_identity_signature(SignatureType::Author),
                server_identity_signature(SignatureType::Committer),
            ),
        ];
        for (author, committer) in identities {
            for unsigned_message in ["server message", "server message\n"] {
                let mut signed_message = format!(
                    "gpgsig -----BEGIN PGP SIGNATURE-----\n \n placeholder-armor\n -----END PGP SIGNATURE-----\n\n{unsigned_message}"
                );
                if !signed_message.ends_with('\n') {
                    signed_message.push('\n');
                }
                let signed = Commit::new_with_kind(
                    HashKind::Sha1,
                    author.clone(),
                    committer.clone(),
                    hash(OLD_TREE),
                    vec![hash(OLD_PARENT)],
                    &signed_message,
                )
                .unwrap();
                let row = signed.into_mega_model(EntryMeta::default());
                for (tree, parents) in [
                    (OLD_TREE, vec![OLD_PARENT.to_owned()]),
                    (NEW_TREE, new_parents().to_vec()),
                ] {
                    let (_, actual) = rewrite_commit(HashKind::Sha1, &row, tree, &parents).unwrap();
                    let unsigned = Commit::new_with_kind(
                        HashKind::Sha1,
                        author.clone(),
                        committer.clone(),
                        hash(tree),
                        parents.iter().map(|parent| hash(parent)).collect(),
                        unsigned_message,
                    )
                    .unwrap();
                    let expected =
                        canonical_commit_payload(&unsigned, &author, &committer).unwrap();
                    assert_eq!(actual, expected);
                }
            }
        }
    }

    #[test]
    fn unframed_message_untouched() {
        for content in [
            "create new directory demo",
            "gpgsig body line without a header/body separator\nsecond line",
        ] {
            let row = row_from_parts(content);
            let parents = new_parents();
            let (_, actual) = rewrite_commit(HashKind::Sha1, &row, NEW_TREE, &parents).unwrap();
            assert_eq!(actual, expected_with(NEW_TREE, &[NEW_PARENT], content));
            assert_eq!(
                strip_signature_headers(content.as_bytes()),
                content.as_bytes()
            );
        }
    }

    #[test]
    fn id_matches_git_hash_object() {
        let _guard = set_hash_kind_for_test(HashKind::Sha256);
        let row = row_from_parts(GOLDEN_INPUT_CONTENT);
        let parents = new_parents();
        let (id, actual) = rewrite_commit(HashKind::Sha1, &row, NEW_TREE, &parents).unwrap();
        assert_eq!(id.to_string(), "cedd1c2c24a9a7594be49bd892f8cf50abb23eb3");
        assert_eq!(
            actual,
            expected_with(NEW_TREE, &[NEW_PARENT], GOLDEN_OUTPUT_CONTENT)
        );
    }
}
