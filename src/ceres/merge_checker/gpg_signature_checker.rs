use std::sync::Arc;

use async_trait::async_trait;
use git_internal::internal::object::{ObjectTrait, commit::Commit};
use pgp::{
    composed::{Deserializable, DetachedSignature, SignedPublicKey},
    packet::{Signature, SubpacketData},
};
use serde::Deserialize;
use serde_json::Value;

use crate::{
    callisto::mega_commit,
    ceres::merge_checker::{
        CheckResult, CheckType, Checker, ConditionResult, MAX_CL_CHAIN_COMMITS,
    },
    common::errors::MegaError,
    contract::vault::server_signing::{SERVER_SIGNING_EMAIL, SERVER_SIGNING_NAME},
    jupiter::{model::cl_dto::ClInfoDto, storage::Storage},
};

pub struct GpgSignatureChecker {
    pub storage: Arc<Storage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GpgSignatureParams {
    cl_from: String,
    cl_to: String,
}

impl GpgSignatureParams {
    fn from_value(v: &serde_json::Value) -> anyhow::Result<Self> {
        Ok(serde_json::from_value(v.clone())?)
    }
}

#[async_trait]
impl Checker for GpgSignatureChecker {
    async fn run(&self, params: &serde_json::Value) -> crate::ceres::merge_checker::CheckResult {
        let params = GpgSignatureParams::from_value(params).expect("parse params err");
        let mut res = CheckResult {
            check_type_code: CheckType::GpgSignature,
            status: ConditionResult::FAILED,
            message: String::new(),
        };

        let is_verified = self.verify_cl(&params.cl_from, &params.cl_to).await;
        match is_verified {
            Ok(_) => {
                res.status = ConditionResult::PASSED;
                res.message = String::from("PASSED");
            }

            Err(e) => {
                res.status = ConditionResult::FAILED;
                res.message = format!("Error during GPG signature verification: {e}");
            }
        };

        res
    }

    async fn build_params(&self, cl_info: &ClInfoDto) -> Result<Value, MegaError> {
        Ok(serde_json::json!({
            "cl_from": cl_info.from_hash,
            "cl_to": cl_info.to_hash,
        }))
    }
}

impl GpgSignatureChecker {
    /// Verify every commit in the CL's cumulative range `(cl_from, cl_to]`
    /// (ADR-MC-03): walk the first-parent chain from `cl_to` towards
    /// `cl_from`; the walk must reach `cl_from` as the range boundary
    /// (fail-closed otherwise) and is bounded by `MAX_CL_CHAIN_COMMITS`
    /// (ADR-MC-07; the receive side enforces the same bound, this is defense
    /// in depth). `cl_from` itself is the baseline, not a CL member, and is
    /// not verified.
    ///
    /// The walk follows first parents only: the chain is assumed linear.
    /// That assumption is enforced on the receive side, which rejects merge
    /// commits on push (plan-20260827 MC-03; still in force once MC-06 opens
    /// multi-commit pushes).
    async fn verify_cl(&self, cl_from: &str, cl_to: &str) -> Result<(), MegaError> {
        let storage = self.storage.mono_storage();
        let mut current = storage
            .get_commit_by_hash(cl_to)
            .await?
            .ok_or_else(|| MegaError::Other(format!("commit {cl_to} not found")))?;

        let mut verified = 0usize;
        loop {
            if verified >= MAX_CL_CHAIN_COMMITS {
                return Err(MegaError::Other(format!(
                    "CL chain exceeds the {MAX_CL_CHAIN_COMMITS}-commit limit \
                     ({cl_from}..{cl_to}); merge the current CL first or squash and re-push"
                )));
            }
            self.verify_commit_chain_member(&current)
                .await
                .map_err(|e| {
                    MegaError::Other(format!("commit {}: {e}", sha_prefix(&current.commit_id)))
                })?;
            verified += 1;

            if current.commit_id == cl_from {
                // Degenerate empty range (from == to): the tip was verified, stop.
                break;
            }
            let parents: Vec<String> =
                serde_json::from_value(current.parents_id.clone()).map_err(|e| {
                    MegaError::Other(format!(
                        "corrupt parents_id for commit {}: {e}",
                        current.commit_id
                    ))
                })?;
            match parents.first() {
                // The base is the range boundary, not a CL member: stop before it.
                Some(parent) if parent == cl_from => break,
                Some(parent) => {
                    current = storage
                        .get_commit_by_hash(parent)
                        .await?
                        .ok_or_else(|| MegaError::Other(broken_chain_message(cl_from, cl_to)))?;
                }
                None => return Err(MegaError::Other(broken_chain_message(cl_from, cl_to))),
            }
        }
        Ok(())
    }

    /// Verify one chain member. The committer header selects the trust
    /// domain (ADR-MC-08): the reserved server identity verifies against the
    /// server keyring only — falling back to the user keyring would be a
    /// spoofing escape hatch — while any other identity follows MC-02 step
    /// ①: the signature's issuer fingerprint (resolved fail-closed from the
    /// hashed subpacket area) selects the registered user key. The payload
    /// is the canonical full commit byte stream rebuilt from the persisted
    /// columns (ADR-MC-09), not just the message.
    async fn verify_commit_chain_member(
        &self,
        commit: &mega_commit::Model,
    ) -> Result<(), MegaError> {
        let raw = rebuild_canonical_commit_bytes(commit)?;
        self.verify_raw_commit_bytes(
            &raw,
            committer_is_server_identity(commit.committer.as_deref()),
        )
        .await
    }

    /// Shared per-commit verification: strip the `gpgsig` block, then verify
    /// the canonical payload against the keyring selected by the committer
    /// header. `server_identity` is the caller-resolved ADR-MC-08 step ②
    /// verdict for the commit's committer.
    async fn verify_raw_commit_bytes(
        &self,
        raw: &str,
        server_identity: bool,
    ) -> Result<(), MegaError> {
        let (payload, signature) = extract_from_commit_content(raw);
        let Some(signature) = signature else {
            if server_identity {
                // Historical server-synthesized commit predating MC-09 —
                // distinct, actionable message (never the user-facing one).
                return Err(MegaError::Other(
                    "server-synthesized commit carries no GPG signature (historical commit \
                     predating server signing); fix: re-trigger the synthesis (e.g. \
                     update-branch or buck upload) or contact an administrator"
                        .to_string(),
                ));
            }
            return Err(MegaError::Other("no GPG signature found".to_string()));
        };

        if server_identity {
            return self.verify_with_server_keyring(&signature, &payload).await;
        }

        // User identity (ADR-MC-08 step ①, MC-02): issuer fingerprint reverse
        // lookup, fail-closed.
        let sig = DetachedSignature::from_string(&signature)
            .map_err(|e| MegaError::Other(format!("failed to parse signature: {e}")))?
            .0;
        let fingerprint = resolve_issuer_fingerprint(&sig.signature)?;

        let key = self
            .storage
            .gpg_storage()
            .find_gpg_key_by_fingerprint(&fingerprint)
            .await?
            .ok_or_else(|| {
                MegaError::Other("signature issuer key is not registered to any user".to_string())
            })?;

        self.verify_signature_with_key(&key.public_key, &signature, &payload)
            .await
    }

    /// Server trust domain (ADR-MC-08 step ②): verify against the server
    /// keyring only — every non-revoked historical public key loaded from
    /// the vault through MC-09's read-only interface, so commits signed by
    /// rotated-out generations stay verifiable. The vault handle comes from
    /// the checker's storage; absent or empty keyring → fail-closed. No
    /// fallback to the user keyring.
    async fn verify_with_server_keyring(
        &self,
        signature: &str,
        payload: &str,
    ) -> Result<(), MegaError> {
        let vault = self.storage.vault().ok_or_else(|| {
            MegaError::Other(
                "server signing unavailable: vault handle is not configured".to_string(),
            )
        })?;
        let keys = vault.list_server_signing_public_keys().await?;
        let sig = DetachedSignature::from_string(signature)
            .map_err(|e| MegaError::Other(format!("failed to parse signature: {e}")))?
            .0;
        for key in &keys {
            if sig.verify(&key.public_key, payload.as_bytes()).is_ok() {
                return Ok(());
            }
        }
        Err(MegaError::Other(
            "server-identity commit not verified by any non-revoked server signing key \
             (user keyring fallback is forbidden)"
                .to_string(),
        ))
    }

