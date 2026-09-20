use std::{collections::BTreeSet, future::Future, io::Write, time::Duration};

use bytes::{Bytes, BytesMut};
use russh::{ChannelMsg, client};

use crate::{
    ceres::{
        github_sync::ssh::SshSession,
        protocol::smart::{PktLine, try_read_pkt_line},
    },
    common::errors::MegaError,
    config::{DEFAULT_GITHUB_SYNC_DIAGNOSTIC_BUDGET_BYTES, GithubSyncConfig},
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
    deadlines: ReceivePackDeadlines,
    aborted: bool,
    tcp_abort: crate::ceres::github_sync::ssh::TransportAbort,
    diag_limit: usize,
}

pub fn diagnostic_budget_from_config(config: &GithubSyncConfig) -> usize {
    config.diagnostic_budget_bytes.min(usize::MAX as u64) as usize
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceivePackDeadlines {
    pub advertise: Duration,
    pub send: Duration,
    pub report: Duration,
    pub exit: Duration,
}

impl ReceivePackDeadlines {
    pub fn from_config(config: &GithubSyncConfig) -> Self {
        Self {
            advertise: Duration::from_secs(config.advertise_timeout_seconds),
            send: Duration::from_secs(config.send_timeout_seconds),
            report: Duration::from_secs(config.report_timeout_seconds),
            exit: Duration::from_secs(config.exit_timeout_seconds),
        }
    }

    pub fn defaults() -> Self {
        Self::from_config(&GithubSyncConfig::default())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeadlineStage {
    Advertise,
    Report,
    Send,
}

impl DeadlineStage {
    fn as_str(self) -> &'static str {
        match self {
            Self::Advertise => "advertise",
            Self::Report => "report",
            Self::Send => "send",
        }
    }
}

const ABORT_TIMEOUT: Duration = Duration::from_secs(1);

async fn with_deadline<T, F>(stage: DeadlineStage, limit: Duration, fut: F) -> Result<T, MegaError>
where
    F: Future<Output = Result<T, MegaError>>,
{
    match tokio::time::timeout(limit, fut).await {
        Ok(result) => result,
        Err(_) => Err(deadline_error(stage)),
    }
}

async fn abort_channel(channel: &russh::Channel<client::Msg>) {
    let _ = tokio::time::timeout(ABORT_TIMEOUT, channel.close()).await;
}

fn deadline_error(stage: DeadlineStage) -> MegaError {
    MegaError::Other(format!(
        "github_sync receive-pack {} deadline exceeded",
        stage.as_str()
    ))
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
    begin_from_config(session, remote, &GithubSyncConfig::default()).await
}

pub async fn begin_from_config(
    session: &mut SshSession,
    remote: &str,
    config: &GithubSyncConfig,
) -> Result<ReceivePack, MegaError> {
    let mut receive =
        begin_with_deadlines(session, remote, ReceivePackDeadlines::from_config(config)).await?;
    receive.diag_limit = diagnostic_budget_from_config(config);
    Ok(receive)
}

pub async fn begin_with_deadlines(
    session: &mut SshSession,
    remote: &str,
    deadlines: ReceivePackDeadlines,
) -> Result<ReceivePack, MegaError> {
    let command = receive_pack_command(remote)?;
    let mut channel = session.exec(&command).await?;
    let buf = match with_deadline(
        DeadlineStage::Advertise,
        deadlines.advertise,
        read_advertisement(&mut channel),
    )
    .await
    {
        Ok(buf) => buf,
        Err(err) => {
            session.abort_transport();
            abort_channel(&channel).await;
            return Err(err);
        }
    };
    Ok(ReceivePack {
        advertisement: accept_advertisement(&buf)?,
        channel,
        deadlines,
        aborted: false,
        tcp_abort: session.abort_transport_handle(),
        diag_limit: DEFAULT_GITHUB_SYNC_DIAGNOSTIC_BUDGET_BYTES as usize,
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
        self.ensure_live()?;
        let plan = build_command_frame(&self.advertisement.main_tip, new_oid, &self.advertisement)?;
        let command_bytes = plan.bytes.len();
        let missing_sideband = plan.missing_sideband;
        let command = plan.bytes;
        match with_deadline(DeadlineStage::Send, self.deadlines.send, async {
            self.channel
                .data_bytes(Bytes::from(command))
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
                missing_sideband,
            })
        })
        .await
        {
            Ok(stats) => Ok(stats),
            Err(err) => {
                self.abort().await;
                Err(err)
            }
        }
    }

    pub async fn read_report(&mut self, sideband: bool) -> Result<(), MegaError> {
        self.ensure_live()?;
        let mut transport = ReportTransport::with_budget(self.diag_limit);
        let report_phase = async {
            loop {
                let step = apply_channel_msg(&mut transport, sideband, self.channel.wait().await);
                match step {
                    TransportStep::Done(result) => return Ok(Some(result)),
                    TransportStep::Continue if transport.awaiting_exit() => return Ok(None),
                    TransportStep::Continue => {}
                }
            }
        };
        let after_report =
            match with_deadline(DeadlineStage::Report, self.deadlines.report, report_phase).await {
                Ok(outcome) => outcome,
                Err(err) => {
                    self.abort().await;
                    return Err(err);
                }
            };
        if let Some(result) = after_report {
            return result;
        }
        let exit_phase = async {
            loop {
                let step = apply_channel_msg(&mut transport, sideband, self.channel.wait().await);
                match step {
                    TransportStep::Done(result) => return result,
                    TransportStep::Continue => {}
                }
            }
        };
        match with_deadline(DeadlineStage::Report, self.deadlines.exit, exit_phase).await {
            Ok(()) => Ok(()),
            Err(err) => {
                self.abort().await;
                Err(err)
            }
        }
    }

    fn ensure_live(&self) -> Result<(), MegaError> {
        if self.aborted {
            return Err(MegaError::Other(
                "github_sync receive-pack channel was aborted after a deadline".to_string(),
            ));
        }
        Ok(())
    }

    async fn abort(&mut self) {
        self.aborted = true;
        self.tcp_abort.abort();
        abort_channel(&self.channel).await;
    }
}

fn apply_channel_msg(
    transport: &mut ReportTransport,
    sideband: bool,
    waited: Option<ChannelMsg>,
) -> TransportStep {
    match waited {
        Some(ChannelMsg::Data { data }) => transport.on_data(&data, sideband),
        Some(ChannelMsg::ExtendedData { data, ext }) => transport.on_stderr(ext, &data),
        Some(ChannelMsg::ExitStatus { exit_status }) => {
            transport.on_exit_status(exit_status, sideband)
        }
        Some(ChannelMsg::ExitSignal {
            signal_name,
            error_message,
            ..
        }) => {
            let signal = if error_message.is_empty() {
                format!("{signal_name:?}")
            } else {
                format!("{signal_name:?} {error_message}")
            };
            transport.on_exit_signal(signal, sideband)
        }
        Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) => transport.on_eof(sideband),
        None => transport.on_end(sideband),
        _ => TransportStep::Continue,
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
const SSH_EXIT_STATUS: &str = "ssh_exit_status";
const SSH_EXIT_SIGNAL: &str = "ssh_exit_signal";
const SSH_EXIT_MISSING: &str = "ssh_exit_missing";
const SSH_STDERR: &str = "ssh_stderr";
const SSH_EXTENDED_DATA_STDERR: u32 = 1;
const DIAGNOSTIC_TRUNCATED: &str = "truncated";
const SIDEBAND_PKT_CAP: usize = 65_540;

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
    match report_progress(
        bytes,
        sideband,
        connection_closed,
        DEFAULT_GITHUB_SYNC_DIAGNOSTIC_BUDGET_BYTES as usize,
    ) {
        ReportProgress::Ok => Ok(()),
        ReportProgress::Fail(err) => Err(err),
        ReportProgress::Pending => Err(report_error(
            REPORT_INCOMPLETE,
            "report-status is not finished",
        )),
    }
}

fn report_progress(
    bytes: &[u8],
    sideband: bool,
    connection_closed: bool,
    budget: usize,
) -> ReportProgress {
    if sideband {
        report_progress_sideband(bytes, connection_closed, budget)
    } else {
        judge_report_lines(bytes, connection_closed, false)
    }
}

fn report_progress_sideband(
    bytes: &[u8],
    connection_closed: bool,
    budget: usize,
) -> ReportProgress {
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
                        let msg = budget_utf8(&payload[1..], budget);
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

fn valid_utf8_prefix_len(bytes: &[u8]) -> usize {
    match std::str::from_utf8(bytes) {
        Ok(_) => bytes.len(),
        Err(err) => err.valid_up_to(),
    }
}

fn budget_utf8(data: &[u8], limit: usize) -> String {
    let mut sink = DiagnosticSink::new(limit);
    sink.push_important(data);
    sink.render()
}

struct DiagnosticSink {
    limit: usize,
    progress: Vec<u8>,
    important: Vec<u8>,
    truncated: bool,
    saw_stderr: bool,
}

impl DiagnosticSink {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            progress: Vec::new(),
            important: Vec::new(),
            truncated: false,
            saw_stderr: false,
        }
    }

    fn keep(&self) -> usize {
        self.limit
            .saturating_sub(DIAGNOSTIC_TRUNCATED.len().saturating_add(1))
    }

    fn push_stderr(&mut self, data: &[u8]) {
        self.saw_stderr = true;
        self.push_important(data);
    }

    fn push_important(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        self.important.extend_from_slice(data);
        self.fit();
    }

    fn push(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        if self.truncated && !self.important.is_empty() {
            return;
        }
        self.progress.extend_from_slice(data);
        self.fit();
    }

    fn fit(&mut self) {
        let keep = self.keep();
        if self.important.len() > keep {
            self.important.truncate(keep);
            self.progress.clear();
            self.truncated = true;
            return;
        }
        let rest = keep.saturating_sub(self.important.len());
        if self.progress.len() > rest {
            self.progress.truncate(rest);
            self.truncated = true;
        }
    }

    fn render(&self) -> String {
        let mut raw = Vec::with_capacity(self.important.len() + self.progress.len());
        raw.extend_from_slice(&self.important);
        raw.extend_from_slice(&self.progress);
        let mut body = String::from_utf8_lossy(&raw).into_owned();
        let mut truncated = self.truncated;
        let keep = self.keep();
        if body.len() > self.limit {
            truncated = true;
        }
        if truncated && body.len() > keep {
            let end = valid_utf8_prefix_len(&body.as_bytes()[..keep.min(body.len())]);
            body.truncate(end);
        }
        if !truncated {
            return body;
        }
        let rendered = if body.is_empty() {
            DIAGNOSTIC_TRUNCATED.to_string()
        } else {
            format!("{body} {DIAGNOSTIC_TRUNCATED}")
        };
        if rendered.len() <= self.limit {
            rendered
        } else {
            DIAGNOSTIC_TRUNCATED.to_string()
        }
    }
}

