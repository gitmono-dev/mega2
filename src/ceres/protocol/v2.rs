use std::collections::HashSet;

use bytes::{BufMut, Bytes, BytesMut};
use tokio_stream::wrappers::ReceiverStream;

use crate::{
    ceres::{
        api_service::state::ProtocolApiState,
        protocol::{
            SmartSession,
            smart::{self, PktLine, add_pkt_line_string, try_read_pkt_line},
        },
    },
    common::errors::ProtocolError,
};

// Only advertise v2 capabilities that are parsed, acted on, and covered by
// tests. `server-option` is intentionally omitted: the parsed capabilities
// from `parse_v2_command` are not inspected, so advertising it would mislead
// clients into sending server options that are silently ignored.
const V2_CAPABILITIES: &[&str] = &[
    "agent=mega/0.1.0",
    "ls-refs",
    "fetch=shallow filter",
    "object-format=sha1",
];

pub fn build_v2_capability_advertisement() -> BytesMut {
    let mut buf = BytesMut::new();
    for cap in V2_CAPABILITIES {
        add_pkt_line_string(&mut buf, format!("{cap}\n"));
    }
    buf.put(Bytes::from_static(smart::PKT_LINE_END_MARKER));
    buf
}

pub async fn handle_v2_ls_refs(
    session: &SmartSession,
    state: &ProtocolApiState,
    request: &mut Bytes,
) -> Result<BytesMut, ProtocolError> {
    let mut ref_prefixes: Vec<String> = Vec::new();
    let mut symrefs = false;
    let mut peel = false;

    loop {
        let pkt_line = try_read_pkt_line(request)?;
        match pkt_line {
            PktLine::Flush => break,
            PktLine::Delim => break,
            PktLine::Data(data) => {
                let line = String::from_utf8_lossy(&data);
                let line = line.trim_end_matches('\n');
                if let Some(value) = line.strip_prefix("ref-prefix ") {
                    ref_prefixes.push(value.to_owned());
                } else if line == "symrefs" {
                    symrefs = true;
                } else if line == "peel" {
                    peel = true;
                }
            }
            PktLine::ResponseEnd => break,
        }
    }

    let repo_handler = session
        .repo_handler_with_commands(state, Vec::new())
        .await?;
    let (head_hash, git_refs) = repo_handler.refs_with_head_hash().await;

    let mut buf = BytesMut::new();

    if symrefs {
        let head_ref = if head_hash == crate::common::utils::ZERO_ID {
            "capabilities^{}"
        } else {
            "HEAD"
        };
        add_pkt_line_string(
            &mut buf,
            format!("{head_hash} {head_ref} symref-target:refs/heads/main\n"),
        );
    } else {
        let head_ref = if head_hash == crate::common::utils::ZERO_ID {
            "capabilities^{}"
        } else {
            "HEAD"
        };
        add_pkt_line_string(&mut buf, format!("{head_hash} {head_ref}\n"));
    }

    for git_ref in &git_refs {
        if !ref_prefixes.is_empty()
            && !ref_prefixes
                .iter()
                .any(|prefix| git_ref.ref_name.starts_with(prefix))
        {
            continue;
        }
        if peel {
            add_pkt_line_string(
                &mut buf,
                format!(
                    "{} {} peeled:{}\n",
                    git_ref.ref_hash, git_ref.ref_name, git_ref.ref_hash
                ),
            );
        } else {
            add_pkt_line_string(
                &mut buf,
                format!("{} {}\n", git_ref.ref_hash, git_ref.ref_name),
            );
        }
    }

    buf.put(Bytes::from_static(smart::PKT_LINE_END_MARKER));
    Ok(buf)
}