    /// Commit status/badge path (consumer: `api_service::commit_ops.rs`,
    /// cache key `gpg_status:v2`): verifies one commit from its live git
    /// object under the same trust domain as the chain path
    /// (`verify_commit_chain_member`) — canonical full commit bytes
    /// (ADR-MC-09), keyring selected by the commit's own committer header
    /// (ADR-MC-08). No externally supplied username is consulted.
    pub(crate) async fn verify_commit_gpg_signature(
        &self,
        commit: &Commit,
    ) -> Result<(), MegaError> {
        let raw = commit.to_data().map_err(|e| {
            MegaError::Other(format!("failed to serialize commit {}: {e}", commit.id))
        })?;
        let raw = String::from_utf8_lossy(&raw);
        self.verify_raw_commit_bytes(
            &raw,
            is_server_identity(&commit.committer.name, &commit.committer.email),
        )
        .await
    }

    async fn verify_signature_with_key(
        &self,
        public_key: &str,
        signature: &str,
        message: &str,
    ) -> Result<(), MegaError> {
        let pub_key = SignedPublicKey::from_string(public_key)
            .map_err(|e| MegaError::Other(format!("Failed to parse public key: {e}")))?
            .0;
        let sig = DetachedSignature::from_string(signature)
            .map_err(|e| MegaError::Other(format!("Failed to parse signature: {e}")))?
            .0;
        let bytes = message.as_bytes();
        sig.verify(&pub_key, bytes)
            .map_err(|e| MegaError::Other(format!("Signature verification failed: {e}")))?;

        Ok(())
    }
}

fn sha_prefix(sha: &str) -> &str {
    &sha[..7.min(sha.len())]
}

/// Whether the identity (name + email) is the reserved server identity
/// (MC-09 constants; ADR-MC-08 step ②).
fn is_server_identity(name: &str, email: &str) -> bool {
    name == SERVER_SIGNING_NAME && email == SERVER_SIGNING_EMAIL
}

/// Parse the persisted committer header (git-internal `Signature::to_data`
/// output: `committer <name> <<email>> <ts> <tz>`) into (name, email)
/// without panicking on malformed input. Anything unparseable is treated as
/// a user identity — the user path is fail-closed anyway.
fn parse_committer_identity(raw: &str) -> Option<(&str, &str)> {
    let rest = raw.strip_prefix("committer ")?;
    let lt = rest.rfind('<')?;
    let gt = rest[lt..].find('>')? + lt;
    let name = rest[..lt].strip_suffix(' ')?;
    let email = &rest[lt + 1..gt];
    Some((name, email))
}

/// The ADR-MC-08 step ② verdict for a persisted `mega_commit.committer`
/// column value.
fn committer_is_server_identity(committer: Option<&str>) -> bool {
    committer
        .and_then(parse_committer_identity)
        .is_some_and(|(name, email)| is_server_identity(name, email))
}

/// The walk reached the chain root (or a gap in storage) without meeting the
/// CL base: the range is not a contiguous chain.
fn broken_chain_message(cl_from: &str, cl_to: &str) -> String {
    format!(
        "broken commit chain: {cl_from} is not an ancestor of {cl_to}. \
         This can happen when an open CL for the same path and user was rebased or \
         diverged; close or update that CL (or rebase onto its base) and re-push"
    )
}

/// Rebuild the canonical full commit bytes from the persisted `mega_commit`
/// columns (ADR-MC-09): `tree {tree}\n`, one `parent {id}\n` per `parents_id`
/// entry in stored order, then the author and committer lines (the columns
/// already carry the `author `/`committer ` prefixes — git-internal
/// `Signature::to_data`), then `content` verbatim (which holds the `gpgsig`
/// block for signed commits). `extract_from_commit_content` strips the
/// signature block from this byte stream, yielding exactly the payload git
/// signed.
pub(crate) fn rebuild_canonical_commit_bytes(
    commit: &mega_commit::Model,
) -> Result<String, MegaError> {
    let parents: Vec<String> = serde_json::from_value(commit.parents_id.clone()).map_err(|e| {
        MegaError::Other(format!(
            "corrupt parents_id for commit {}: {e}",
            commit.commit_id
        ))
    })?;
    let mut raw = String::from("tree ");
    raw.push_str(&commit.tree);
    raw.push('\n');
    for parent in parents {
        raw.push_str("parent ");
        raw.push_str(&parent);
        raw.push('\n');
    }
    raw.push_str(commit.author.as_deref().unwrap_or_default());
    raw.push('\n');
    raw.push_str(commit.committer.as_deref().unwrap_or_default());
    raw.push('\n');
    raw.push_str(commit.content.as_deref().unwrap_or_default());
    Ok(raw)
}

/// Resolve the signature's issuer fingerprint, fail-closed (ADR-MC-08):
/// exactly one `IssuerFingerprint` subpacket in the hashed area, and every
/// unhashed occurrence must agree with it. The returned string is normalized
/// to the `gpg_key.fingerprint` storage format (`format!("{:?}", ..)`, see
/// `GpgStorage::create_key`).
fn resolve_issuer_fingerprint(sig: &Signature) -> Result<String, MegaError> {
    let config = sig
        .config()
        .ok_or_else(|| MegaError::Other("unsupported signature packet".to_string()))?;
    let hashed: Vec<_> = config
        .hashed_subpackets()
        .filter_map(|sp| match &sp.data {
            SubpacketData::IssuerFingerprint(fp) => Some(fp),
            _ => None,
        })
        .collect();
    let unhashed: Vec<_> = config
        .unhashed_subpackets()
        .filter_map(|sp| match &sp.data {
            SubpacketData::IssuerFingerprint(fp) => Some(fp),
            _ => None,
        })
        .collect();

    let [issuer] = hashed.as_slice() else {
        return Err(MegaError::Other(format!(
            "signature must carry exactly one issuer fingerprint in its hashed area \
             (found {})",
            hashed.len()
        )));
    };
    if unhashed.iter().any(|fp| fp != issuer) {
        return Err(MegaError::Other(
            "unhashed issuer fingerprint conflicts with the hashed one".to_string(),
        ));
    }
    Ok(format!("{issuer:?}"))
}

/// Split a full commit byte stream (or a bare `content` column value, whose
/// header region is its start up to the first empty line) into the payload
/// git signed and the detached armor carried by its `gpgsig` header.
///
/// The `gpgsig` header is recognized only inside the header region — the
/// lines before the first *empty* (zero-character) line — mirroring git:
/// an armor block forged inside the message body is not a signature and
/// leaves the commit unsigned (fail-closed). Header continuation lines carry
/// exactly one leading space, which is stripped; the armor's internal blank
/// line arrives as a single-space continuation `" "`, never as an empty
/// line, so it cannot be mistaken for the header/body boundary. The header
/// line and its continuations are removed from the payload, which then keeps
/// git's canonical shape (remaining headers, blank line, message).
pub(crate) fn extract_from_commit_content(msg_gpg: &str) -> (String, Option<String>) {
    const GPGSIG_PREFIX: &str = "gpgsig ";

    let mut payload = String::with_capacity(msg_gpg.len());
    let mut signature: Option<String> = None;
    let mut in_header = true;
    let mut in_gpgsig_continuation = false;

    for line in msg_gpg.split_inclusive('\n') {
        let text = line.strip_suffix('\n').unwrap_or(line);

        if in_gpgsig_continuation {
            match text.strip_prefix(' ') {
                Some(continuation) => {
                    if let Some(sig) = signature.as_mut() {
                        sig.push('\n');
                        sig.push_str(continuation);
                    }
                    continue;
                }
                None => in_gpgsig_continuation = false,
            }
        }

        if in_header {
            if text.is_empty() {
                in_header = false;
            } else if signature.is_none()
                && let Some(value) = text.strip_prefix(GPGSIG_PREFIX)
            {
                signature = Some(value.to_string());
                in_gpgsig_continuation = true;
                continue;
            }
        }

        payload.push_str(line);
    }

    let Some(mut signature) = signature else {
        return (msg_gpg.to_string(), None);
    };
    signature.push('\n');

    // Historical payload shape (locked by `test_commits_verification` and the
    // badge path): leading newlines left by a removed first header line are
    // stripped, and the payload always ends with exactly one newline.
    let mut payload = payload.trim_start_matches('\n').to_string();
    if !payload.ends_with('\n') {
        payload.push('\n');
    }
    (payload, Some(signature))
}

/// Real GnuPG-signed commit in git object byte format — the external format
/// anchor shared by `test_commits_verification` and the full-chain
/// `real_git_signed_commit_passes_full_chain` test. Do not edit the bytes.
#[cfg(test)]
const REAL_SIGNED_COMMIT: &str = r#"tree 52a266a58f2c028ad7de4dfd3a72fdf76b0d4e24
author AidCheng <cn.aiden.cheng@gmail.com> 1758211153 +0100
committer AidCheng <cn.aiden.cheng@gmail.com> 1758211153 +0100
gpgsig -----BEGIN PGP SIGNATURE-----
 
