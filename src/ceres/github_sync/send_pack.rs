use std::{collections::BTreeSet, io::Write};

use bytes::Bytes;
use russh::ChannelMsg;

use crate::{
    ceres::{
        github_sync::ssh::SshSession,
        protocol::smart::{PktLine, try_read_pkt_line},
    },
    common::errors::MegaError,
};

pub const ZERO_OID: &str = "0000000000000000000000000000000000000000";
const MAIN_REF: &str = "refs/heads/main";
const REQUIRED_CAPABILITY: &str = "report-status";

/// Parsed receive-pack advertisement (plan-20260916 GS-08).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advertisement {
    pub main_tip: String,
    pub capabilities: BTreeSet<String>,
}

pub fn receive_pack_command(remote: &str) -> Result<String, MegaError> {
    if remote.is_empty() || remote.contains('\'') {
        return Err(MegaError::Other(
            "github_sync remote cannot be used as an SSH exec argument".to_string(),
        ));
    }
    Ok(format!("git-receive-pack '{remote}.git'"))
}

pub fn parse_advertisement(bytes: &[u8]) -> Result<Advertisement, MegaError> {
    let mut remaining = Bytes::copy_from_slice(bytes);
    let mut capabilities = BTreeSet::new();
    let mut main_tip = ZERO_OID.to_string();
    let mut saw_flush = false;
    let mut first_data = true;

    while !remaining.is_empty() {
        let line = try_read_pkt_line(&mut remaining).map_err(|err| {
            MegaError::Other(format!("github_sync receive-pack advertisement: {err}"))
        })?;
        match line {
            PktLine::Flush => {
                saw_flush = true;
                break;
            }
            PktLine::Data(payload) => {
                parse_ref_line(&payload, first_data, &mut capabilities, &mut main_tip)?;
                first_data = false;
            }
            PktLine::Delim | PktLine::ResponseEnd => {
                return Err(MegaError::Other(
                    "github_sync receive-pack advertisement used an unexpected pkt-line"
                        .to_string(),
                ));
            }
        }
    }
    if !saw_flush {
        return Err(MegaError::Other(
            "github_sync receive-pack advertisement is missing a flush-pkt".to_string(),
        ));
    }
    Ok(Advertisement {
        main_tip,
        capabilities,
    })
}

fn parse_ref_line(
    payload: &[u8],
    first_data: bool,
    capabilities: &mut BTreeSet<String>,
    main_tip: &mut String,
) -> Result<(), MegaError> {
    let payload = payload.strip_suffix(b"\n").unwrap_or(payload);
    if payload.is_empty() {
        return Ok(());
    }
    let (oid_ref, caps) = match payload.iter().position(|b| *b == 0) {
        Some(nul) => {
            if !first_data {
                return Err(MegaError::Other(
                    "github_sync receive-pack advertisement has a capability NUL after the first ref"
                        .to_string(),
                ));
            }
            (&payload[..nul], Some(&payload[nul + 1..]))
        }
        None => (payload, None),
    };
    let mut parts = oid_ref.splitn(2, |b| *b == b' ');
    let oid = parts.next().unwrap_or_default();
    let name = parts.next().unwrap_or_default();
    if oid.len() != 40 || !oid.iter().all(|b| b.is_ascii_hexdigit()) {
        return Err(MegaError::Other(
            "github_sync receive-pack advertisement has an invalid object id".to_string(),
        ));
    }
    let oid = std::str::from_utf8(oid).map_err(|_| {
        MegaError::Other("github_sync receive-pack advertisement is not UTF-8".to_string())
    })?;
    let name = std::str::from_utf8(name).map_err(|_| {
        MegaError::Other("github_sync receive-pack advertisement is not UTF-8".to_string())
    })?;
    if name == MAIN_REF {
        *main_tip = oid.to_ascii_lowercase();
    }
    if let Some(caps) = caps {
        let caps = std::str::from_utf8(caps).map_err(|_| {
            MegaError::Other("github_sync receive-pack advertisement is not UTF-8".to_string())
        })?;
        for cap in caps.split(|c: char| c.is_ascii_whitespace()) {
            if !cap.is_empty() {
                capabilities.insert(cap.to_string());
            }
        }
    }
    Ok(())
}