pub async fn handle_v2_fetch(
    session: &mut SmartSession,
    state: &ProtocolApiState,
    request: &mut Bytes,
) -> Result<(ReceiverStream<Vec<u8>>, BytesMut), ProtocolError> {
    let mut want: HashSet<String> = HashSet::new();
    let mut have: HashSet<String> = HashSet::new();
    let mut deepen_depth: Option<u32> = None;
    let mut deepen_relative = false;
    let mut done = false;
    let mut filter_spec: Option<String> = None;

    loop {
        let pkt_line = try_read_pkt_line(request)?;
        match pkt_line {
            PktLine::Flush => break,
            PktLine::Delim => {
                done = true;
                break;
            }
            PktLine::Data(data) => {
                let line = String::from_utf8_lossy(&data);
                let line = line.trim_end_matches('\n');
                if let Some(oid) = line.strip_prefix("want ") {
                    want.insert(oid.to_owned());
                } else if let Some(oid) = line.strip_prefix("have ") {
                    have.insert(oid.to_owned());
                } else if line == "done" {
                    done = true;
                    break;
                } else if let Some(depth_str) = line.strip_prefix("deepen ") {
                    deepen_depth = Some(depth_str.parse::<u32>().map_err(|_| {
                        ProtocolError::InvalidInput(format!("invalid deepen depth: {depth_str}"))
                    })?);
                } else if line == "deepen-relative" {
                    deepen_relative = true;
                } else if line.starts_with("deepen-since") || line.starts_with("deepen-not") {
                    return Err(ProtocolError::InvalidInput(
                        "deepen-since and deepen-not are not supported".to_owned(),
                    ));
                } else if let Some(spec) = line.strip_prefix("filter ") {
                    filter_spec = Some(spec.to_owned());
                }
            }
            PktLine::ResponseEnd => break,
        }
    }

    if !done {
        return Err(ProtocolError::InvalidInput(
            "fetch command missing done marker".to_owned(),
        ));
    }

    let repo_handler = session
        .repo_handler_with_commands(state, Vec::new())
        .await?;

    // Capability honesty: `fetch=shallow filter` is advertised globally, but
    // only MonoRepo genuinely implements shallow/filter pack generation.
    // ImportRepo's default trait implementations silently fall back to
    // full/incremental packs, which would mislead clients. Return an explicit
    // protocol error instead of silently producing a wrong pack.
    if filter_spec.is_some() && !repo_handler.supports_filtered_fetch() {
        return Err(ProtocolError::InvalidInput(
            "filter is not supported for this repository".to_owned(),
        ));
    }
    if deepen_depth.is_some() && !repo_handler.supports_shallow_fetch() {
        return Err(ProtocolError::InvalidInput(
            "shallow fetch is not supported for this repository".to_owned(),
        ));
    }
    // Combined filtered + shallow fetch is not implemented: `filtered_pack`
    // ignores `deepen` and would silently produce a non-shallow pack.
    if filter_spec.is_some() && deepen_depth.is_some() {
        return Err(ProtocolError::InvalidInput(
            "combined filter and shallow fetch is not supported".to_owned(),
        ));
    }
    // Shallow incremental fetch (deepen with non-empty have) is not
    // implemented: the incremental path ignores `deepen` and would silently
    // produce a full-depth incremental pack.
    if deepen_depth.is_some() && !have.is_empty() {
        return Err(ProtocolError::InvalidInput(
            "shallow incremental fetch is not supported".to_owned(),
        ));
    }

    let want: Vec<String> = want.into_iter().collect();
    let have: Vec<String> = have.into_iter().collect();

    let mut protocol_buf = BytesMut::new();
    let mut shallow_commits: Vec<String> = Vec::new();

    let pack_data = if let Some(ref spec) = filter_spec {
        repo_handler
            .filtered_pack(want.clone(), have.clone(), spec)
            .await
            .map_err(|e| {
                ProtocolError::InvalidInput(format!("filtered pack generation failed: {e}"))
            })?
    } else if have.is_empty() {
        if let Some(depth) = deepen_depth {
            let (stream, shallows) = repo_handler
                .shallow_pack(want, depth, deepen_relative)
                .await
                .map_err(|e| {
                    ProtocolError::InvalidInput(format!("shallow pack generation failed: {e}"))
                })?;
            shallow_commits = shallows;
            stream
        } else {
            repo_handler
                .full_pack(want)
                .await
                .map_err(|e| ProtocolError::InvalidInput(format!("pack generation failed: {e}")))?
        }
    } else {
        let mut last_common_commit = String::new();
        for hash in &have {
            if repo_handler.check_commit_exist(hash).await {
                add_pkt_line_string(&mut protocol_buf, format!("ACK {hash}\n"));
                if last_common_commit.is_empty() {
                    last_common_commit = hash.to_string();
                }
            }
        }
        if last_common_commit.is_empty() {
            add_pkt_line_string(&mut protocol_buf, String::from("NAK\n"));
            let (_, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
            return Ok((ReceiverStream::new(rx), protocol_buf));
        }
        repo_handler
            .incremental_pack(want.clone(), have)
            .await
            .map_err(|e| ProtocolError::InvalidInput(format!("pack generation failed: {e}")))?
    };

    for shallow in &shallow_commits {
        add_pkt_line_string(&mut protocol_buf, format!("shallow {shallow}\n"));
    }

    Ok((pack_data, protocol_buf))
}

pub fn parse_v2_command(request: &mut Bytes) -> Result<(String, BytesMut), ProtocolError> {
    let mut command = String::new();
    let mut capabilities = BytesMut::new();

    loop {
        let pkt_line = try_read_pkt_line(request)?;
        match pkt_line {
            PktLine::Flush => break,
            PktLine::Delim => break,
            PktLine::Data(data) => {
                let line = String::from_utf8_lossy(&data);
                let line = line.trim_end_matches('\n');
                if let Some(cmd) = line.strip_prefix("command=") {
                    command = cmd.to_owned();
                } else {
                    capabilities.put(data);
                }
            }
            PktLine::ResponseEnd => break,
        }
    }

    if command.is_empty() {
        return Err(ProtocolError::InvalidInput(
            "missing command in v2 request".to_owned(),
        ));
    }

    Ok((command, capabilities))
}

pub fn is_v2_upload_pack_request(body: &mut Bytes) -> bool {
    if body.len() < 4 {
        return false;
    }
    let peek = body.clone();
    let mut peek = peek;
    match try_read_pkt_line(&mut peek) {
        Ok(PktLine::Data(data)) => {
            let line = String::from_utf8_lossy(&data);
            line.starts_with("command=")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v2_capability_advertisement_includes_expected_capabilities() {
        let adv = build_v2_capability_advertisement();
        let adv_str = String::from_utf8_lossy(&adv);

        assert!(adv_str.contains("agent=mega/0.1.0"));
        assert!(adv_str.contains("ls-refs"));
        assert!(adv_str.contains("fetch=shallow"));
        assert!(adv_str.contains("object-format=sha1"));
        assert!(adv_str.ends_with("0000"));
        // server-option is intentionally not advertised: parsed v2 capabilities
        // are not inspected, so advertising it would mislead clients.
        assert!(
            !adv_str.contains("server-option"),
            "server-option must not be advertised: {adv_str}"
        );
    }

    #[test]
    fn parse_v2_command_extracts_ls_refs() {
        let mut buf = BytesMut::new();
        add_pkt_line_string(&mut buf, "command=ls-refs\n".to_owned());
        add_pkt_line_string(&mut buf, "agent=git/2.45.0\n".to_owned());
        buf.put(Bytes::from_static(smart::PKT_LINE_END_MARKER));

        let (command, _caps) = parse_v2_command(&mut buf.freeze()).unwrap();
        assert_eq!(command, "ls-refs");
    }

    #[test]
    fn parse_v2_command_extracts_fetch() {
        let mut buf = BytesMut::new();
        add_pkt_line_string(&mut buf, "command=fetch\n".to_owned());
        buf.put(Bytes::from_static(smart::PKT_LINE_END_MARKER));

        let (command, _caps) = parse_v2_command(&mut buf.freeze()).unwrap();
        assert_eq!(command, "fetch");
    }

    #[test]
    fn parse_v2_command_rejects_missing_command() {
        let mut buf = BytesMut::new();
        add_pkt_line_string(&mut buf, "agent=git/2.45.0\n".to_owned());
        buf.put(Bytes::from_static(smart::PKT_LINE_END_MARKER));

        let err = parse_v2_command(&mut buf.freeze()).unwrap_err();
        assert!(err.to_string().contains("missing command"));
    }
}
