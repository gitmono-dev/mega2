use std::{collections::BTreeSet, io::Write};

use bytes::{Bytes, BytesMut};
use russh::{ChannelMsg, client};

use crate::{
    ceres::{
        github_sync::ssh::SshSession,
        protocol::smart::{PktLine, try_read_pkt_line},
    },
    common::errors::MegaError,
};

pub const ZERO_OID: &str = "0000000000000000000000000000000000000000";
/// Max bytes copied into one outbound pack write. Larger caller chunks are split.
pub const PACK_WRITE_WINDOW: usize = 16 * 1024;
const MAIN_REF: &str = "refs/heads/main";
const REQUIRED_CAPABILITY: &str = "report-status";
const SIDE_BAND_64K: &str = "side-band-64k";

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

/// Command pkt-line plus flush (no pack bytes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandPlan {
    pub bytes: Vec<u8>,
    pub requested_sideband: bool,
    pub missing_sideband: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteStats {
    pub command_bytes: usize,
    pub pack_bytes: u64,
    pub peak_chunk: usize,
    pub missing_sideband: bool,
}

/// Authenticated receive-pack channel after advertisement (plan-20260916 GS-24).
pub struct ReceivePack {
    channel: russh::Channel<client::Msg>,
    pub advertisement: Advertisement,
}

pub fn build_command_frame(
    old_oid: &str,
    new_oid: &str,
    advertisement: &Advertisement,
) -> Result<CommandPlan, MegaError> {
    let old_oid = require_oid(old_oid, "old")?;
    let new_oid = require_oid(new_oid, "new")?;
    let requested_sideband = advertisement.capabilities.contains(SIDE_BAND_64K);
    let missing_sideband = !requested_sideband;
    let mut caps = String::from(REQUIRED_CAPABILITY);
    if requested_sideband {
        caps.push(' ');
        caps.push_str(SIDE_BAND_64K);
    }
    // NUL + leading space before capabilities; no trailing LF (git receive-pack).
    let payload = format!("{old_oid} {new_oid} {MAIN_REF}\0 {caps}");
    let mut bytes = format!("{:04x}", payload.len() + 4).into_bytes();
    bytes.extend_from_slice(payload.as_bytes());
    bytes.extend_from_slice(b"0000");
    Ok(CommandPlan {
        bytes,
        requested_sideband,
        missing_sideband,
    })
}

fn require_oid(oid: &str, which: &str) -> Result<String, MegaError> {
    if oid.len() != 40 || !oid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(MegaError::Other(format!(
            "github_sync receive-pack {which} object id is invalid"
        )));
    }
    Ok(oid.to_ascii_lowercase())
}

pub async fn begin(session: &mut SshSession, remote: &str) -> Result<ReceivePack, MegaError> {
    let command = receive_pack_command(remote)?;
    let mut channel = session.exec(&command).await?;
    let buf = read_advertisement(&mut channel).await?;
    Ok(ReceivePack {
        advertisement: accept_advertisement(&buf)?,
        channel,
    })
}

pub async fn advertise(session: &mut SshSession, remote: &str) -> Result<Advertisement, MegaError> {
    Ok(begin(session, remote).await?.advertisement)
}

impl ReceivePack {
    pub async fn write_pack<I, B>(
        &mut self,
        new_oid: &str,
        chunks: I,
    ) -> Result<WriteStats, MegaError>
    where
        I: IntoIterator<Item = B>,
        B: AsRef<[u8]>,
    {
        let plan = build_command_frame(&self.advertisement.main_tip, new_oid, &self.advertisement)?;
        let command_bytes = plan.bytes.len();
        self.channel
            .data_bytes(Bytes::from(plan.bytes))
            .await
            .map_err(|err| {
                MegaError::Other(format!(
                    "github_sync receive-pack command write failed: {err}"
                ))
            })?;
        let mut pack_bytes = 0_u64;
        let mut peak_chunk = 0_usize;
        for chunk in chunks {
            for window in pack_windows(chunk.as_ref()) {
                peak_chunk = peak_chunk.max(window.len());
                pack_bytes += window.len() as u64;
                self.channel
                    .data_bytes(Bytes::copy_from_slice(window))
                    .await
                    .map_err(|err| {
                        MegaError::Other(format!(
                            "github_sync receive-pack pack write failed: {err}"
                        ))
                    })?;
            }
        }
        self.channel.eof().await.map_err(|err| {
            MegaError::Other(format!("github_sync receive-pack pack eof failed: {err}"))
        })?;
        Ok(WriteStats {
            command_bytes,
            pack_bytes,
            peak_chunk,
            missing_sideband: plan.missing_sideband,
        })
    }