/// Fail closed when `report-status` is absent. Does not write to `sink`.
pub fn gate_report_status(
    advertisement: &Advertisement,
    sink: &mut impl Write,
) -> Result<(), MegaError> {
    if !advertisement.capabilities.contains(REQUIRED_CAPABILITY) {
        return Err(MegaError::Other(format!(
            "github_sync receive-pack advertisement is missing required capability {REQUIRED_CAPABILITY}; aborting before sending bytes"
        )));
    }
    let _ = sink;
    Ok(())
}

pub async fn advertise(session: &mut SshSession, remote: &str) -> Result<Advertisement, MegaError> {
    let command = receive_pack_command(remote)?;
    let mut channel = session.exec(&command).await?;
    let mut buf = Vec::new();
    loop {
        match channel.wait().await {
            Some(ChannelMsg::Data { data }) => buf.extend_from_slice(&data),
            Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
            Some(ChannelMsg::ExitStatus { .. }) => break,
            _ => continue,
        }
        if advertisement_complete(&buf) {
            break;
        }
    }
    accept_advertisement(&buf)
}

fn accept_advertisement(bytes: &[u8]) -> Result<Advertisement, MegaError> {
    let advertisement = parse_advertisement(bytes)?;
    gate_report_status(&advertisement, &mut std::io::sink())?;
    Ok(advertisement)
}

fn advertisement_complete(buf: &[u8]) -> bool {
    let mut remaining = Bytes::copy_from_slice(buf);
    loop {
        match try_read_pkt_line(&mut remaining) {
            Ok(PktLine::Flush) => return true,
            Ok(_) => continue,
            Err(_) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;

    use super::*;
    use crate::ceres::protocol::smart::add_pkt_line_string;

    fn encode(lines: &[&str]) -> Vec<u8> {
        let mut out = BytesMut::new();
        for line in lines {
            add_pkt_line_string(&mut out, (*line).to_string());
        }
        out.extend_from_slice(b"0000");
        out.to_vec()
    }

    #[test]
    fn advertisement_parsing() {
        let oid = "0123456789abcdef0123456789abcdef01234567";
        let first = format!("{oid} HEAD\0report-status side-band-64k ofs-delta\n");
        let main = format!("{oid} refs/heads/main\n");
        let parsed = parse_advertisement(&encode(&[&first, &main])).expect("parse");
        assert_eq!(parsed.main_tip, oid);
        assert!(parsed.capabilities.contains("report-status"));
        assert!(parsed.capabilities.contains("side-band-64k"));
        assert!(parsed.capabilities.contains("ofs-delta"));

        let other = format!("{oid} refs/heads/other\n");
        let missing_main = parse_advertisement(&encode(&[&first, &other])).expect("no main");
        assert_eq!(missing_main.main_tip, ZERO_OID);
        assert!(missing_main.capabilities.contains("report-status"));

        assert_eq!(
            receive_pack_command("acme/app").expect("quote"),
            "git-receive-pack 'acme/app.git'"
        );
        assert!(receive_pack_command("acme/ap'p").is_err());
    }

    #[test]
    fn minimum_capability_gate() {
        let oid = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let first = format!("{oid} refs/heads/main\0delete-refs ofs-delta\n");
        let encoded = encode(&[&first]);
        let err = accept_advertisement(&encoded).expect_err("advertise path");
        assert!(err.to_string().contains("report-status"), "{err}");
        let parsed = parse_advertisement(&encoded).expect("parse");
        let mut sink = Vec::new();
        let err = gated_then_mark(&parsed, &mut sink).expect_err("missing report-status");
        assert!(err.to_string().contains("report-status"), "{err}");
        assert!(
            sink.is_empty(),
            "capability gate wrote {} bytes",
            sink.len()
        );

        let ok_first = format!("{oid} refs/heads/main\0report-status\n");
        let ok = parse_advertisement(&encode(&[&ok_first])).expect("ok parse");
        gated_then_mark(&ok, &mut sink).expect("present");
        assert_eq!(sink.as_slice(), b"WOULD_WRITE");
    }

    fn gated_then_mark(advertisement: &Advertisement, sink: &mut Vec<u8>) -> Result<(), MegaError> {
        gate_report_status(advertisement, sink)?;
        sink.write_all(b"WOULD_WRITE").expect("mark");
        Ok(())
    }
}