 iQGzBAABCAAdFiEExZ7S27JTGFCk8+gpQv9AeDZzXb0FAmjMLFEACgkQQv9AeDZz
 Xb3zKQwAoZ+CpV7x8SyF2ZWm3MJZmqa12G51s1/N1/ND6VRiyO6cIS71w5nA0RY/
 vNwQlxjD2gDq8MFr8eFIwQCBrHVFx4QrmAnLNhtbnf9fGws//nPEPrzm3bHaRmt4
 tnjstUEJDE2sKztTcics740FZJnXW13MGQZV6HEODYo00zldOUYWqNflTYt8oVmQ
 LrPWncxOVXBKnhs1X+Zh8aJIj5Gnqrl0A8PMRlqSOKMEQzZD0Erd2/Fj+uzemGwl
 EexiTwtuoMBSjAWCTholW8HzvHOoSvSj3fV5bKD7XbWtBaB62rCtqNvqJK2QX8aM
 fSNM3KnloWz+sDFGYnmGacNsn+uqxF517DT/mqJeNMrI2MWGMzuQolHqeNTRSCiq
 Mvslf2i50W0P8npM+U+JJBIaNGReslK0zlsSwc4X50ReDXJxxi5QvlS0WLEmJIOl
 f58UBUCXm/LEYABTW5xKdEFoSxmpGcZ09G7/O6CvqPVau0gGcqwjl4LP3/496ifz
 jjI4Ah4p
 =QXLc
 -----END PGP SIGNATURE-----

test

Signed-off-by: AidCheng <cn.aiden.cheng@gmail.com>"#;

#[test]
fn test_commits_verification() {
    let cm = REAL_SIGNED_COMMIT;
    let (msg, sig) = extract_from_commit_content(cm);
    let sig = sig.expect("unable to parse");
    println!("{msg}\n{sig}");

    assert_eq!(
        msg,
        r#"tree 52a266a58f2c028ad7de4dfd3a72fdf76b0d4e24
author AidCheng <cn.aiden.cheng@gmail.com> 1758211153 +0100
committer AidCheng <cn.aiden.cheng@gmail.com> 1758211153 +0100

test

Signed-off-by: AidCheng <cn.aiden.cheng@gmail.com>
"#
    );

    assert_eq!(
        sig,
        r#"-----BEGIN PGP SIGNATURE-----

iQGzBAABCAAdFiEExZ7S27JTGFCk8+gpQv9AeDZzXb0FAmjMLFEACgkQQv9AeDZz
Xb3zKQwAoZ+CpV7x8SyF2ZWm3MJZmqa12G51s1/N1/ND6VRiyO6cIS71w5nA0RY/
vNwQlxjD2gDq8MFr8eFIwQCBrHVFx4QrmAnLNhtbnf9fGws//nPEPrzm3bHaRmt4
tnjstUEJDE2sKztTcics740FZJnXW13MGQZV6HEODYo00zldOUYWqNflTYt8oVmQ
LrPWncxOVXBKnhs1X+Zh8aJIj5Gnqrl0A8PMRlqSOKMEQzZD0Erd2/Fj+uzemGwl
EexiTwtuoMBSjAWCTholW8HzvHOoSvSj3fV5bKD7XbWtBaB62rCtqNvqJK2QX8aM
fSNM3KnloWz+sDFGYnmGacNsn+uqxF517DT/mqJeNMrI2MWGMzuQolHqeNTRSCiq
Mvslf2i50W0P8npM+U+JJBIaNGReslK0zlsSwc4X50ReDXJxxi5QvlS0WLEmJIOl
f58UBUCXm/LEYABTW5xKdEFoSxmpGcZ09G7/O6CvqPVau0gGcqwjl4LP3/496ifz
jjI4Ah4p
=QXLc
-----END PGP SIGNATURE-----
"#
    );

    let pk = REAL_PUBLIC_KEY;
    let pub_key = SignedPublicKey::from_string(pk)
        .expect("unable to parse key")
        .0;
    let sig = DetachedSignature::from_string(&sig)
        .expect("unable to parse sig")
        .0;
    let bytes = msg.as_bytes();
    sig.verify(&pub_key, bytes).expect("unable to verify");
}

/// The public key matching `REAL_SIGNED_COMMIT`'s GnuPG signature.
/// Do not edit the bytes.
#[cfg(test)]
const REAL_PUBLIC_KEY: &str = r#"-----BEGIN PGP PUBLIC KEY BLOCK-----

mQGNBGiGkcsBDADDQzGo993e+e/6h5lvYGtPt2kSHAmGIXyzeNUsePfEE2lewNLl
uAnAUR56A5vxyV0zER1F8Sp2OGXola/x6yT86c0ZRQ6nItMojYTKJUfcy7o56F9Z
eL515XqFz5x29NXKfqaHc+EblqbvPIocC+uGEQD6l5nee6BDxmachUg+4SO8mqjd
xmaGfpka0mmzQK2xgnFTsR0SkYXKmwf/w81vv5z53nXkJRUWUlZ0PHaCCaxO65fV
vbLtaRVp7niRWnxmttNwG23AlIDDeSRaQ8FqJrCN3ZAdpfMoPmOZ1IWEmEb4p0Pn
0vTz5WeT4kR9SmpMqbkpChWYaX8EgCpNrSqV62hrapVJ42fGb9nocuqSDNk7qrBY
+EnzlPNbTSy9x0e7sbffvCrjxCfOnV6KmkPNBTs4un7cIThfyvZz5Aaw/BM6xT4v
/01m7VLwT/+ZBKSP6GpRntsSnBitsUXtgN9URV+vnRMgMaXRjESIvWjeB+qMxBDU
MhrN7eTQ11ByqsEAEQEAAbRLQWlkZW4gQ2hlbmcgKENvbnRhY3QgbXkgZW1haWwg
Zm9yIGFueSBxdWVzdGlvbnMpIDxjbi5haWRlbi5jaGVuZ0BnbWFpbC5jb20+iQHR
BBMBCAA7FiEExZ7S27JTGFCk8+gpQv9AeDZzXb0FAmiGkcsCGwMFCwkIBwICIgIG
FQoJCAsCBBYCAwECHgcCF4AACgkQQv9AeDZzXb0EGQv8C+1XkqXqLVmdWRKOhzJG
XL7RB9Oexh2ueJYlojpyCFs8KXYGzIf/8L2SPxAmBh1ayEcDqvgUoYc/0lOl7pQr
1rl7ZS2iGtYyxF9kyw0OEyLpXeQOXb1lUPG/k7S5xUq+xtsoByMhxJJDmW3fD99p
LP1ApAW5P8jx4E3wdirxKb+5fip3CFvk0/pLzwCxxIf15ijG4nlWi/ZWIHo/VsMx
GATOyL2Bn3BEaT95LEtvaEItyjnGp+bixqeZOlYFckQDG8nX7KQvZNQtJ9Ux9UJX
DJ5OdSGwSv98EMMwmbAv9UGhANkgv+FAxAaQ1FCGHZD9PN+jVxlAVK4jDJeP4DOc
BWU3SEWAVJqISa9ZmhBAU3mRJMYT6qqSl4r07tc1Ii8WYwFEnccjSi1axomnBln7
Dy01QggQNbLMAu/70HG28vDVtxBfe+WyAI/D59/uCnNO1phpQ7XVSN0D+dkleqba
l6aRFd3Ll7EZDpW1kU+cmFbTFzo0ScUEdswT5pFO9JsguQGNBGiGkcsBDACVoLqH
bM72FQP9delVRRY0UH0XbS7AmtzQ8wGZx2Wb3bHaY2H8WPJ4Zt/XWbplIy2sB9XC
GcOTc5WXBOC74YQJ9Ub4o7+92S0ZFVaY7v3KGTXfSW4G+nghu0aS2RTXL2GctUJy
pSQmmX0yIR9vhFA65OtaG9QzY3vAXXtMWoCLYIIcOC/2b5F3KhEjK0YwDLCpBo2g
7Xmu2o4kPY1Uuri1DGnfbMtJ1Ac2Mc2YodAlj9lapG2f5G2NV2TKwdYnCu8gCZ+z
k+kaa9v7yn8O8j1kBEW5dmnaR0l0rGJXQcp6ffyzI0ulZB6f2u8lroAMf1j0oD45
CS1kEZ/xL+N7WJ2JYWXHV4VdRZ3QOfDpSUzSh1wQ9Z1kVF8gHVsVtk5eSBsORFVp
pa1sVkba5eOIfOIPYqYwI+0qbKMe1SCLL88Hfhm1xoK7gQ1ErsxgqlAd8OP/SQ0y
cqsyrVuyrrhlursQFW6Mo8o0aFKrJ1DWjgUX3By/pWp7n788ZD/dSYGOLs8AEQEA
AYkBtgQYAQgAIBYhBMWe0tuyUxhQpPPoKUL/QHg2c129BQJohpHLAhsMAAoJEEL/
QHg2c129fNoL/0OK3CBZgrvbzTXprRDc19AoDLfViIY0/nEAVITCvrTVMZXBD1Dx
JN9cbvinjeZEUsoXsBHcbz2wUn1bhq/58e0ki1XmAC0ZJtJLFtbLAAvTJ2Wo56Os
PNmE7OOV4VtHF4UWfRbkvg86oCjIY9+TS5v25GKIEkMZRFsNiVpC0uK5kNyaHeRB
RqlG5ZV4pO12+EN66agJfRLRwlOmsyJ51/gFzdxP8Lygh2Br2WYU9girwxfQhUs/
DeSsNHPkv1ESPvA2vDsKHLiatnp1gJfyC0vIgKUdG/v/7FzFtY2B3B0EbBD6iBZN
cPVW3TGw8pbK+HT7vBhWjYxpk0evfVCUd67eXtsexutj30YszRcNn5ja+cfGBD+R
dpDLm5hpuQtgfJYTuvwRtabRCZG8oRsOZfuRIkxWwN+VcjvmjWUF/1lSetAhpWs+
VEG4kspCB7X0ePlBP1jPaOWzVphmV0e1eHo79qKS6038FySK81stvRux0DP57E3n
F5MtAwnDBeT2Qg==
=Q/C5
-----END PGP PUBLIC KEY BLOCK-----
"#;

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use pgp::{
        composed::{
            ArmorOptions, KeyType, SecretKeyParamsBuilder, SignedSecretKey, SubpacketConfig,
        },
        crypto::hash::HashAlgorithm,
        packet::Subpacket,
        types::{KeyDetails, KeyVersion, Password, Timestamp},
    };
    use sea_orm::{ActiveModelTrait, Set};
    use tempfile::TempDir;