    pub async fn read_report(&mut self, sideband: bool) -> Result<(), MegaError> {
        let mut buf = Vec::new();
        loop {
            match self.channel.wait().await {
                Some(ChannelMsg::Data { data }) => {
                    buf.extend_from_slice(&data);
                    match report_progress(&buf, sideband, false) {
                        ReportProgress::Ok => return Ok(()),
                        ReportProgress::Fail(err) => return Err(err),
                        ReportProgress::Pending => continue,
                    }
                }
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => {
                    return parse_report_status(&buf, sideband, true);
                }
                _ => continue,
            }
        }
    }
}

async fn read_advertisement(
    channel: &mut russh::Channel<client::Msg>,
) -> Result<Vec<u8>, MegaError> {
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
    Ok(buf)
}

fn pack_windows(chunk: &[u8]) -> impl Iterator<Item = &[u8]> {
    chunk.chunks(PACK_WRITE_WINDOW)
}

fn accept_advertisement(bytes: &[u8]) -> Result<Advertisement, MegaError> {
    let advertisement = parse_advertisement(bytes)?;
    gate_report_status(&advertisement, &mut std::io::sink())?;
    Ok(advertisement)
}

const REPORT_NG: &str = "report_ng";
const REPORT_UNPACK: &str = "report_unpack";
const REPORT_INCOMPLETE: &str = "report_incomplete";
const REPORT_DISCONNECT: &str = "report_disconnect";
const REPORT_FATAL: &str = "report_fatal";

enum ReportProgress {
    Ok,
    Fail(MegaError),
    Pending,
}

/// Judge a receive-pack report-status payload (plan-20260916 GS-15).
pub fn parse_report_status(
    bytes: &[u8],
    sideband: bool,
    connection_closed: bool,
) -> Result<(), MegaError> {
    match report_progress(bytes, sideband, connection_closed) {
        ReportProgress::Ok => Ok(()),
        ReportProgress::Fail(err) => Err(err),
        ReportProgress::Pending => Err(report_error(
            REPORT_INCOMPLETE,
            "report-status is not finished",
        )),
    }
}

fn report_progress(bytes: &[u8], sideband: bool, connection_closed: bool) -> ReportProgress {
    if sideband {
        report_progress_sideband(bytes, connection_closed)
    } else {
        judge_report_lines(bytes, connection_closed, false)
    }
}

fn report_progress_sideband(bytes: &[u8], connection_closed: bool) -> ReportProgress {
    let mut remaining = Bytes::copy_from_slice(bytes);
    let mut channel1 = BytesMut::new();
    let mut saw_outer_flush = false;

    while !remaining.is_empty() {
        let line = match try_read_pkt_line(&mut remaining) {
            Ok(line) => line,
            Err(_) => {
                return if connection_closed {
                    ReportProgress::Fail(report_error(
                        REPORT_DISCONNECT,
                        "connection closed mid pkt-line",
                    ))
                } else {
                    match judge_report_lines(&channel1, false, false) {
                        ReportProgress::Pending => ReportProgress::Pending,
                        other => other,
                    }
                };
            }
        };
        match line {
            PktLine::Flush => {
                saw_outer_flush = true;
                break;
            }
            PktLine::Data(payload) => {
                if payload.is_empty() {
                    continue;
                }
                match payload[0] {
                    3 => {
                        let msg = String::from_utf8_lossy(&payload[1..]).into_owned();
                        return ReportProgress::Fail(report_error(REPORT_FATAL, msg));
                    }
                    2 => continue,
                    1 => {
                        channel1.extend_from_slice(&payload[1..]);
                        match judge_report_lines(&channel1, false, false) {
                            ReportProgress::Ok => return ReportProgress::Ok,
                            ReportProgress::Fail(err) => return ReportProgress::Fail(err),
                            ReportProgress::Pending => {}
                        }
                    }
                    _ => {
                        return ReportProgress::Fail(report_error(
                            REPORT_INCOMPLETE,
                            "unexpected side-band channel",
                        ));
                    }
                }
            }
            PktLine::Delim | PktLine::ResponseEnd => {
                return ReportProgress::Fail(report_error(
                    REPORT_INCOMPLETE,
                    "unexpected pkt-line in report-status",
                ));
            }
        }
    }

    judge_report_lines(&channel1, connection_closed, saw_outer_flush)
}