impl Default for DiagnosticSink {
    fn default() -> Self {
        Self::new(DEFAULT_GITHUB_SYNC_DIAGNOSTIC_BUDGET_BYTES as usize)
    }
}

fn collect_stderr(diag: &mut DiagnosticSink, ext: u32, data: &[u8]) {
    if ext == SSH_EXTENDED_DATA_STDERR {
        diag.push_stderr(data);
    }
}

struct ReportTransport {
    buf: Vec<u8>,
    diag: DiagnosticSink,
    sideband_tail: Vec<u8>,
    stream_closed: bool,
    report_ok: bool,
    multiplex_flushed: bool,
    saw_fatal: bool,
    unexpected_channel: bool,
    exit_status: Option<u32>,
    exit_signal: Option<String>,
}

impl ReportTransport {
    fn with_budget(limit: usize) -> Self {
        Self {
            buf: Vec::new(),
            diag: DiagnosticSink::new(limit),
            sideband_tail: Vec::new(),
            stream_closed: false,
            report_ok: false,
            multiplex_flushed: false,
            saw_fatal: false,
            unexpected_channel: false,
            exit_status: None,
            exit_signal: None,
        }
    }
}

impl Default for ReportTransport {
    fn default() -> Self {
        Self::with_budget(DEFAULT_GITHUB_SYNC_DIAGNOSTIC_BUDGET_BYTES as usize)
    }
}