    use super::*;
    use crate::{
        callisto::sea_orm_active_enums::MergeStatusEnum,
        contract::vault::{
            integration::vault_core::VaultCore, server_signing::ServerSigningContext,
        },
        jupiter::{
            model::cl_dto::ClInfoDto, storage::base_storage::StorageConnector, tests::test_storage,
        },
    };

    const AUTHOR: &str = "author Test User <test@example.com> 1750000000 +0000";
    const COMMITTER: &str = "committer Test User <test@example.com> 1750000000 +0000";

    fn sha(n: u64) -> String {
        format!("{n:040x}")
    }

    fn generate_key(uid: &str) -> SignedSecretKey {
        let mut params = SecretKeyParamsBuilder::default();
        params
            .version(KeyVersion::V4)
            .key_type(KeyType::Ed25519Legacy)
            .can_certify(true)
            .can_sign(true)
            .primary_user_id(uid.into());
        params
            .build()
            .expect("key params")
            .generate(rand08::thread_rng())
            .expect("generate key")
    }

    fn public_key_armored(key: &SignedSecretKey) -> String {
        SignedPublicKey::from(key.clone())
            .to_armored_string(ArmorOptions::default())
            .expect("armor public key")
    }

    fn sign_payload(key: &SignedSecretKey, payload: &str, subpackets: SubpacketConfig) -> String {
        DetachedSignature::sign_binary_data_with_subpackets(
            rand08::thread_rng(),
            &key.primary_key,
            &Password::empty(),
            HashAlgorithm::Sha256,
            payload.as_bytes(),
            subpackets,
        )
        .expect("sign payload")
        .to_armored_string(ArmorOptions::default())
        .expect("armor signature")
    }

    /// The exact byte stream git signs: headers (tree, parents, author,
    /// committer), a blank line, then the message.
    fn canonical_payload(tree: &str, parents: &[String], message: &str) -> String {
        let mut payload = format!("tree {tree}\n");
        for parent in parents {
            payload.push_str("parent ");
            payload.push_str(parent);
            payload.push('\n');
        }
        payload.push_str(AUTHOR);
        payload.push('\n');
        payload.push_str(COMMITTER);
        payload.push('\n');
        payload.push('\n');
        payload.push_str(message);
        if !payload.ends_with('\n') {
            payload.push('\n');
        }
        payload
    }

    /// Embed an armored detached signature into the commit the way git does:
    /// a `gpgsig ` header whose continuation lines are space-prefixed, then a
    /// blank line and the message. This is what the `content` column holds
    /// for a signed commit (git-internal `Commit.message` keeps the gpgsig
    /// block).
    fn signed_content(message: &str, armored_sig: &str) -> String {
        let mut content = String::from("gpgsig ");
        for (i, line) in armored_sig.trim_end_matches('\n').lines().enumerate() {
            if i > 0 {
                content.push(' ');
            }
            content.push_str(line);
            content.push('\n');
        }
        content.push('\n');
        content.push_str(message);
        content
    }

    fn commit_model(
        id: i64,
        commit_sha: &str,
        tree: &str,
        parents: &[String],
        content: Option<String>,
    ) -> mega_commit::Model {
        mega_commit::Model {
            id,
            commit_id: commit_sha.to_string(),
            tree: tree.to_string(),
            parents_id: serde_json::json!(parents),
            author: Some(AUTHOR.to_string()),
            committer: Some(COMMITTER.to_string()),
            content,
            created_at: chrono::Utc::now().naive_utc(),
            pack_id: String::new(),
            pack_offset: 0,
        }
    }

    /// Sign `message` over the canonical payload of the described commit and
    /// return the persistable model. Asserts the ADR-MC-09 round-trip:
    /// rebuilding the commit bytes from the persisted columns and stripping
    /// the gpgsig block reproduces exactly the signed payload.
    fn make_signed_commit(
        key: &SignedSecretKey,
        id: i64,
        commit_sha: &str,
        tree: &str,
        parents: &[String],
        message: &str,
        subpackets: SubpacketConfig,
    ) -> mega_commit::Model {
        let payload = canonical_payload(tree, parents, message);
        let armored = sign_payload(key, &payload, subpackets);
        let model = commit_model(
            id,
            commit_sha,
            tree,
            parents,
            Some(signed_content(message, &armored)),
        );
        let raw = rebuild_canonical_commit_bytes(&model).expect("rebuild commit bytes");
        let (extracted, sig) = extract_from_commit_content(&raw);
        assert!(sig.is_some(), "embedded signature must be extractable");
        assert_eq!(
            extracted, payload,
            "rebuilt-and-stripped commit must equal the signed payload"
        );
        model
    }

    async fn setup_checker() -> (TempDir, Arc<Storage>, GpgSignatureChecker) {
        let temp = TempDir::new().expect("temp dir");
        let storage = Arc::new(test_storage(temp.path()).await);
        let checker = GpgSignatureChecker {
            storage: storage.clone(),
        };
        (temp, storage, checker)
    }

    async fn insert_commit(storage: &Storage, model: &mega_commit::Model) {
        mega_commit::ActiveModel {
            id: Set(model.id),
            commit_id: Set(model.commit_id.clone()),
            tree: Set(model.tree.clone()),
            parents_id: Set(model.parents_id.clone()),
            author: Set(model.author.clone()),
            committer: Set(model.committer.clone()),
            content: Set(model.content.clone()),
            created_at: Set(model.created_at),
            pack_id: Set(model.pack_id.clone()),
            pack_offset: Set(model.pack_offset),
        }
        .insert(storage.mono_storage().get_connection())
        .await
        .expect("insert commit");
    }

