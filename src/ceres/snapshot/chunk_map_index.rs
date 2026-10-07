//! Canonical MCL2 Merkle subtrees addressed by their exact leaf interval.

use mst2_codec::chunkmap::{ProofSide, ProofStep};
use sha2::{Digest, Sha256};

use super::error::{SnapshotError, SnapshotErrorCode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChunkMapNode {
    pub start: u64,
    pub pages: u64,
    pub digest: [u8; 32],
}

pub(crate) fn indexed_nodes(hashes: &[[u8; 32]]) -> Result<Vec<ChunkMapNode>, SnapshotError> {
    if hashes.is_empty() || hashes.len() > 32_768 {
        return Err(integrity("chunk map is outside the indexed page profile"));
    }
    let mut nodes = Vec::new();
    nodes.try_reserve_exact(hashes.len() * 2 - 1).map_err(|_| {
        SnapshotError::new(
            SnapshotErrorCode::TemporaryUnavailable,
            "chunk map index allocation failed",
        )
    })?;
    build(hashes, 0, &mut nodes);
    Ok(nodes)
}

fn build(hashes: &[[u8; 32]], start: u64, nodes: &mut Vec<ChunkMapNode>) -> [u8; 32] {
    let pages = hashes.len() as u64;
    let digest = if pages == 1 {
        hashes[0]
    } else {
        let split = split(pages);
        let left = build(&hashes[..split as usize], start, nodes);
        let right = build(&hashes[split as usize..], start + split, nodes);
        let mut hash = Sha256::new();
        hash.update(b"mega.mst2.chunkbranch\0");
        hash.update(split.to_le_bytes());
        hash.update(left);
        hash.update((pages - split).to_le_bytes());
        hash.update(right);
        hash.finalize().into()
    };
    nodes.push(ChunkMapNode {
        start,
        pages,
        digest,
    });
    digest
}

fn split(pages: u64) -> u64 {
    1 << (63 - (pages - 1).leading_zeros())
}

/// Only these sibling intervals may be fetched for a selected page.
pub(crate) fn proof_intervals(
    pages: u64,
    index: u64,
) -> Result<Vec<(ProofSide, u64, u64)>, SnapshotError> {
    if pages == 0 || pages > 32_768 || index >= pages {
        return Err(SnapshotError::new(
            SnapshotErrorCode::PathNotFound,
            "chunk map page does not exist",
        ));
    }
    let (mut start, mut count) = (0, pages);
    let mut intervals = Vec::new();
    while count > 1 {
        let left = split(count);
        if index < start + left {
            intervals.push((ProofSide::Right, start + left, count - left));
            count = left;
        } else {
            intervals.push((ProofSide::Left, start, left));
            start += left;
            count -= left;
        }
    }
    intervals.reverse();
    Ok(intervals)
}

pub(crate) fn selected_proof(
    intervals: &[(ProofSide, u64, u64)],
    nodes: &[ChunkMapNode],
) -> Result<Vec<ProofStep>, SnapshotError> {
    if nodes.len() != intervals.len() {
        return Err(integrity(
            "persisted chunk map proof coverage is incomplete",
        ));
    }
    intervals
        .iter()
        .map(|&(side, start, pages)| {
            let mut matching = nodes
                .iter()
                .filter(|n| n.start == start && n.pages == pages);
            let node = matching
                .next()
                .ok_or_else(|| integrity("persisted chunk map sibling is missing"))?;
            if matching.next().is_some() {
                return Err(integrity("persisted chunk map sibling is duplicated"));
            }
            Ok(ProofStep {
                side,
                sibling_pages: pages,
                digest: node.digest,
            })
        })
        .collect()
}

fn integrity(message: &str) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::IntegrityError, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexed_selected_proofs_match_independent_codec_for_uneven_trees() {
        for count in [1, 2, 3, 5, 9, 17, 257, 32_768] {
            let hashes: Vec<[u8; 32]> = (0u64..count)
                .map(|n| Sha256::digest(n.to_le_bytes()).into())
                .collect();
            let nodes = indexed_nodes(&hashes).unwrap();
            assert_eq!(nodes.len(), hashes.len() * 2 - 1);
            let root = mst2_codec::chunkmap::merkle_root(&hashes).unwrap();
            assert_eq!(nodes.last().unwrap().digest, root);
            for page in [0, count / 2, count - 1] {
                let intervals = proof_intervals(count, page).unwrap();
                assert!(intervals.len() <= 15);
                let selected: Vec<_> = nodes
                    .iter()
                    .filter(|n| {
                        intervals
                            .iter()
                            .any(|&(_, s, c)| n.start == s && n.pages == c)
                    })
                    .copied()
                    .collect();
                let proof = selected_proof(&intervals, &selected).unwrap();
                assert_eq!(
                    proof,
                    mst2_codec::chunkmap::leaf_proof(&hashes, page).unwrap()
                );
                mst2_codec::chunkmap::verify_leaf(count, page, hashes[page as usize], &proof, root)
                    .unwrap();
                if !selected.is_empty() {
                    assert!(selected_proof(&intervals, &selected[1..]).is_err());
                    let mut bad = proof.clone();
                    bad[0].digest[0] ^= 1;
                    assert!(
                        mst2_codec::chunkmap::verify_leaf(
                            count,
                            page,
                            hashes[page as usize],
                            &bad,
                            root
                        )
                        .is_err()
                    );
                }
            }
        }
    }
}