enum TransportStep {
    Continue,
    Done(Result<(), MegaError>),
}

impl ReportTransport {
    fn ingest_sideband(&mut self, data: &[u8]) {
        self.sideband_tail.extend_from_slice(data);
        if self.sideband_tail.len() > SIDEBAND_PKT_CAP {
            self.diag.push(&self.sideband_tail);
            self.sideband_tail.clear();
            return;
        }
        let mut remaining = Bytes::copy_from_slice(&self.sideband_tail);
        let start = remaining.len();
        while !remaining.is_empty() {
            match try_read_pkt_line(&mut remaining) {
                Ok(PktLine::Flush) => {
                    self.multiplex_flushed = true;
                }
                Ok(PktLine::Data(payload)) if !payload.is_empty() => match payload[0] {
                    1 => self.buf.extend_from_slice(&payload[1..]),
                    2 => self.diag.push(&payload[1..]),
                    3 => {
                        self.diag.push_important(&payload[1..]);
                        self.saw_fatal = true;
                    }
                    _ => self.unexpected_channel = true,
                },
                Ok(PktLine::Data(_)) => {}
                Ok(PktLine::Delim | PktLine::ResponseEnd) => {
                    self.unexpected_channel = true;
                }
                Err(_) => break,
            }
        }
        let consumed = start.saturating_sub(remaining.len());
        self.sideband_tail.drain(..consumed);
    }

    fn on_data(&mut self, data: &[u8], sideband: bool) -> TransportStep {
        if sideband {
            self.ingest_sideband(data);
            if self.saw_fatal {
                return self.done_with(Err(report_error(REPORT_FATAL, self.diag.render())));
            }
            if self.unexpected_channel {
                return self.done_with(Err(report_error(
                    REPORT_INCOMPLETE,
                    "unexpected side-band channel",
                )));
            }
        } else {
            self.buf.extend_from_slice(data);
        }
        match self.judge_buf(false) {
            ReportProgress::Fail(err) => self.done_with(Err(err)),
            ReportProgress::Ok => {
                self.report_ok = true;
                self.maybe_done(sideband)
            }
            ReportProgress::Pending => self.maybe_done(sideband),
        }
    }

    fn on_stderr(&mut self, ext: u32, data: &[u8]) -> TransportStep {
        collect_stderr(&mut self.diag, ext, data);
        TransportStep::Continue
    }

    fn on_exit_status(&mut self, code: u32, sideband: bool) -> TransportStep {
        self.exit_status = Some(code);
        self.maybe_done(sideband)
    }

    fn on_exit_signal(&mut self, signal: String, sideband: bool) -> TransportStep {
        self.exit_signal = Some(signal);
        self.maybe_done(sideband)
    }

    fn on_eof(&mut self, sideband: bool) -> TransportStep {
        self.stream_closed = true;
        self.maybe_done(sideband)
    }

    fn on_end(&mut self, sideband: bool) -> TransportStep {
        self.stream_closed = true;
        self.done_with(self.parse_report(sideband, true))
    }

    fn awaiting_exit(&self) -> bool {
        self.report_ok || self.stream_closed
    }

    fn maybe_done(&self, sideband: bool) -> TransportStep {
        if self.stream_closed && (self.exit_status.is_some() || self.exit_signal.is_some()) {
            self.done_with(self.parse_report(sideband, true))
        } else {
            TransportStep::Continue
        }
    }

    fn judge_buf(&self, connection_closed: bool) -> ReportProgress {
        if self.saw_fatal {
            return ReportProgress::Fail(report_error(REPORT_FATAL, self.diag.render()));
        }
        if self.unexpected_channel {
            return ReportProgress::Fail(report_error(
                REPORT_INCOMPLETE,
                "unexpected side-band channel",
            ));
        }
        judge_report_lines(&self.buf, connection_closed, self.multiplex_flushed)
    }

    fn parse_report(&self, _sideband: bool, connection_closed: bool) -> Result<(), MegaError> {
        match self.judge_buf(connection_closed) {
            ReportProgress::Ok => Ok(()),
            ReportProgress::Fail(err) => Err(err),
            ReportProgress::Pending => Err(report_error(
                REPORT_INCOMPLETE,
                "report-status is not finished",
            )),
        }
    }