    async fn register_key(storage: &Storage, user: &str, key: &SignedSecretKey) {
        storage
            .gpg_storage()
            .add_gpg_key(user.to_string(), public_key_armored(key))
            .await
            .expect("register key");
    }

    fn cl_info(from: &str, to: &str, owner: &str) -> ClInfoDto {
        ClInfoDto {
            link: "CLGPG001".to_string(),
            title: "gpg test".to_string(),
            merge_date: None,
            status: MergeStatusEnum::Open,
            path: "/".to_string(),
            from_hash: from.to_string(),
            to_hash: to.to_string(),
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
            username: owner.to_string(),
        }
    }

    // AC①: a commit signed over the full commit bytes verifies when the
    // payload is rebuilt from the persisted columns.
    #[tokio::test]
    async fn single_commit_signed_over_full_payload_passes() {
        let (_temp, storage, checker) = setup_checker().await;
        let key = generate_key("alice <alice@example.com>");
        register_key(&storage, "alice", &key).await;

        let base = sha(1000);
        let tip = sha(1001);
        let commit = make_signed_commit(
            &key,
            1,
            &tip,
            &sha(9001),
            std::slice::from_ref(&base),
            "feat: signed commit",
            SubpacketConfig::Default,
        );
        insert_commit(&storage, &commit).await;

        checker
            .verify_cl(&base, &tip)
            .await
            .expect("chain verification must pass");
    }

    // AC①: the same scenario driven through the checker framework entry
    // points keeps the pre-chain behavior for a chain of length 1 (AC⑥).
    #[tokio::test]
    async fn single_commit_chain_compat_owner_signed_passes_via_run() {
        let (_temp, storage, checker) = setup_checker().await;
        let key = generate_key("alice <alice@example.com>");
        register_key(&storage, "alice", &key).await;

        let base = sha(1010);
        let tip = sha(1011);
        let commit = make_signed_commit(
            &key,
            1,
            &tip,
            &sha(9011),
            std::slice::from_ref(&base),
            "feat: owner signed",
            SubpacketConfig::Default,
        );
        insert_commit(&storage, &commit).await;

        let cl = cl_info(&base, &tip, "alice");
        let params = checker.build_params(&cl).await.expect("build params");
        let res = checker.run(&params).await;
        assert_eq!(res.status, ConditionResult::PASSED);
    }

    // AC⑥: an unsigned single-commit CL fails, as before the change.
    #[tokio::test]
    async fn single_commit_chain_unsigned_fails_via_run() {
        let (_temp, storage, checker) = setup_checker().await;
        let key = generate_key("alice <alice@example.com>");
        register_key(&storage, "alice", &key).await;

        let base = sha(1020);
        let tip = sha(1021);
        let commit = commit_model(
            1,
            &tip,
            &sha(9021),
            std::slice::from_ref(&base),
            Some("feat: unsigned".to_string()),
        );
        insert_commit(&storage, &commit).await;

        let cl = cl_info(&base, &tip, "alice");
        let params = checker.build_params(&cl).await.expect("build params");
        let res = checker.run(&params).await;
        assert_eq!(res.status, ConditionResult::FAILED);
        assert!(
            res.message.contains(&tip[..7]),
            "failure message must carry the failing commit sha prefix: {}",
            res.message
        );
    }

    // AC①: tampering with the persisted tree invalidates the signature.
    #[tokio::test]
    async fn tampered_tree_fails_verification() {
        let (_temp, storage, checker) = setup_checker().await;
        let key = generate_key("alice <alice@example.com>");
        register_key(&storage, "alice", &key).await;

        let base = sha(1030);
        let tip = sha(1031);
        let mut commit = make_signed_commit(
            &key,
            1,
            &tip,
            &sha(9031),
            std::slice::from_ref(&base),
            "feat: tree binding",
            SubpacketConfig::Default,
        );
        commit.tree = sha(9999);
        insert_commit(&storage, &commit).await;

        let err = checker
            .verify_cl(&base, &tip)
            .await
            .expect_err("tampered tree must fail");
        assert!(err.to_string().contains(&tip[..7]), "{err}");
    }

    // AC①: tampering with the persisted parent list invalidates the signature.
    #[tokio::test]
    async fn tampered_parent_fails_verification() {
        let (_temp, storage, checker) = setup_checker().await;
        let key = generate_key("alice <alice@example.com>");
        register_key(&storage, "alice", &key).await;

        let base = sha(1040);
        let tip = sha(1041);
        let mut commit = make_signed_commit(
            &key,
            1,
            &tip,
            &sha(9041),
            std::slice::from_ref(&base),
            "feat: parent binding",
            SubpacketConfig::Default,
        );
        commit.parents_id = serde_json::json!([sha(8888)]);
        insert_commit(&storage, &commit).await;

        let err = checker
            .verify_cl(&base, &tip)
            .await
            .expect_err("tampered parents must fail");
        assert!(err.to_string().contains(&tip[..7]), "{err}");
    }

    // AC①: tampering with the persisted author header invalidates the signature.
    #[tokio::test]
    async fn tampered_author_fails_verification() {
        let (_temp, storage, checker) = setup_checker().await;
        let key = generate_key("alice <alice@example.com>");
        register_key(&storage, "alice", &key).await;

        let base = sha(1050);
        let tip = sha(1051);
        let mut commit = make_signed_commit(
            &key,
            1,
            &tip,
            &sha(9051),
            std::slice::from_ref(&base),
            "feat: author binding",
            SubpacketConfig::Default,
        );
        commit.author = Some("author Mallory <mallory@example.com> 1750000000 +0000".to_string());
        insert_commit(&storage, &commit).await;

        let err = checker
            .verify_cl(&base, &tip)
            .await
            .expect_err("tampered author must fail");
        assert!(err.to_string().contains(&tip[..7]), "{err}");
    }

    // AC①: tampering with the persisted committer header invalidates the signature.
    #[tokio::test]
    async fn tampered_committer_fails_verification() {
        let (_temp, storage, checker) = setup_checker().await;
        let key = generate_key("alice <alice@example.com>");
        register_key(&storage, "alice", &key).await;

        let base = sha(1060);
        let tip = sha(1061);
        let mut commit = make_signed_commit(
            &key,
            1,
            &tip,
            &sha(9061),
            std::slice::from_ref(&base),
            "feat: committer binding",
            SubpacketConfig::Default,
        );
        commit.committer =
            Some("committer Mallory <mallory@example.com> 1750000001 +0000".to_string());
        insert_commit(&storage, &commit).await;

        let err = checker
            .verify_cl(&base, &tip)
            .await
            .expect_err("tampered committer must fail");
        assert!(err.to_string().contains(&tip[..7]), "{err}");
    }

    // AC②: the keyring is selected by issuer fingerprint, so a commit signed
    // by a registered user who is not the CL owner passes.
    #[tokio::test]
    async fn non_owner_registered_key_passes() {
        let (_temp, storage, checker) = setup_checker().await;
        let key = generate_key("bob <bob@example.com>");
        register_key(&storage, "bob", &key).await;

        let base = sha(1070);
        let tip = sha(1071);
        let commit = make_signed_commit(
            &key,
            1,
            &tip,
            &sha(9071),
            std::slice::from_ref(&base),
            "feat: bob signed alice's CL",
            SubpacketConfig::Default,
        );
        insert_commit(&storage, &commit).await;

        let cl = cl_info(&base, &tip, "alice");
        let params = checker.build_params(&cl).await.expect("build params");
        let res = checker.run(&params).await;
        assert_eq!(res.status, ConditionResult::PASSED);
    }

    // AC②: a signature from an unregistered key is fail-closed (no fallback).
    #[tokio::test]
    async fn unregistered_key_is_fail_closed() {
        let (_temp, storage, checker) = setup_checker().await;
        let key = generate_key("mallory <mallory@example.com>");

        let base = sha(1080);
        let tip = sha(1081);
        let commit = make_signed_commit(
            &key,
            1,
            &tip,
            &sha(9081),
            std::slice::from_ref(&base),
            "feat: unknown key",
            SubpacketConfig::Default,
        );
        insert_commit(&storage, &commit).await;

        let err = checker
            .verify_cl(&base, &tip)
            .await
            .expect_err("unregistered key must fail");
        assert!(err.to_string().contains("not registered"), "{err}");
    }