fn judge_report_lines(
    bytes: &[u8],
    connection_closed: bool,
    multiplex_flushed: bool,
) -> ReportProgress {
    let mut remaining = Bytes::copy_from_slice(bytes);
    let mut unpack_ok = false;
    let mut ref_ok = false;
    let mut ref_ng: Option<String> = None;
    let mut saw_flush = false;

    while !remaining.is_empty() {
        let line = match try_read_pkt_line(&mut remaining) {
            Ok(line) => line,
            Err(_) => {
                return if connection_closed {
                    ReportProgress::Fail(report_error(
                        REPORT_DISCONNECT,
                        "connection closed mid pkt-line",
                    ))
                } else if multiplex_flushed {
                    ReportProgress::Fail(report_error(
                        REPORT_INCOMPLETE,
                        "side-band stream flushed mid pkt-line",
                    ))
                } else {
                    ReportProgress::Pending
                };
            }
        };
        match line {
            PktLine::Flush => {
                saw_flush = true;
                break;
            }
            PktLine::Data(payload) => {
                let text = match std::str::from_utf8(&payload) {
                    Ok(text) => text.trim_end_matches(['\n', '\r']),
                    Err(_) => {
                        return ReportProgress::Fail(report_error(
                            REPORT_INCOMPLETE,
                            "report line is not UTF-8",
                        ));
                    }
                };
                if let Some(rest) = text.strip_prefix("unpack ") {
                    if rest == "ok" {
                        unpack_ok = true;
                    } else {
                        return ReportProgress::Fail(report_error(REPORT_UNPACK, rest));
                    }
                } else if let Some(name) = text.strip_prefix("ok ") {
                    if name == MAIN_REF {
                        ref_ok = true;
                    }
                } else if let Some(rest) = text.strip_prefix("ng ") {
                    let mut parts = rest.splitn(2, ' ');
                    let name = parts.next().unwrap_or_default();
                    let reason = parts.next().unwrap_or_default();
                    if name == MAIN_REF {
                        ref_ng = Some(reason.to_string());
                    }
                }
            }
            PktLine::Delim | PktLine::ResponseEnd => {
                return ReportProgress::Fail(report_error(
                    REPORT_INCOMPLETE,
                    "unexpected pkt-line in report-status",
                ));
            }
        }
    }

    if let Some(reason) = ref_ng {
        return ReportProgress::Fail(report_error(REPORT_NG, reason));
    }
    if saw_flush && unpack_ok && ref_ok {
        return ReportProgress::Ok;
    }
    if saw_flush || multiplex_flushed {
        return ReportProgress::Fail(report_error(
            REPORT_INCOMPLETE,
            "report-status flush arrived without unpack ok and ok refs/heads/main",
        ));
    }
    if connection_closed {
        return ReportProgress::Fail(report_error(
            REPORT_DISCONNECT,
            "connection closed before report-status flush",
        ));
    }
    ReportProgress::Pending
}