    fn done_with(&self, report: Result<(), MegaError>) -> TransportStep {
        TransportStep::Done(judge_transport_diag(
            report,
            &self.diag,
            self.exit_status,
            self.exit_signal.as_deref(),
        ))
    }
}

fn judge_transport(
    report: Result<(), MegaError>,
    stderr: &[u8],
    exit_status: Option<u32>,
    exit_signal: Option<&str>,
) -> Result<(), MegaError> {
    let mut diag = DiagnosticSink::default();
    if !stderr.is_empty() {
        diag.push_stderr(stderr);
    }
    judge_transport_diag(report, &diag, exit_status, exit_signal)
}

fn judge_transport_diag(
    report: Result<(), MegaError>,
    diag: &DiagnosticSink,
    exit_status: Option<u32>,
    exit_signal: Option<&str>,
) -> Result<(), MegaError> {
    if let Some(signal) = exit_signal {
        return Err(ssh_error(SSH_EXIT_SIGNAL, format_detail(signal, diag)));
    }
    if let Some(code) = exit_status
        && code != 0
    {
        return Err(ssh_error(SSH_EXIT_STATUS, format_detail(code, diag)));
    }
    match report {
        Err(err) => Err(attach_diag(err, diag)),
        Ok(()) => {
            if exit_status == Some(0) {
                Ok(())
            } else {
                Err(ssh_error(
                    SSH_EXIT_MISSING,
                    format_detail("EOF/close without exit-status", diag),
                ))
            }
        }
    }
}

fn format_detail(detail: impl std::fmt::Display, diag: &DiagnosticSink) -> String {
    let rendered = diag.render();
    if rendered.is_empty() {
        detail.to_string()
    } else if diag.saw_stderr {
        format!("{detail}; {SSH_STDERR} {rendered}")
    } else {
        format!("{detail}; {rendered}")
    }
}

fn attach_diag(err: MegaError, diag: &DiagnosticSink) -> MegaError {
    let rendered = diag.render();
    if rendered.is_empty() {
        err
    } else {
        let text = err.to_string();
        if text.contains(&rendered) {
            err
        } else {
            MegaError::Other(format_detail(text, diag))
        }
    }
}