    // AC② (0 of the 0/1/many states): no issuer fingerprint subpacket in the
    // hashed area is fail-closed even though the signature itself is valid.
    #[tokio::test]
    async fn missing_issuer_fingerprint_is_fail_closed() {
        let (_temp, storage, checker) = setup_checker().await;
        let key = generate_key("alice <alice@example.com>");
        register_key(&storage, "alice", &key).await;

        let base = sha(1090);
        let tip = sha(1091);
        let hashed = vec![
            Subpacket::regular(SubpacketData::SignatureCreationTime(Timestamp::now()))
                .expect("subpacket"),
        ];
        let commit = make_signed_commit(
            &key,
            1,
            &tip,
            &sha(9091),
            std::slice::from_ref(&base),
            "feat: no issuer fingerprint",
            SubpacketConfig::UserDefined {
                hashed,
                unhashed: vec![],
            },
        );
        insert_commit(&storage, &commit).await;

        let err = checker
            .verify_cl(&base, &tip)
            .await
            .expect_err("missing issuer fingerprint must fail");
        assert!(
            err.to_string().contains("exactly one issuer fingerprint"),
            "{err}"
        );
    }

    // AC② (many): two issuer fingerprint subpackets in the hashed area are
    // fail-closed even though the signature itself is valid.
    #[tokio::test]
    async fn multiple_issuer_fingerprints_are_fail_closed() {
        let (_temp, storage, checker) = setup_checker().await;
        let key = generate_key("alice <alice@example.com>");
        register_key(&storage, "alice", &key).await;
        let other = generate_key("carol <carol@example.com>");

        let base = sha(1100);
        let tip = sha(1101);
        let hashed = vec![
            Subpacket::regular(SubpacketData::IssuerFingerprint(
                SignedPublicKey::from(key.clone()).fingerprint(),
            ))
            .expect("subpacket"),
            Subpacket::regular(SubpacketData::SignatureCreationTime(Timestamp::now()))
                .expect("subpacket"),
            Subpacket::regular(SubpacketData::IssuerFingerprint(
                SignedPublicKey::from(other.clone()).fingerprint(),
            ))
            .expect("subpacket"),
        ];
        let commit = make_signed_commit(
            &key,
            1,
            &tip,
            &sha(9101),
            std::slice::from_ref(&base),
            "feat: two issuer fingerprints",
            SubpacketConfig::UserDefined {
                hashed,
                unhashed: vec![],
            },
        );
        insert_commit(&storage, &commit).await;

        let err = checker
            .verify_cl(&base, &tip)
            .await
            .expect_err("multiple issuer fingerprints must fail");
        assert!(
            err.to_string().contains("exactly one issuer fingerprint"),
            "{err}"
        );
    }

    // AC②: an unhashed issuer fingerprint conflicting with the hashed one is
    // fail-closed.
    #[tokio::test]
    async fn conflicting_unhashed_issuer_fingerprint_is_fail_closed() {
        let (_temp, storage, checker) = setup_checker().await;
        let key = generate_key("alice <alice@example.com>");
        register_key(&storage, "alice", &key).await;
        let other = generate_key("carol <carol@example.com>");

        let base = sha(1110);
        let tip = sha(1111);
        let hashed = vec![
            Subpacket::regular(SubpacketData::IssuerFingerprint(
                SignedPublicKey::from(key.clone()).fingerprint(),
            ))
            .expect("subpacket"),
            Subpacket::regular(SubpacketData::SignatureCreationTime(Timestamp::now()))
                .expect("subpacket"),
        ];
        let unhashed = vec![
            Subpacket::regular(SubpacketData::IssuerFingerprint(
                SignedPublicKey::from(other.clone()).fingerprint(),
            ))
            .expect("subpacket"),
        ];
        let commit = make_signed_commit(
            &key,
            1,
            &tip,
            &sha(9111),
            std::slice::from_ref(&base),
            "feat: conflicting issuer fingerprints",
            SubpacketConfig::UserDefined { hashed, unhashed },
        );
        insert_commit(&storage, &commit).await;

        let err = checker
            .verify_cl(&base, &tip)
            .await
            .expect_err("conflicting unhashed issuer fingerprint must fail");
        assert!(err.to_string().contains("conflicts"), "{err}");
    }

    // AC①/AC③: every commit in a multi-commit chain is verified, and the
    // failure names the offending commit.
    #[tokio::test]
    async fn multi_commit_chain_fails_at_the_unsigned_member() {
        let (_temp, storage, checker) = setup_checker().await;
        let key = generate_key("alice <alice@example.com>");
        register_key(&storage, "alice", &key).await;

        let base = sha(1200);
        let mid = sha(1201);
        let tip = sha(1202);
        let tip_commit = make_signed_commit(
            &key,
            1,
            &tip,
            &sha(9202),
            std::slice::from_ref(&mid),
            "feat: tip signed",
            SubpacketConfig::Default,
        );
        let mid_commit = commit_model(2, &mid, &sha(9201), std::slice::from_ref(&base), None);
        insert_commit(&storage, &tip_commit).await;
        insert_commit(&storage, &mid_commit).await;

        let err = checker
            .verify_cl(&base, &tip)
            .await
            .expect_err("unsigned middle commit must fail the chain");
        assert!(err.to_string().contains(&mid[..7]), "{err}");
    }

    // AC⑤: `from_hash` not on the parent chain is fail-closed, and the error
    // explains the regular trigger path (an open CL for the same path+user
    // whose frozen base is no longer an ancestor) and the remediation.
    #[tokio::test]
    async fn from_hash_not_on_chain_is_fail_closed() {
        let (_temp, storage, checker) = setup_checker().await;
        let key = generate_key("alice <alice@example.com>");
        register_key(&storage, "alice", &key).await;

        let real_base = sha(1300);
        let tip = sha(1301);
        let commit = make_signed_commit(
            &key,
            1,
            &tip,
            &sha(9301),
            std::slice::from_ref(&real_base),
            "feat: rebased chain",
            SubpacketConfig::Default,
        );
        insert_commit(&storage, &commit).await;
        // The chain root: no parents, so the walk terminates here without
        // ever meeting the (stale) `cl_from`. It must still carry a valid
        // signature — every walked commit is verified before its parents are
        // followed, so only a signed root lets the walk reach the
        // broken-chain branch.
        let root = make_signed_commit(
            &key,
            2,
            &real_base,
            &sha(9300),
            &[],
            "feat: chain root",
            SubpacketConfig::Default,
        );
        insert_commit(&storage, &root).await;

        let stale_from = sha(7777);
        let err = checker
            .verify_cl(&stale_from, &tip)
            .await
            .expect_err("a from_hash outside the chain must fail");
        let msg = err.to_string();
        assert!(msg.contains("broken commit chain"), "{msg}");
        assert!(msg.contains("same path and user"), "{msg}");
    }

    // AC④: the cumulative range limit — 250 commits pass, 251 are rejected.
    #[tokio::test]
    async fn chain_at_limit_passes_and_over_limit_is_rejected() {
        let (_temp, storage, checker) = setup_checker().await;
        let key = generate_key("alice <alice@example.com>");
        register_key(&storage, "alice", &key).await;

        // Build one chain of MAX_CL_CHAIN_COMMITS + 1 signed commits;
        // sha(2000) is the base, sha(2001..=2251) are CL members.
        let base = sha(2000);
        for i in 1..=(MAX_CL_CHAIN_COMMITS as u64 + 1) {
            let commit_sha = sha(2000 + i);
            let parent = sha(2000 + i - 1);
            let commit = make_signed_commit(
                &key,
                i as i64,
                &commit_sha,
                &sha(9900 + i),
                std::slice::from_ref(&parent),
                &format!("feat: chain member {i}"),
                SubpacketConfig::Default,
            );
            insert_commit(&storage, &commit).await;
        }

        let at_limit_tip = sha(2000 + MAX_CL_CHAIN_COMMITS as u64);
        checker
            .verify_cl(&base, &at_limit_tip)
            .await
            .expect("a chain at the limit must pass");

        let over_limit_tip = sha(2001 + MAX_CL_CHAIN_COMMITS as u64);
        let err = checker
            .verify_cl(&base, &over_limit_tip)
            .await
            .expect_err("a chain over the limit must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains(&format!("{MAX_CL_CHAIN_COMMITS}-commit limit")),
            "{msg}"
        );
        assert!(msg.contains("squash"), "{msg}");
    }