fn report_error(kind: &str, detail: impl std::fmt::Display) -> MegaError {
    MegaError::Other(format!("github_sync receive-pack report {kind}: {detail}"))
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

    fn encode_sideband(channel: u8, data: &[u8]) -> Vec<u8> {
        let mut payload = Vec::with_capacity(data.len() + 1);
        payload.push(channel);
        payload.extend_from_slice(data);
        let mut out = format!("{:04x}", payload.len() + 4).into_bytes();
        out.extend_from_slice(&payload);
        out
    }

    fn wrap_report_sideband(inner: &[u8]) -> Vec<u8> {
        let mut out = encode_sideband(1, inner);
        out.extend_from_slice(b"0000");
        out
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

    #[test]
    fn command_frame_golden() {
        let old = ZERO_OID;
        let new = "0123456789ABCDEF0123456789ABCDEF01234567";
        let advertisement = Advertisement {
            main_tip: old.to_string(),
            capabilities: BTreeSet::from(["report-status".into()]),
        };
        let plan = build_command_frame(old, new, &advertisement).expect("frame");
        let payload = format!(
            "{} {} {MAIN_REF}\0 {REQUIRED_CAPABILITY}",
            old,
            new.to_ascii_lowercase()
        );
        assert!(
            !payload.as_bytes().contains(&b'\n'),
            "command payload must not end with LF"
        );
        let nul = payload
            .as_bytes()
            .iter()
            .position(|b| *b == 0)
            .expect("NUL");
        assert_eq!(
            payload.as_bytes()[nul + 1],
            b' ',
            "capability leading space"
        );
        let mut expected = format!("{:04x}", payload.len() + 4).into_bytes();
        expected.extend_from_slice(payload.as_bytes());
        expected.extend_from_slice(b"0000");
        assert_eq!(plan.bytes, expected);
        assert!(plan.bytes.ends_with(b"0000"));
        assert!(!plan.requested_sideband);
        assert!(plan.missing_sideband);
    }

    #[test]
    fn sideband_negotiation_and_downgrade() {
        let old = ZERO_OID;
        let new = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let with_sb = Advertisement {
            main_tip: old.to_string(),
            capabilities: BTreeSet::from(["report-status".into(), "side-band-64k".into()]),
        };
        let requested = build_command_frame(old, new, &with_sb).expect("with");
        assert!(requested.requested_sideband);
        assert!(!requested.missing_sideband);
        assert!(
            requested
                .bytes
                .windows(SIDE_BAND_64K.len())
                .any(|w| w == SIDE_BAND_64K.as_bytes())
        );
        assert!(requested.bytes.ends_with(b"0000"));

        let without = Advertisement {
            main_tip: old.to_string(),
            capabilities: BTreeSet::from(["report-status".into()]),
        };
        let downgraded = build_command_frame(old, new, &without).expect("without");
        assert!(!downgraded.requested_sideband);
        assert!(downgraded.missing_sideband);
        assert!(
            !downgraded
                .bytes
                .windows(SIDE_BAND_64K.len())
                .any(|w| w == SIDE_BAND_64K.as_bytes())
        );
        assert!(downgraded.bytes.ends_with(b"0000"));
    }

    #[test]
    fn pack_windows_cap_residency() {
        let fat = vec![1u8; PACK_WRITE_WINDOW * 3 + 7];
        let windows: Vec<&[u8]> = pack_windows(&fat).collect();
        assert!(windows.iter().all(|w| w.len() <= PACK_WRITE_WINDOW));
        assert_eq!(windows.iter().map(|w| w.len()).sum::<usize>(), fat.len());
        assert_eq!(windows.len(), 4);
        assert_eq!(pack_windows(&[]).count(), 0);
    }

    #[test]
    fn success_requires_full_report() {
        let full = encode(&["unpack ok\n", "ok refs/heads/main\n"]);
        parse_report_status(&full, false, false).expect("full report");

        let no_unpack = encode(&["ok refs/heads/main\n"]);
        let err = parse_report_status(&no_unpack, false, false).expect_err("unpack");
        assert!(err.to_string().contains(REPORT_INCOMPLETE), "{err}");
        assert!(!err.to_string().contains(REPORT_NG), "{err}");

        let no_ref = encode(&["unpack ok\n"]);
        let err = parse_report_status(&no_ref, false, false).expect_err("ref");
        assert!(err.to_string().contains(REPORT_INCOMPLETE), "{err}");

        let mut no_flush = BytesMut::new();
        add_pkt_line_string(&mut no_flush, "unpack ok\n".to_string());
        add_pkt_line_string(&mut no_flush, "ok refs/heads/main\n".to_string());
        let err = parse_report_status(&no_flush, false, false).expect_err("flush");
        assert!(err.to_string().contains(REPORT_INCOMPLETE), "{err}");

        let inner = encode(&["unpack ok\n", "ok refs/heads/main\n"]);
        parse_report_status(&wrap_report_sideband(&inner), true, false)
            .expect("side-band channel 1 report");
        parse_report_status(&encode_sideband(1, &inner), true, false)
            .expect("inner flush is enough without outer flush");

        let (head, tail) = inner.split_at(6);
        let mut split = encode_sideband(1, head);
        split.extend_from_slice(&encode_sideband(1, tail));
        split.extend_from_slice(b"0000");
        parse_report_status(&split, true, false).expect("reassembled channel 1");
    }

    #[test]
    fn failure_modes_are_distinguishable() {
        let ng = encode(&["unpack ok\n", "ng refs/heads/main non-fast-forward\n"]);
        let err = parse_report_status(&ng, false, false).expect_err("ng");
        let text = err.to_string();
        assert!(text.contains(REPORT_NG), "{text}");
        assert!(text.contains("non-fast-forward"), "{text}");
        assert!(!text.contains(REPORT_INCOMPLETE), "{text}");
        assert!(!text.contains(REPORT_DISCONNECT), "{text}");
        assert!(!text.contains(REPORT_FATAL), "{text}");
        assert!(!text.contains(REPORT_UNPACK), "{text}");

        let unpack_fail = encode(&["unpack index-pack failed\n"]);
        let err = parse_report_status(&unpack_fail, false, false).expect_err("unpack");
        let text = err.to_string();
        assert!(text.contains(REPORT_UNPACK), "{text}");
        assert!(text.contains("index-pack failed"), "{text}");
        assert!(!text.contains(REPORT_INCOMPLETE), "{text}");
        assert!(!text.contains(REPORT_NG), "{text}");
        assert!(!text.contains(REPORT_FATAL), "{text}");

        let incomplete = encode(&["unpack ok\n"]);
        let err = parse_report_status(&incomplete, false, false).expect_err("incomplete");
        let text = err.to_string();
        assert!(text.contains(REPORT_INCOMPLETE), "{text}");
        assert!(!text.contains(REPORT_NG), "{text}");
        assert!(!text.contains(REPORT_DISCONNECT), "{text}");

        let mut half = BytesMut::new();
        add_pkt_line_string(&mut half, "unpack ok\n".to_string());
        let err = parse_report_status(&half, false, true).expect_err("disconnect");
        let text = err.to_string();
        assert!(text.contains(REPORT_DISCONNECT), "{text}");
        assert!(!text.contains(REPORT_NG), "{text}");
        assert!(!text.contains(REPORT_INCOMPLETE), "{text}");

        let unpack_inner = encode(&["unpack index-pack failed\n"]);
        let err = parse_report_status(&wrap_report_sideband(&unpack_inner), true, false)
            .expect_err("side-band unpack");
        let text = err.to_string();
        assert!(text.contains(REPORT_UNPACK), "{text}");
        assert!(text.contains("index-pack failed"), "{text}");
        assert!(!text.contains(REPORT_INCOMPLETE), "{text}");

        let ng_inner = encode(&["unpack ok\n", "ng refs/heads/main non-fast-forward\n"]);
        let err = parse_report_status(&wrap_report_sideband(&ng_inner), true, false)
            .expect_err("side-band ng");
        let text = err.to_string();
        assert!(text.contains(REPORT_NG), "{text}");
        assert!(text.contains("non-fast-forward"), "{text}");
        assert!(!text.contains(REPORT_INCOMPLETE), "{text}");

        let incomplete_inner = encode(&["unpack ok\n"]);
        let err = parse_report_status(&wrap_report_sideband(&incomplete_inner), true, false)
            .expect_err("side-band incomplete");
        let text = err.to_string();
        assert!(text.contains(REPORT_INCOMPLETE), "{text}");
        assert!(!text.contains(REPORT_NG), "{text}");
        assert!(!text.contains(REPORT_DISCONNECT), "{text}");

        let mut half_inner = BytesMut::new();
        add_pkt_line_string(&mut half_inner, "unpack ok\n".to_string());
        let half_outer = encode_sideband(1, &half_inner);
        let err = parse_report_status(&half_outer, true, true).expect_err("side-band disconnect");
        let text = err.to_string();
        assert!(text.contains(REPORT_DISCONNECT), "{text}");
        assert!(!text.contains(REPORT_NG), "{text}");
        assert!(!text.contains(REPORT_INCOMPLETE), "{text}");

        let err = parse_report_status(&half_outer[..4], true, false)
            .expect_err("fragmented outer frame still open");
        assert!(err.to_string().contains(REPORT_INCOMPLETE), "{err}");
        let err = parse_report_status(&half_outer[..4], true, true)
            .expect_err("fragmented outer frame closed");
        assert!(err.to_string().contains(REPORT_DISCONNECT), "{err}");

        let mut fatal = BytesMut::new();
        fatal.extend_from_slice(&encode_sideband(3, b"index-pack failed"));
        fatal.extend_from_slice(&encode_sideband(
            1,
            &encode(&["unpack ok\n", "ok refs/heads/main\n"]),
        ));
        fatal.extend_from_slice(b"0000");
        let err = parse_report_status(&fatal, true, false).expect_err("fatal");
        let text = err.to_string();
        assert!(text.contains(REPORT_FATAL), "{text}");
        assert!(text.contains("index-pack failed"), "{text}");
        assert!(!text.contains(REPORT_NG), "{text}");
    }
}