fn ssh_error(kind: &str, detail: impl std::fmt::Display) -> MegaError {
    MegaError::Other(format!("github_sync receive-pack {kind}: {detail}"))
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

    enum TestEv<'a> {
        Data(&'a [u8]),
        Stderr(&'a [u8]),
        Exit(u32),
        Signal(&'a str),
        Eof,
        End,
    }

    fn drive(sideband: bool, events: &[TestEv<'_>]) -> Result<(), MegaError> {
        drive_with_budget(
            sideband,
            DEFAULT_GITHUB_SYNC_DIAGNOSTIC_BUDGET_BYTES as usize,
            events,
        )
    }

    fn drive_with_budget(
        sideband: bool,
        budget: usize,
        events: &[TestEv<'_>],
    ) -> Result<(), MegaError> {
        let mut transport = ReportTransport::with_budget(budget);
        for event in events {
            let step = match event {
                TestEv::Data(data) => transport.on_data(data, sideband),
                TestEv::Stderr(data) => transport.on_stderr(SSH_EXTENDED_DATA_STDERR, data),
                TestEv::Exit(code) => transport.on_exit_status(*code, sideband),
                TestEv::Signal(signal) => transport.on_exit_signal((*signal).to_string(), sideband),
                TestEv::Eof => transport.on_eof(sideband),
                TestEv::End => transport.on_end(sideband),
            };
            if let TransportStep::Done(result) = step {
                return result;
            }
        }
        panic!("transport still pending");
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

    #[test]
    fn remote_exit_overrides_report() {
        let full = encode(&["unpack ok\n", "ok refs/heads/main\n"]);
        let report = parse_report_status(&full, false, false);
        report.as_ref().expect("report");

        let err = judge_transport(report, b"", Some(1), None).expect_err("nonzero");
        let text = err.to_string();
        assert!(text.contains(SSH_EXIT_STATUS), "{text}");
        assert!(text.contains('1'), "{text}");
        assert!(!text.contains(REPORT_INCOMPLETE), "{text}");
        assert!(!text.contains(SSH_EXIT_SIGNAL), "{text}");

        let report = parse_report_status(&full, false, false);
        let err = judge_transport(report, b"", None, Some("TERM")).expect_err("signal");
        let text = err.to_string();
        assert!(text.contains(SSH_EXIT_SIGNAL), "{text}");
        assert!(text.contains("TERM"), "{text}");
        assert!(!text.contains(SSH_EXIT_STATUS), "{text}");
        assert!(!text.contains(REPORT_NG), "{text}");

        let (head, tail) = full.split_at(6);
        let err = drive(
            false,
            &[
                TestEv::Data(head),
                TestEv::Exit(1),
                TestEv::Data(tail),
                TestEv::Eof,
            ],
        )
        .expect_err("nonzero after trailing report");
        assert!(err.to_string().contains(SSH_EXIT_STATUS), "{err}");
    }

    #[test]
    fn eof_without_exit_status_is_not_success() {
        let full = encode(&["unpack ok\n", "ok refs/heads/main\n"]);
        let report = parse_report_status(&full, false, true);
        report.as_ref().expect("report");

        let err = judge_transport(report, b"", None, None).expect_err("missing exit");
        let text = err.to_string();
        assert!(text.contains(SSH_EXIT_MISSING), "{text}");
        assert!(!text.contains(REPORT_INCOMPLETE), "{text}");
        assert!(!text.contains(SSH_EXIT_STATUS), "{text}");

        let report = parse_report_status(&full, false, true);
        judge_transport(report, b"", Some(0), None).expect("exit 0 plus report");

        let err = drive(false, &[TestEv::Data(&full), TestEv::Eof, TestEv::End])
            .expect_err("eof then end");
        assert!(err.to_string().contains(SSH_EXIT_MISSING), "{err}");

        let (head, tail) = full.split_at(6);
        drive(
            false,
            &[
                TestEv::Data(head),
                TestEv::Exit(0),
                TestEv::Data(tail),
                TestEv::Eof,
            ],
        )
        .expect("exit 0 then remaining report then eof");
    }

    #[test]
    fn stderr_is_collected() {
        let mut stderr = DiagnosticSink::default();
        collect_stderr(
            &mut stderr,
            SSH_EXTENDED_DATA_STDERR,
            b"remote: hook denied\n",
        );
        collect_stderr(&mut stderr, 0, b"ignored stdout-like");
        assert_eq!(stderr.render(), "remote: hook denied\n");

        let full = encode(&["unpack ok\n", "ok refs/heads/main\n"]);
        let report = parse_report_status(&full, false, false);
        let err =
            judge_transport(report, b"remote: hook denied\n", Some(128), None).expect_err("stderr");
        let text = err.to_string();
        assert!(text.contains(SSH_STDERR), "{text}");
        assert!(text.contains("hook denied"), "{text}");
        assert!(text.contains(SSH_EXIT_STATUS), "{text}");

        let full = encode(&["unpack ok\n", "ok refs/heads/main\n"]);
        let err = drive(
            false,
            &[
                TestEv::Stderr(b"remote: hook denied\n"),
                TestEv::Data(&full),
                TestEv::Exit(128),
                TestEv::Eof,
            ],
        )
        .expect_err("driven stderr");
        let text = err.to_string();
        assert!(text.contains(SSH_STDERR), "{text}");
        assert!(text.contains("hook denied"), "{text}");

        let fatal = encode_sideband(3, b"index-pack failed");
        let err = drive(true, &[TestEv::Data(&fatal)]).expect_err("fatal is immediate");
        assert!(err.to_string().contains(REPORT_FATAL), "{err}");
    }

    #[test]
    fn diagnostic_budget_truncates() {
        const BUDGET: usize = 21;
        let oversize = "aaaaaaaaaa你好more-than-budget";
        assert!(oversize.len() > BUDGET);
        let keep = BUDGET.saturating_sub(DIAGNOSTIC_TRUNCATED.len() + 1);
        assert!(
            !oversize.is_char_boundary(keep),
            "content cap {keep} should land inside 你"
        );

        let mut sink = DiagnosticSink::new(BUDGET);
        sink.push(oversize.as_bytes());
        let rendered = sink.render();
        assert!(rendered.contains(DIAGNOSTIC_TRUNCATED), "{rendered}");
        assert!(rendered.is_char_boundary(rendered.len()), "{rendered}");
        assert!(std::str::from_utf8(rendered.as_bytes()).is_ok());
        assert!(
            rendered.len() <= BUDGET,
            "rendered {} exceeds {BUDGET}: {rendered}",
            rendered.len()
        );
        assert!(!rendered.contains("more-than-budget"), "{rendered}");

        let full = encode(&["unpack ok\n", "ok refs/heads/main\n"]);
        let err = drive_with_budget(
            false,
            BUDGET,
            &[
                TestEv::Stderr(oversize.as_bytes()),
                TestEv::Data(&full),
                TestEv::Exit(128),
                TestEv::Eof,
            ],
        )
        .expect_err("stderr budget");
        let text = err.to_string();
        assert!(text.contains(DIAGNOSTIC_TRUNCATED), "{text}");
        assert!(text.contains(SSH_STDERR), "{text}");
        assert!(!text.contains("more-than-budget"), "{text}");

        let progress = encode_sideband(2, oversize.as_bytes());
        let wrapped = wrap_report_sideband(&full);
        let err = drive_with_budget(
            true,
            BUDGET,
            &[
                TestEv::Data(&progress),
                TestEv::Data(&wrapped),
                TestEv::Exit(128),
                TestEv::Eof,
            ],
        )
        .expect_err("channel 2 budget");
        let text = err.to_string();
        assert!(text.contains(DIAGNOSTIC_TRUNCATED), "{text}");
        assert!(!text.contains("more-than-budget"), "{text}");

        let fatal = encode_sideband(3, oversize.as_bytes());
        let err = drive_with_budget(true, BUDGET, &[TestEv::Data(&fatal)]).expect_err("channel 3");
        let text = err.to_string();
        assert!(text.contains(REPORT_FATAL), "{text}");
        assert!(text.contains(DIAGNOSTIC_TRUNCATED), "{text}");
        assert!(!text.contains("more-than-budget"), "{text}");
        assert!(std::str::from_utf8(text.as_bytes()).is_ok());

        let min = DIAGNOSTIC_TRUNCATED.len();
        let mut min_sink = DiagnosticSink::new(min);
        min_sink.push(b"abcdefghijklmnop");
        assert_eq!(min_sink.render(), DIAGNOSTIC_TRUNCATED);

        let mut transport = ReportTransport::with_budget(BUDGET);
        let progress = encode_sideband(2, &[b'x'; 256]);
        for _ in 0..40 {
            let _ = transport.on_data(&progress, true);
        }
        assert!(
            transport.buf.is_empty(),
            "channel 2 must not accumulate in the report buffer: {}",
            transport.buf.len()
        );
        assert!(transport.diag.render().contains(DIAGNOSTIC_TRUNCATED));

        let mut lossy = DiagnosticSink::new(64);
        lossy.push_stderr(b"hook \xff denied");
        let rendered = lossy.render();
        assert!(rendered.contains("hook"), "{rendered}");
        assert!(rendered.contains("denied"), "{rendered}");
        assert!(rendered.contains('\u{FFFD}'), "{rendered}");
        assert!(std::str::from_utf8(rendered.as_bytes()).is_ok());
        assert!(!rendered.contains(DIAGNOSTIC_TRUNCATED), "{rendered}");

        let mut mixed = DiagnosticSink::new(64);
        let mut blob = b"remote: hook \xff declined: ".to_vec();
        blob.extend(std::iter::repeat_n(0x80, 80));
        blob.extend_from_slice(b" refs/heads/main");
        mixed.push_stderr(&blob);
        let rendered = mixed.render();
        assert!(rendered.contains("hook"), "{rendered}");
        assert!(rendered.contains("declined"), "{rendered}");
        assert!(rendered.contains('\u{FFFD}'), "{rendered}");
        assert!(rendered.contains(DIAGNOSTIC_TRUNCATED), "{rendered}");
        assert!(std::str::from_utf8(rendered.as_bytes()).is_ok());

        let mut transport = ReportTransport::with_budget(64);
        let progress = encode_sideband(2, &[b'x'; 80]);
        let _ = transport.on_data(&progress, true);
        let fatal = encode_sideband(3, b"fatal: refs/heads/main rejected by policy");
        let TransportStep::Done(Err(err)) = transport.on_data(&fatal, true) else {
            panic!("expected channel 3 fatal");
        };
        let text = err.to_string();
        assert!(text.contains(REPORT_FATAL), "{text}");
        assert!(text.contains("rejected by policy"), "{text}");
    }

    const OLD_OID: &str = "0123456789abcdef0123456789abcdef01234567";
    const NEW_OID: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const SHORT: Duration = Duration::from_millis(400);
    const LONG: Duration = Duration::from_secs(30);

    fn deadlines(
        advertise: Duration,
        send: Duration,
        report: Duration,
        exit: Duration,
    ) -> ReceivePackDeadlines {
        ReceivePackDeadlines {
            advertise,
            send,
            report,
            exit,
        }
    }

    struct ClearHold;
    impl Drop for ClearHold {
        fn drop(&mut self) {
            crate::ceres::github_sync::key::clear_held_for_it();
        }
    }

    #[derive(Clone)]
    struct LoopServer {
        user: String,
        public: russh::keys::PublicKey,
        expected_exec: String,
        advertisement: Option<Vec<u8>>,
        report: Option<Vec<u8>>,
        send_stall: Option<SendStall>,
        inbound: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    }

    #[derive(Clone)]
    struct SendStall {
        drain: std::sync::Arc<std::sync::atomic::AtomicBool>,
        wake: std::sync::Arc<tokio::sync::Notify>,
    }

    impl SendStall {
        fn new() -> Self {
            Self {
                drain: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                wake: std::sync::Arc::new(tokio::sync::Notify::new()),
            }
        }

        fn release(&self) {
            self.drain.store(true, std::sync::atomic::Ordering::SeqCst);
            self.wake.notify_waiters();
        }
    }

    impl russh::server::Server for LoopServer {
        type Handler = Self;

        fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self {
            self.clone()
        }
    }

    impl russh::server::Handler for LoopServer {
        type Error = russh::Error;

        async fn auth_publickey(
            &mut self,
            user: &str,
            public_key: &russh::keys::PublicKey,
        ) -> Result<russh::server::Auth, Self::Error> {
            if user == self.user
                && public_key.algorithm() == self.public.algorithm()
                && public_key.key_data() == self.public.key_data()
            {
                Ok(russh::server::Auth::Accept)
            } else {
                Ok(russh::server::Auth::reject())
            }
        }

        async fn channel_open_session(
            &mut self,
            _channel: russh::Channel<russh::server::Msg>,
            reply: russh::server::ChannelOpenHandle,
            _session: &mut russh::server::Session,
        ) -> Result<(), Self::Error> {
            reply.accept().await;
            Ok(())
        }

        async fn exec_request(
            &mut self,
            channel: russh::ChannelId,
            data: &[u8],
            session: &mut russh::server::Session,
        ) -> Result<(), Self::Error> {
            if data == self.expected_exec.as_bytes() {
                session.channel_success(channel)?;
                if let Some(advertisement) = &self.advertisement {
                    session.data(channel, advertisement.clone())?;
                }
            } else {
                session.channel_failure(channel)?;
            }
            Ok(())
        }

        async fn data(
            &mut self,
            _channel: russh::ChannelId,
            data: &[u8],
            _session: &mut russh::server::Session,
        ) -> Result<(), Self::Error> {
            self.inbound
                .lock()
                .expect("inbound")
                .extend_from_slice(data);
            if let Some(stall) = &self.send_stall
                && !stall.drain.load(std::sync::atomic::Ordering::SeqCst)
            {
                stall.wake.notified().await;
            }
            Ok(())
        }

        async fn channel_eof(
            &mut self,
            channel: russh::ChannelId,
            session: &mut russh::server::Session,
        ) -> Result<(), Self::Error> {
            if let Some(report) = &self.report {
                session.data(channel, report.clone())?;
            }
            Ok(())
        }
    }

    fn advertise_bytes() -> Vec<u8> {
        let first = format!("{OLD_OID} refs/heads/main\0report-status\n");
        encode(&[&first])
    }

    fn install_client() -> russh::keys::PublicKey {
        let client_key =
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .expect("client key");
        let openssh = client_key
            .to_openssh(ed25519_dalek::pkcs8::spki::der::pem::LineEnding::LF)
            .expect("encode hold");
        crate::ceres::github_sync::key::install_openssh_for_it(openssh.as_str()).expect("hold");
        client_key.public_key().clone()
    }

    async fn connect_loopback(
        server: LoopServer,
        host_key: russh::keys::PrivateKey,
        window_size: u32,
    ) -> crate::ceres::github_sync::ssh::SshSession {
        let host_pub = host_key
            .public_key()
            .to_openssh()
            .expect("host public")
            .trim()
            .to_string();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let config = std::sync::Arc::new(russh::server::Config {
            auth_rejection_time: Duration::from_millis(0),
            auth_rejection_time_initial: Some(Duration::from_millis(0)),
            keys: vec![host_key],
            inactivity_timeout: Some(Duration::from_secs(5)),
            window_size,
            maximum_packet_size: window_size,
            ..Default::default()
        });
        let mut server = server;
        tokio::spawn(async move {
            let _ = russh::server::Server::run_on_socket(&mut server, config, &listener).await;
        });
        crate::ceres::github_sync::ssh::connect(&crate::config::GithubSyncConfig {
            ssh_host: addr.to_string(),
            ssh_user: "git".to_string(),
            ssh_host_key: host_pub,
            ..Default::default()
        })
        .await
        .expect("loopback auth")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn four_deadlines() {
        let _serial = crate::ceres::github_sync::key::HOLDER_TEST_SERIAL
            .lock()
            .await;
        let _clear = ClearHold;
        let exec = receive_pack_command("acme/app").expect("exec");

        let client_pub = install_client();
        let inbound = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut session = connect_loopback(
            LoopServer {
                user: "git".to_string(),
                public: client_pub.clone(),
                expected_exec: exec.clone(),
                advertisement: None,
                report: None,
                send_stall: None,
                inbound: inbound.clone(),
            },
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .expect("host"),
            2 * 1024 * 1024,
        )
        .await;
        let err = match begin_with_deadlines(
            &mut session,
            "acme/app",
            deadlines(SHORT, LONG, LONG, LONG),
        )
        .await
        {
            Ok(_) => panic!("advertise should time out"),
            Err(err) => err,
        };
        let text = err.to_string();
        assert!(text.contains("advertise"), "{text}");
        assert!(text.contains("deadline"), "{text}");
        assert_session_dead(&mut session).await;
        drop(session);

        let inbound = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let stall = SendStall::new();
        let mut session = connect_loopback(
            LoopServer {
                user: "git".to_string(),
                public: client_pub.clone(),
                expected_exec: exec.clone(),
                advertisement: Some(advertise_bytes()),
                report: None,
                send_stall: Some(stall.clone()),
                inbound: inbound.clone(),
            },
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .expect("host"),
            128,
        )
        .await;
        let mut receive =
            begin_with_deadlines(&mut session, "acme/app", deadlines(LONG, SHORT, LONG, LONG))
                .await
                .expect("begin send");
        let err = receive
            .write_pack(NEW_OID, std::iter::once([0_u8; 64].as_slice()))
            .await
            .expect_err("send");
        let text = err.to_string();
        assert!(text.contains("send"), "{text}");
        assert!(text.contains("deadline"), "{text}");
        stall.release();
        drop(receive);
        assert_session_dead(&mut session).await;
        drop(session);

        let inbound = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut session = connect_loopback(
            LoopServer {
                user: "git".to_string(),
                public: client_pub.clone(),
                expected_exec: exec.clone(),
                advertisement: Some(advertise_bytes()),
                report: None,
                send_stall: None,
                inbound: inbound.clone(),
            },
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .expect("host"),
            2 * 1024 * 1024,
        )
        .await;
        let mut receive =
            begin_with_deadlines(&mut session, "acme/app", deadlines(LONG, LONG, SHORT, LONG))
                .await
                .expect("begin report");
        receive
            .write_pack(NEW_OID, std::iter::once([0_u8; 8].as_slice()))
            .await
            .expect("pack");
        let err = receive.read_report(false).await.expect_err("report");
        let text = err.to_string();
        assert!(text.contains("report"), "{text}");
        assert!(text.contains("deadline"), "{text}");
        drop(receive);
        assert_session_dead(&mut session).await;
        drop(session);

        let inbound = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut session = connect_loopback(
            LoopServer {
                user: "git".to_string(),
                public: client_pub,
                expected_exec: exec,
                advertisement: Some(advertise_bytes()),
                report: Some(encode(&["unpack ok\n", "ok refs/heads/main\n"])),
                send_stall: None,
                inbound,
            },
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .expect("host"),
            2 * 1024 * 1024,
        )
        .await;
        let mut receive =
            begin_with_deadlines(&mut session, "acme/app", deadlines(LONG, LONG, LONG, SHORT))
                .await
                .expect("begin exit");
        receive
            .write_pack(NEW_OID, std::iter::once([0_u8; 8].as_slice()))
            .await
            .expect("pack");
        let err = receive.read_report(false).await.expect_err("exit wait");
        let text = err.to_string();
        assert!(text.contains("report"), "{text}");
        assert!(text.contains("deadline"), "{text}");
        drop(receive);
        assert_session_dead(&mut session).await;
        assert_eq!(
            ReceivePackDeadlines::defaults().advertise,
            Duration::from_secs(crate::config::DEFAULT_GITHUB_SYNC_ADVERTISE_TIMEOUT_SECONDS)
        );
    }

    async fn assert_session_dead(session: &mut crate::ceres::github_sync::ssh::SshSession) {
        let started = std::time::Instant::now();
        match begin_with_deadlines(session, "acme/app", deadlines(LONG, LONG, LONG, LONG)).await {
            Ok(_) => panic!("aborted session accepted a new receive-pack"),
            Err(err) => assert!(
                started.elapsed() < Duration::from_millis(400),
                "dead session did not fail fast: {err}"
            ),
        }
    }

    async fn assert_aborted_fast(receive: &mut ReceivePack) {
        let started = std::time::Instant::now();
        let again = receive
            .write_pack(NEW_OID, std::iter::once([0_u8; 8].as_slice()))
            .await
            .expect_err("aborted channel");
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "follow-up did not fail fast after abort: {again}"
        );
        assert!(
            again.to_string().contains("aborted"),
            "follow-up should see the aborted channel: {again}"
        );
    }

    fn random_host() -> russh::keys::PrivateKey {
        russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
            .expect("host")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn deadline_cancels_inflight() {
        let _serial = crate::ceres::github_sync::key::HOLDER_TEST_SERIAL
            .lock()
            .await;
        let _clear = ClearHold;
        let exec = receive_pack_command("acme/app").expect("exec");
        let client_pub = install_client();
        let wide = 2 * 1024 * 1024;

        let inbound = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut session = connect_loopback(
            LoopServer {
                user: "git".to_string(),
                public: client_pub.clone(),
                expected_exec: exec.clone(),
                advertisement: None,
                report: None,
                send_stall: None,
                inbound: inbound.clone(),
            },
            random_host(),
            wide,
        )
        .await;
        match begin_with_deadlines(&mut session, "acme/app", deadlines(SHORT, LONG, LONG, LONG))
            .await
        {
            Ok(_) => panic!("advertise should time out"),
            Err(err) => {
                let text = err.to_string();
                assert!(text.contains("advertise"), "{text}");
                assert!(text.contains("deadline"), "{text}");
            }
        }
        assert_session_dead(&mut session).await;
        drop(session);

        let inbound = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let stall = SendStall::new();
        let mut session = connect_loopback(
            LoopServer {
                user: "git".to_string(),
                public: client_pub.clone(),
                expected_exec: exec.clone(),
                advertisement: Some(advertise_bytes()),
                report: None,
                send_stall: Some(stall.clone()),
                inbound: inbound.clone(),
            },
            random_host(),
            wide,
        )
        .await;
        let mut receive =
            begin_with_deadlines(&mut session, "acme/app", deadlines(LONG, SHORT, LONG, LONG))
                .await
                .expect("begin send");
        let fat = vec![7_u8; 3 * 1024 * 1024];
        let err = receive
            .write_pack(NEW_OID, std::iter::once(fat.as_slice()))
            .await
            .expect_err("send cancelled");
        assert!(err.to_string().contains("send"), "{err}");
        stall.release();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let after_timeout = inbound.lock().expect("inbound").len();
        assert!(
            after_timeout < fat.len() / 4,
            "queued transport writes survived abort: {after_timeout}"
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
        let later = inbound.lock().expect("inbound").len();
        assert_eq!(later, after_timeout, "queued writes continued after abort");
        assert_aborted_fast(&mut receive).await;
        drop(receive);
        assert_session_dead(&mut session).await;
        drop(session);

        let inbound = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut session = connect_loopback(
            LoopServer {
                user: "git".to_string(),
                public: client_pub.clone(),
                expected_exec: exec.clone(),
                advertisement: Some(advertise_bytes()),
                report: None,
                send_stall: None,
                inbound: inbound.clone(),
            },
            random_host(),
            wide,
        )
        .await;
        let mut receive =
            begin_with_deadlines(&mut session, "acme/app", deadlines(LONG, LONG, SHORT, LONG))
                .await
                .expect("begin report");
        receive
            .write_pack(NEW_OID, std::iter::once([0_u8; 8].as_slice()))
            .await
            .expect("pack");
        let err = receive.read_report(false).await.expect_err("report cancel");
        assert!(err.to_string().contains("report"), "{err}");
        assert_aborted_fast(&mut receive).await;
        drop(receive);
        assert_session_dead(&mut session).await;
        drop(session);

        let inbound = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut session = connect_loopback(
            LoopServer {
                user: "git".to_string(),
                public: client_pub,
                expected_exec: exec,
                advertisement: Some(advertise_bytes()),
                report: Some(encode(&["unpack ok\n", "ok refs/heads/main\n"])),
                send_stall: None,
                inbound,
            },
            random_host(),
            wide,
        )
        .await;
        let mut receive =
            begin_with_deadlines(&mut session, "acme/app", deadlines(LONG, LONG, LONG, SHORT))
                .await
                .expect("begin exit");
        receive
            .write_pack(NEW_OID, std::iter::once([0_u8; 8].as_slice()))
            .await
            .expect("pack");
        let err = receive.read_report(false).await.expect_err("exit cancel");
        assert!(err.to_string().contains("report"), "{err}");
        assert_aborted_fast(&mut receive).await;
        drop(receive);
        assert_session_dead(&mut session).await;
    }
}