    // Codex R1 P1-2: the real GnuPG-signed commit from the format-anchor
    // fixture, decomposed into `mega_commit` columns and verified through the
    // production path (rebuild → extract → issuer fingerprint lookup →
    // verify) — an external, non-self-referential anchor for the commit-byte
    // reconstruction.
    #[tokio::test]
    async fn real_git_signed_commit_passes_full_chain() {
        let (_temp, storage, checker) = setup_checker().await;
        storage
            .gpg_storage()
            .add_gpg_key("aidcheng".to_string(), REAL_PUBLIC_KEY.to_string())
            .await
            .expect("register the real fixture key");

        // The `content` column of a signed commit holds the gpgsig block and
        // the message (git-internal keeps the signature in `Commit.message`).
        let content = &REAL_SIGNED_COMMIT[REAL_SIGNED_COMMIT
            .find("gpgsig ")
            .expect("fixture carries a gpgsig header")..];
        let tip = sha(3001);
        let commit = mega_commit::Model {
            author: Some("author AidCheng <cn.aiden.cheng@gmail.com> 1758211153 +0100".to_string()),
            committer: Some(
                "committer AidCheng <cn.aiden.cheng@gmail.com> 1758211153 +0100".to_string(),
            ),
            ..commit_model(
                1,
                &tip,
                "52a266a58f2c028ad7de4dfd3a72fdf76b0d4e24",
                &[],
                Some(content.to_string()),
            )
        };
        insert_commit(&storage, &commit).await;

        // External anchor: the persisted columns rebuild to the exact git
        // object bytes, and stripping the gpgsig header yields the payload
        // git signed, byte-for-byte (the same expectation the format-anchor
        // test locks).
        let raw = rebuild_canonical_commit_bytes(&commit).expect("rebuild commit bytes");
        assert_eq!(raw, REAL_SIGNED_COMMIT);
        let (payload, signature) = extract_from_commit_content(&raw);
        assert!(signature.is_some());
        assert_eq!(
            payload,
            "tree 52a266a58f2c028ad7de4dfd3a72fdf76b0d4e24\n\
             author AidCheng <cn.aiden.cheng@gmail.com> 1758211153 +0100\n\
             committer AidCheng <cn.aiden.cheng@gmail.com> 1758211153 +0100\n\
             \ntest\n\nSigned-off-by: AidCheng <cn.aiden.cheng@gmail.com>\n"
        );

        // from == to (degenerate range): the tip is verified once.
        checker
            .verify_cl(&tip, &tip)
            .await
            .expect("the real GnuPG signature must verify through the full chain");
    }

    // Codex R1 P1-1: an armor block forged inside the message body must not
    // be extracted as the signature — git only honors gpgsig in the header
    // region, so this commit is unsigned and fails closed.
    #[tokio::test]
    async fn armor_block_forged_in_message_body_is_not_a_signature() {
        let (_temp, storage, checker) = setup_checker().await;
        let key = generate_key("alice <alice@example.com>");
        register_key(&storage, "alice", &key).await;

        let base = sha(1400);
        let tip = sha(1401);
        let forged = "feat: subject\n\nbody text\ngpgsig -----BEGIN PGP SIGNATURE-----\n\n forged\n -----END PGP SIGNATURE-----\n";
        let commit = commit_model(
            1,
            &tip,
            &sha(9401),
            std::slice::from_ref(&base),
            Some(forged.to_string()),
        );
        insert_commit(&storage, &commit).await;

        let err = checker
            .verify_cl(&base, &tip)
            .await
            .expect_err("forged body armor must not count as a signature");
        assert!(err.to_string().contains("no GPG signature found"), "{err}");
    }

    // P1-1 unit-level pin: body armor is invisible to the extractor and the
    // input passes through untouched.
    #[test]
    fn extract_ignores_armor_forged_in_message_body() {
        let raw = "tree 52a266a58f2c028ad7de4dfd3a72fdf76b0d4e24\n\
                   author A <a@x> 1 +0000\n\
                   committer C <c@x> 1 +0000\n\
                   \nbody\ngpgsig -----BEGIN PGP SIGNATURE-----\n -----END PGP SIGNATURE-----\n";
        let (payload, signature) = extract_from_commit_content(raw);
        assert!(signature.is_none());
        assert_eq!(payload, raw);
    }

    // P1-1 badge-path pin: the bare `content` column of a signed commit
    // starts with the gpgsig header; its header region runs to the first
    // empty line and extraction must work there exactly as before.
    #[test]
    fn extract_handles_content_column_shape_for_badge_path() {
        let content = "gpgsig -----BEGIN PGP SIGNATURE-----\n \n abcdef\n =csum\n -----END PGP SIGNATURE-----\n\nmessage subject\n\nbody line\n";
        let (payload, signature) = extract_from_commit_content(content);
        assert_eq!(
            signature.as_deref(),
            Some("-----BEGIN PGP SIGNATURE-----\n\nabcdef\n=csum\n-----END PGP SIGNATURE-----\n")
        );
        assert_eq!(payload, "message subject\n\nbody line\n");
    }

    // P2-1: from == to degenerate range — the tip is still verified (once).
    #[tokio::test]
    async fn degenerate_from_equals_to_verifies_tip() {
        let (_temp, storage, checker) = setup_checker().await;
        let key = generate_key("alice <alice@example.com>");
        register_key(&storage, "alice", &key).await;

        let tip = sha(1501);
        let signed = make_signed_commit(
            &key,
            1,
            &tip,
            &sha(9501),
            &[],
            "feat: degenerate",
            SubpacketConfig::Default,
        );
        insert_commit(&storage, &signed).await;
        checker
            .verify_cl(&tip, &tip)
            .await
            .expect("signed tip passes the degenerate range");

        let tip2 = sha(1502);
        let unsigned = commit_model(2, &tip2, &sha(9502), &[], None);
        insert_commit(&storage, &unsigned).await;
        let err = checker
            .verify_cl(&tip2, &tip2)
            .await
            .expect_err("unsigned tip must fail even when from == to");
        assert!(err.to_string().contains("no GPG signature found"), "{err}");
    }

    // P2-1: parents_id pointing at a commit missing from storage fails
    // closed as a broken chain.
    #[tokio::test]
    async fn missing_parent_object_is_fail_closed() {
        let (_temp, storage, checker) = setup_checker().await;
        let key = generate_key("alice <alice@example.com>");
        register_key(&storage, "alice", &key).await;

        let base = sha(1600);
        let tip = sha(1601);
        let ghost_parent = sha(1699);
        let commit = make_signed_commit(
            &key,
            1,
            &tip,
            &sha(9601),
            std::slice::from_ref(&ghost_parent),
            "feat: dangling parent",
            SubpacketConfig::Default,
        );
        insert_commit(&storage, &commit).await;

        let err = checker
            .verify_cl(&base, &tip)
            .await
            .expect_err("a missing parent object must fail closed");
        assert!(err.to_string().contains("broken commit chain"), "{err}");
    }

    // P2-1: a parents_id that is not a Json array fails closed.
    #[tokio::test]
    async fn corrupt_parents_id_is_fail_closed() {
        let (_temp, storage, checker) = setup_checker().await;
        let base = sha(1700);
        let tip = sha(1701);
        let mut commit = commit_model(1, &tip, &sha(9701), std::slice::from_ref(&base), None);
        commit.parents_id = serde_json::json!({"not": "an array"});
        insert_commit(&storage, &commit).await;

        let err = checker
            .verify_cl(&base, &tip)
            .await
            .expect_err("corrupt parents_id must fail closed");
        assert!(err.to_string().contains("corrupt parents_id"), "{err}");
    }

    // --- MC-11: server trust domain (ADR-MC-08 step ②) ---
    //
    // The committer header selects the keyring: the reserved server identity
    // verifies against the server keyring loaded from the vault (real MC-09
    // production path: `ServerSigningContext::sign_commit` +
    // `list_server_signing_public_keys`), every other identity against the
    // user keyring via issuer fingerprint — with no fallback between them.

    fn server_author_header() -> String {
        format!("author {SERVER_SIGNING_NAME} <{SERVER_SIGNING_EMAIL}> 1750000000 +0000")
    }

    fn server_committer_header() -> String {
        format!("committer {SERVER_SIGNING_NAME} <{SERVER_SIGNING_EMAIL}> 1750000000 +0000")
    }

    async fn test_redis() -> ::redis::aio::ConnectionManager {
        let url = std::env::var("MEGA_REDIS__URL")
            .unwrap_or_else(|_| "redis://127.0.0.1:16379".to_string());
        let client = ::redis::Client::open(url).expect("redis client");
        ::redis::aio::ConnectionManager::new(client)
            .await
            .expect("redis connection")
    }

    /// A vault-backed checker: the storage carries the MC-09 vault handle so
    /// the server keyring is reachable, and the signing context produces real
    /// server-signed commits.
    async fn setup_server_checker() -> (
        TempDir,
        Arc<Storage>,
        GpgSignatureChecker,
        ServerSigningContext,
        VaultCore,
    ) {
        use crate::jupiter::{
            migration::apply_migrations,
            storage::{base_storage::BaseStorage, vault_storage::VaultStorage},
            tests::test_db_connection,
        };

        let temp = TempDir::new().expect("temp dir");
        let conn = Arc::new(test_db_connection(temp.path()).await);
        apply_migrations(&conn, true).await.expect("migrations");
        let vault = VaultCore::config(
            VaultStorage {
                base: BaseStorage::new(conn),
            },
            temp.path().join("mc11_core_key.json"),
        )
        .await
        .expect("vault core should initialize");

        let storage = test_storage(temp.path()).await.with_vault(vault.clone());
        let storage = Arc::new(storage);
        let checker = GpgSignatureChecker {
            storage: storage.clone(),
        };
        let signing = ServerSigningContext::new(vault.clone(), test_redis().await);
        (temp, storage, checker, signing, vault)
    }

    /// Sign a synthetic commit through the real MC-09 production path and
    /// return its persistable model (committer column = server identity).
    async fn server_signed_commit(
        signing: &ServerSigningContext,
        tree: &str,
        parents: &[String],
        message: &str,
    ) -> mega_commit::Model {
        use crate::jupiter::utils::converter::IntoMegaModel;

        let key = signing.active_key().await.expect("active server key");
        let tree_id = tree.parse().expect("tree hash");
        let parent_ids = parents
            .iter()
            .map(|p| p.parse().expect("parent hash"))
            .collect();
        let unsigned = Commit::from_tree_id(tree_id, parent_ids, message);
        let signed = signing.sign_commit(&key, &unsigned).expect("sign commit");
        signed
            .into_mega_model(git_internal::internal::metadata::EntryMeta::default())
            .expect("test ID generator initialized")
    }

    // AC①/②: a server-identity commit verifies against the server keyring;
    // the server key is never registered in the user table, so this cannot
    // pass through the user path.
    #[tokio::test]
    async fn server_identity_commit_verifies_against_server_keyring() {
        let (_temp, storage, checker, signing, _vault) = setup_server_checker().await;
        let base = sha(2000);
        let model = server_signed_commit(
            &signing,
            &sha(9000),
            std::slice::from_ref(&base),
            "update-branch: rebase",
        )
        .await;
        let tip = model.commit_id.clone();
        insert_commit(&storage, &model).await;

        checker
            .verify_cl(&base, &tip)
            .await
            .expect("server-identity commit must verify via the server keyring");
    }

    // AC③: after rotation, a historical commit signed by the rotated-out
    // generation still verifies (non-revoked historical public keys stay
    // listed), and the new synthetic commit signed by the new active
    // generation verifies — both in one chain walk.
    #[tokio::test]
    async fn rotated_server_key_generations_verify_mixed_chain() {
        let (_temp, storage, checker, signing, vault) = setup_server_checker().await;
        let base = sha(2100);

        // Generation 1 signs the first synthetic commit.
        let first = server_signed_commit(
            &signing,
            &sha(9101),
            std::slice::from_ref(&base),
            "update-branch: rebase (gen-1)",
        )
        .await;
        let first_id = first.commit_id.clone();
        insert_commit(&storage, &first).await;

        // Rotate: generation 2 becomes active; generation 1 stays listed.
        let rotated = vault
            .rotate_server_signing_key(test_redis().await)
            .await
            .expect("rotate server key");
        let listed = vault
            .list_server_signing_public_keys()
            .await
            .expect("list public keys");
        assert_eq!(listed.len(), 2, "both generations stay in the keyring");
        assert!(listed.iter().any(|k| k.key_id == rotated.key_id));

        // Generation 2 signs the next synthetic commit, chained on the first.
        let second = server_signed_commit(
            &signing,
            &sha(9102),
            std::slice::from_ref(&first_id),
            "update-branch: rebase (gen-2)",
        )
        .await;
        let second_id = second.commit_id.clone();
        insert_commit(&storage, &second).await;

        checker
            .verify_cl(&base, &second_id)
            .await
            .expect("mixed-generation chain must verify after rotation");
    }

    // AC④ (negative A): the signature comes from the server key (its issuer
    // fingerprint) but the committer header is a user identity — the user
    // path resolves no registered key and fails closed; the server keyring
    // must NOT rescue it. The tampered committer also breaks the payload,
    // which is fine: the fingerprint lookup precedes (and precludes) any
    // cryptographic step.
    #[tokio::test]
    async fn server_key_with_user_identity_is_fail_closed() {
        let (_temp, storage, checker, signing, vault) = setup_server_checker().await;
        let base = sha(2200);
        let mut model = server_signed_commit(
            &signing,
            &sha(9200),
            std::slice::from_ref(&base),
            "server-signed but user-identity",
        )
        .await;
        // Precondition: the signing key really is in the server keyring.
        let active_key_id = vault
            .list_server_signing_public_keys()
            .await
            .expect("list public keys")
            .first()
            .map(|k| k.key_id.clone());
        assert!(active_key_id.is_some(), "server keyring must be non-empty");

        model.committer = Some(COMMITTER.to_string());
        let tip = model.commit_id.clone();
        insert_commit(&storage, &model).await;

        let err = checker
            .verify_cl(&base, &tip)
            .await
            .expect_err("user-identity commit signed by the server key must fail closed");
        assert!(err.to_string().contains("not registered"), "{err}");
    }

    // AC⑤ (negative B): a user key signs a commit whose committer header
    // claims the reserved server identity. The server keyring does not hold
    // the user key, and the absence of fallback is asserted by registering
    // the user key in the user table — a fallback would pass, so failure
    // proves none happened.
    #[tokio::test]
    async fn user_key_with_forged_server_identity_is_fail_closed() {
        let (_temp, storage, checker, signing, _vault) = setup_server_checker().await;
        // The server keyring exists (gen-1 initialized) but holds no user key.
        signing.active_key().await.expect("init server key");
        let user_key = generate_key("mallory <mallory@example.com>");
        register_key(&storage, "mallory", &user_key).await;

        let base = sha(2300);
        let tip = sha(2301);
        let tree = sha(9301);
        let author = server_author_header();
        let committer = server_committer_header();
        let payload =
            format!("tree {tree}\nparent {base}\n{author}\n{committer}\n\nforge: pretend server\n");
        let armor = sign_payload(&user_key, &payload, SubpacketConfig::Default);
        let mut model = commit_model(
            1,
            &tip,
            &tree,
            std::slice::from_ref(&base),
            Some(signed_content("forge: pretend server", &armor)),
        );
        model.author = Some(author);
        model.committer = Some(committer);
        insert_commit(&storage, &model).await;

        let err = checker
            .verify_cl(&base, &tip)
            .await
            .expect_err("forged server identity must fail against the server keyring");
        let msg = err.to_string();
        assert!(msg.contains("server signing key"), "{msg}");
        assert!(msg.contains("fallback is forbidden"), "{msg}");
    }

    // AC⑥: a historical server-synthesized commit without a gpgsig block
    // fails with the dedicated actionable message, not the user-facing one.
    // (No vault needed: the identity branch precedes any keyring access.)
    #[tokio::test]
    async fn unsigned_server_synthesized_commit_gets_actionable_message() {
        let (_temp, storage, checker) = setup_checker().await;
        let base = sha(2400);
        let tip = sha(2401);
        let mut model = commit_model(
            1,
            &tip,
            &sha(9401),
            std::slice::from_ref(&base),
            Some("historical synthetic commit".to_string()),
        );
        model.author = Some(server_author_header());
        model.committer = Some(server_committer_header());
        insert_commit(&storage, &model).await;

        let err = checker
            .verify_cl(&base, &tip)
            .await
            .expect_err("unsigned historical synthetic commit must fail");
        let msg = err.to_string();
        assert!(msg.contains("server-synthesized commit"), "{msg}");
        assert!(msg.contains("re-trigger the synthesis"), "{msg}");
        assert!(msg.contains("administrator"), "{msg}");
    }
}
