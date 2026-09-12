use std::{
    collections::{BTreeMap, HashSet},
    pin::Pin,
    time::Instant,
};

use anyhow::Result;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use futures::Stream;
use git_internal::hash::HashKind;
use tokio_stream::wrappers::ReceiverStream;

use crate::{
    callisto::sea_orm_active_enums::RefTypeEnum,
    ceres::{
        api_service::state::ProtocolApiState,
        protocol::{
            Capability, ServiceType, SideBind, SmartSession, TransportProtocol, ZERO_ID,
            import_refs::{CommandType, RefCommand},
        },
    },
    common::errors::ProtocolError,
};

const LF: char = '\n';

pub const SP: char = ' ';

const NUL: char = '\0';

pub const PKT_LINE_END_MARKER: &[u8; 4] = b"0000";
pub const PKT_LINE_DELIMITER: &[u8; 4] = b"0001";

// see https://git-scm.com/docs/protocol-capabilities
// Only advertise capabilities that are parsed, acted on, and covered by tests.
const RECEIVE_CAP_LIST: &str = "report-status delete-refs ";

// The ofs-delta and side-band-64k capabilities are sent and recognized by both upload-pack and receive-pack protocols.
// The agent and session-id capabilities may optionally be sent in both protocols.
const COMMON_CAP_LIST: &str = "side-band-64k ofs-delta agent=mega/0.1.0";

// All other capabilities are only recognized by the upload-pack (fetch from server) process.
const UPLOAD_CAP_LIST: &str = "multi_ack_detailed no-done shallow ";

fn advertised_capabilities(service_type: ServiceType, hash_kind: HashKind) -> String {
    let caps = match service_type {
        ServiceType::UploadPack => format!("{UPLOAD_CAP_LIST}{COMMON_CAP_LIST}"),
        ServiceType::ReceivePack => format!("{RECEIVE_CAP_LIST}{COMMON_CAP_LIST}"),
    };
    format!("{caps} object-format={}", hash_kind.as_str())
}

impl SmartSession {
    /// # Retrieves the information about Git references (refs) for the specified service type.
    ///
    /// The function returns a `BytesMut` object containing the Git reference information.
    ///
    /// The `service_type` is extracted from the `PackProtocol` instance.
    ///
    /// The function checks if the `object_id` of the head object in the storage is zero. If it is zero,
    /// the name is set to "capabilities^{}" to include capability declarations behind a NUL on the first ref.
    /// Otherwise, the name is set to "HEAD".
    ///
    /// The `cap_list` is determined based on the `service_type` and contains the appropriate capability lists.
    ///
    /// A packet line (`pkt_line`) is constructed using the `object_id`, `name`, `NUL` delimiter, `cap_list`, and line feed (`LF`).
    /// The `pkt_line` is added to the `ref_list`.
    ///
    /// The `object_id` and `name` pairs for other refs are retrieved using the `get_ref_object_id` method from the storage.
    /// Each pair is used to construct a packet line (`pkt_line`) and added to the `ref_list`.
    ///
    /// The `build_smart_reply` method is called with the `ref_list`, `service_type`, and its string representation
    /// to build a smart reply packet line stream.
    ///
    /// Tracing information is logged regarding the response packet line stream.
    ///
    /// Finally, the constructed packet line stream is returned.
    pub async fn git_info_refs(&self, state: &ProtocolApiState) -> Result<BytesMut, ProtocolError> {
        let repo_handler = self.repo_handler_with_commands(state, Vec::new()).await?;
        let service_type = self.service_type;

        // The stream MUST include capability declarations behind a NUL on the first ref.
        let (head_hash, git_refs) = repo_handler.refs_with_head_hash().await?;
        self.ensure_advertised_object_ids(&head_hash, &git_refs)?;
        let name = if head_hash == ZERO_ID {
            "capabilities^{}"
        } else {
            "HEAD"
        };
        let cap_list = advertised_capabilities(service_type, self.hash_kind);
        let pkt_line = format!("{head_hash}{SP}{name}{NUL}{cap_list}{LF}");
        let mut ref_list = vec![pkt_line];

        for git_ref in git_refs {
            let pkt_line = format!("{}{}{}{}", git_ref.ref_hash, SP, git_ref.ref_name, LF);
            ref_list.push(pkt_line);
        }
        let pkt_line_stream = self.build_smart_reply(&ref_list, service_type.to_string());
        tracing::info!("git_info_refs, return: --------> {:?}", pkt_line_stream);
        Ok(pkt_line_stream)
    }

    pub async fn git_upload_pack(
        &mut self,
        state: &ProtocolApiState,
        upload_request: &mut Bytes,
    ) -> Result<(ReceiverStream<Vec<u8>>, BytesMut), ProtocolError> {
        let repo_handler = self.repo_handler_with_commands(state, Vec::new()).await?;

        let mut want: HashSet<String> = HashSet::new();
        let mut have: HashSet<String> = HashSet::new();
        let mut last_common_commit = String::new();
        let mut deepen_depth: Option<u32> = None;
        let mut deepen_relative = false;
        let mut shallow_commits: Vec<String> = Vec::new();

        let mut read_first_line = false;
        loop {
            let pkt_line = try_read_pkt_line(upload_request)?;
            let dst = match pkt_line {
                PktLine::Flush => {
                    if upload_request.is_empty() {
                        break;
                    } else {
                        continue;
                    }
                }
                PktLine::Data(data) => data.to_vec(),
                _ => {
                    return Err(ProtocolError::InvalidInput(
                        "unexpected pkt-line type in upload-pack".to_owned(),
                    ));
                }
            };
            if dst.len() < 4 {
                return Err(ProtocolError::InvalidInput(
                    "pkt-line command is shorter than 4 bytes".to_owned(),
                ));
            }
            let commands = &dst[0..4];

            match commands {
                b"want" => {
                    if dst.len() < 45 {
                        return Err(ProtocolError::InvalidInput(
                            "want command is missing object id".to_owned(),
                        ));
                    }
                    want.insert(String::from_utf8(dst[5..45].to_vec()).map_err(|_| {
                        ProtocolError::InvalidInput("want object id is not valid UTF-8".to_owned())
                    })?);
                }
                b"have" => {
                    if dst.len() < 45 {
                        return Err(ProtocolError::InvalidInput(
                            "have command is missing object id".to_owned(),
                        ));
                    }
                    have.insert(String::from_utf8(dst[5..45].to_vec()).map_err(|_| {
                        ProtocolError::InvalidInput("have object id is not valid UTF-8".to_owned())
                    })?);
                }
                b"done" => break,
                b"deep" => {
                    let payload = &dst[4..];
                    if payload.starts_with(b"en ") {
                        let depth_str = core::str::from_utf8(&payload[3..])
                            .map_err(|_| {
                                ProtocolError::InvalidInput(
                                    "deepen depth is not valid UTF-8".to_owned(),
                                )
                            })?
                            .trim();
                        deepen_depth = Some(depth_str.parse::<u32>().map_err(|_| {
                            ProtocolError::InvalidInput(format!(
                                "invalid deepen depth: {depth_str}"
                            ))
                        })?);
                    } else if payload.starts_with(b"en-relative") {
                        deepen_relative = true;
                    } else if payload.starts_with(b"en-since") || payload.starts_with(b"en-not") {
                        return Err(ProtocolError::InvalidInput(
                            "deepen-since and deepen-not are not supported".to_owned(),
                        ));
                    } else {
                        tracing::warn!(
                            "unknown deepen variant: {:?}",
                            String::from_utf8_lossy(payload)
                        );
                    }
                }
                other => {
                    tracing::error!(
                        "unsupported command: {:?}",
                        String::from_utf8(other.to_vec())
                    );
                    continue;
                }
            };
            if !read_first_line {
                if dst.len() > 46 {
                    let caps = core::str::from_utf8(&dst[46..]).map_err(|_| {
                        ProtocolError::InvalidInput("capabilities are not valid UTF-8".to_owned())
                    })?;
                    self.parse_capabilities(caps);
                }
                read_first_line = true;
            }
        }

        tracing::info!(
            "want commands: {:?}\n have commands: {:?}\n deepen: {:?}\n caps:{:?}",
            want,
            have,
            deepen_depth,
            self.capabilities
        );

        let pack_data;
        let mut protocol_buf = BytesMut::new();

        let want: Vec<String> = want.into_iter().collect();
        let have: Vec<String> = have.into_iter().collect();

        // Empty want can appear when an SSH data chunk is handled before the
        // client has sent object ids. Do not attempt pack generation.
        if want.is_empty() {
            let (_tx, rx) = tokio::sync::mpsc::channel(1);
            add_pkt_line_string(&mut protocol_buf, String::from("NAK\n"));
            return Ok((ReceiverStream::new(rx), protocol_buf));
        }

        // Capability honesty: `shallow` is advertised for upload-pack, but
        // only Monorepo genuinely implements depth-limited pack generation.
        // ImportRepo's default trait `shallow_pack` silently falls back to
        // `full_pack`, which would mislead clients into thinking they got a
        // shallow clone. Return an explicit protocol error instead.
        if deepen_depth.is_some() && !repo_handler.supports_shallow_fetch() {
            return Err(ProtocolError::InvalidInput(
                "shallow fetch is not supported for this repository".to_owned(),
            ));
        }
        // Shallow incremental fetch (deepen + non-empty have) is not
        // implemented: the code would fall through to `incremental_pack`
        // without applying depth or emitting `shallow` lines, silently
        // producing a non-shallow pack. Reject this combination explicitly.
        if deepen_depth.is_some() && !have.is_empty() {
            return Err(ProtocolError::InvalidInput(
                "shallow fetch with non-empty have is not supported".to_owned(),
            ));
        }

        if have.is_empty() {
            if let Some(depth) = deepen_depth {
                let (stream, shallows) = repo_handler
                    .shallow_pack(want, depth, deepen_relative)
                    .await
                    .map_err(|e| {
                        ProtocolError::InvalidInput(format!("shallow pack generation failed: {e}"))
                    })?;
                pack_data = stream;
                shallow_commits = shallows;
            } else {
                pack_data = repo_handler.full_pack(want).await.map_err(|e| {
                    ProtocolError::InvalidInput(format!("pack generation failed: {e}"))
                })?;
            }
            add_pkt_line_string(&mut protocol_buf, String::from("NAK\n"));
        } else {
            if self.capabilities.contains(&Capability::MultiAckDetailed) {
                for hash in &have {
                    if repo_handler.check_commit_exist(hash).await {
                        add_pkt_line_string(&mut protocol_buf, format!("ACK {hash} common\n"));
                        if last_common_commit.is_empty() {
                            last_common_commit = hash.to_string();
                        }
                    }
                }
                pack_data = repo_handler
                    .incremental_pack(want.clone(), have)
                    .await
                    .map_err(|e| {
                        ProtocolError::InvalidInput(format!("pack generation failed: {e}"))
                    })?;

                if last_common_commit.is_empty() {
                    add_pkt_line_string(&mut protocol_buf, String::from("NAK\n"));
                    return Ok((pack_data, protocol_buf));
                }

                for hash in want {
                    if self.capabilities.contains(&Capability::NoDone) {
                        add_pkt_line_string(&mut protocol_buf, format!("ACK {hash} ready\n"));
                    }
                }
            } else {
                tracing::error!("capability unsupported");
                let (_, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
                pack_data = ReceiverStream::new(rx);
            }
            add_pkt_line_string(&mut protocol_buf, format!("ACK {last_common_commit} \n"));
        }

        for shallow in &shallow_commits {
            add_pkt_line_string(&mut protocol_buf, format!("shallow {shallow}\n"));
        }

        Ok((pack_data, protocol_buf))
    }

    pub fn parse_receive_pack_commands(
        &mut self,
        mut protocol_bytes: Bytes,
    ) -> Result<Vec<RefCommand>, ProtocolError> {
        let mut commands: Vec<RefCommand> = Vec::new();
        while !protocol_bytes.is_empty() {
            let pkt_line = try_read_pkt_line(&mut protocol_bytes)?;
            if let PktLine::Data(mut data) = pkt_line {
                let command = self.parse_receive_pack_command_line(&mut data)?;
                commands.push(command);
            }
        }
        Ok(commands)
    }

    pub fn split_receive_pack_request(
        &mut self,
        mut protocol_bytes: Bytes,
    ) -> Result<(Vec<RefCommand>, Bytes), ProtocolError> {
        let mut commands: Vec<RefCommand> = Vec::new();

        while !protocol_bytes.is_empty() {
            let pkt_line = try_read_pkt_line(&mut protocol_bytes)?;
            match pkt_line {
                PktLine::Flush => {
                    if commands.is_empty() {
                        return Err(ProtocolError::InvalidInput(
                            "receive-pack request contains no commands".to_owned(),
                        ));
                    }
                    if !Self::is_delete_only_push(&commands) {
                        if protocol_bytes.is_empty() {
                            return Err(ProtocolError::InvalidInput(
                                "receive-pack request missing pack payload".to_owned(),
                            ));
                        }
                        if !protocol_bytes.starts_with(b"PACK") {
                            return Err(ProtocolError::InvalidInput(
                                "receive-pack request pack payload does not start with PACK"
                                    .to_owned(),
                            ));
                        }
                    }
                    return Ok((commands, protocol_bytes));
                }
                PktLine::Data(mut data) => {
                    let command = self.parse_receive_pack_command_line(&mut data)?;
                    commands.push(command);
                }
                _ => {
                    return Err(ProtocolError::InvalidInput(
                        "unexpected pkt-line type in receive-pack".to_owned(),
                    ));
                }
            }
        }

        Err(ProtocolError::InvalidInput(
            "receive-pack command list missing flush-pkt".to_owned(),
        ))
    }

    fn is_delete_only_push(commands: &[RefCommand]) -> bool {
        !commands.is_empty()
            && commands
                .iter()
                .all(|c| c.command_type == CommandType::Delete)
    }

    fn parse_receive_pack_command_line(
        &mut self,
        pkt_line: &mut Bytes,
    ) -> Result<RefCommand, ProtocolError> {
        let command = Self::parse_ref_command(pkt_line);
        let caps = core::str::from_utf8(pkt_line).map_err(|_| {
            ProtocolError::InvalidInput("capabilities are not valid UTF-8".to_owned())
        })?;
        self.parse_capabilities(caps);
        tracing::debug!(
            "parse ref_command: {:?}, with caps:{:?}",
            command,
            self.capabilities
        );
        Ok(command)
    }

    pub async fn git_receive_pack_stream(
        &mut self,
        state: &ProtocolApiState,
        commands: Vec<RefCommand>,
        data_stream: Pin<Box<dyn Stream<Item = Result<Bytes, axum::Error>> + Send>>,
    ) -> Result<Bytes, ProtocolError> {
        let t0 = Instant::now();
        let mut timings_ms: BTreeMap<String, u128> = BTreeMap::new();
        let mut metrics: BTreeMap<String, u128> = BTreeMap::new();
        // After receiving the pack data from the sender, the receiver sends a report
        let mut report_status = BytesMut::new();
        let mut commands = commands;
        let repo_handler = self
            .repo_handler_with_commands(state, commands.clone())
            .await?;
        let is_monorepo = repo_handler.is_monorepo();
        //1. unpack progress
        let delete_only = Self::is_delete_only_push(&commands);
        let unpack_result = if delete_only {
            timings_ms.insert("unpack_stream_ms".to_string(), 0);
            timings_ms.insert("receiver_handler_ms".to_string(), 0);
            Ok(())
        } else {
            let t_unpack = Instant::now();
            let receiver = repo_handler
                .unpack_stream(&state.storage.config().pack, data_stream)
                .await?;
            timings_ms.insert(
                "unpack_stream_ms".to_string(),
                t_unpack.elapsed().as_millis(),
            );

            let t_receiver = Instant::now();
            let res = repo_handler
                .clone()
                .receiver_handler(receiver.0, receiver.1)
                .await;
            timings_ms.insert(
                "receiver_handler_ms".to_string(),
                t_receiver.elapsed().as_millis(),
            );
            res
        };

        // write "unpack ok\n to report"
        add_pkt_line_string(&mut report_status, "unpack ok\n".to_owned());

        let mut default_exist = repo_handler.check_default_branch().await;

        let mut unpack_failed = false;

        // 2. Tags: persist immediately. Branches: only unpack / default-branch flags here;
        //    mono and import both persist branch refs inside `finalize_receive_pack`.
        for command in commands.iter_mut() {
            if command.ref_type == RefTypeEnum::Tag {
                // just update if refs type is tag
                if let Err(e) = repo_handler.update_refs(command).await {
                    command.failed(e.to_string());
                }
            } else {
                // Updates can be unsuccessful for a number of reasons.
                // a.The reference can have changed since the reference discovery phase was originally sent, meaning someone pushed in the meantime.
                // b.The reference being pushed could be a non-fast-forward reference and the update hooks or configuration could be set to not allow that, etc.
                // c.Also, some references can be updated while others can be rejected.
                match unpack_result {
                    Ok(_) => {
                        if !default_exist {
                            command.default_branch = true;
                            default_exist = true;
                        }
                    }
                    Err(ref err) => {
                        command.failed(err.to_string());
                        unpack_failed = true;
                    }
                }
            }
        }

        // Handler was built with an early `commands.clone()`; merge loop updates (e.g.
        // `default_branch`) before finalize so import/mono metadata uses the final commands.
        repo_handler.sync_commands_after_unpack(&commands);

        let mut finalize_ms: Option<u128> = None;
        let mut bind_ms: Option<u128> = None;
        let mut finalize_failed = false;
        let mut receive_notice: Option<String> = None;
        if !unpack_failed {
            let t_finalize = Instant::now();
            if let Err(e) = repo_handler.finalize_receive_pack().await {
                // UN-16: a per-ref rejection (e.g. main-branch delete) must reach
                // the git client as an actionable `ng <ref> <reason>` report-status
                // line, not a bare HTTP 400. Mark the branch commands failed and
                // continue to build the report; skip the post-finalize bindings
                // (the refs were not written).
                let msg = e.to_string();
                for c in commands.iter_mut() {
                    if c.ref_type == RefTypeEnum::Branch && c.status == "ok" {
                        c.failed(msg.clone());
                    }
                }
                finalize_failed = true;
            } else {
                finalize_ms = Some(t_finalize.elapsed().as_millis());
                // ADR-MC-05: a no-op push notice only accompanies a successful
                // finalize (a failed one already reports `ng` lines).
                receive_notice = repo_handler.receive_pack_notice();
            }

            if !finalize_failed && repo_handler.bind_tip_after_receive() {
                let t_bind = Instant::now();
                self.process_commit_bindings(state, &commands).await;
                bind_ms = Some(t_bind.elapsed().as_millis());
            }
        }

        for command in &commands {
            add_pkt_line_string(&mut report_status, command.get_status());
        }

        let buf = self.build_receive_pack_report(report_status, receive_notice);

        if let Some(ms) = finalize_ms {
            timings_ms.insert("finalize_receive_pack_ms".to_string(), ms);
        }
        if let Some(ms) = bind_ms {
            timings_ms.insert("process_commit_bindings_ms".to_string(), ms);
        }
        for (k, v) in repo_handler.receive_pack_extra_timings_ms() {
            if k.ends_with("_count") {
                metrics.insert(k, v);
            } else {
                timings_ms.insert(k, v);
            }
        }
        timings_ms.insert("total_ms".to_string(), t0.elapsed().as_millis());

        let timings_pretty = timings_ms
            .iter()
            .map(|(k, v)| format!("  - {k}: {v}ms"))
            .collect::<Vec<_>>()
            .join("\n");
        let metrics_pretty = if metrics.is_empty() {
            "  (none)".to_string()
        } else {
            metrics
                .iter()
                .map(|(k, v)| format!("  - {k}: {v}"))
                .collect::<Vec<_>>()
                .join("\n")
        };

        tracing::info!(
            service = "git-receive-pack",
            repo_path = %self.repo_path.display(),
            is_monorepo = is_monorepo,
            command_count = commands.len(),
            unpack_ok = unpack_result.is_ok(),
            timings_ms = ?timings_ms,
            metrics = ?metrics,
            "\nreceive-pack timing report\n\ntimings_ms:\n{timings_pretty}\nmetrics:\n{metrics_pretty}"
        );
        Ok(buf.into())
    }

    /// # Builds the packet data in the sideband format if the SideBand/64k capability is enabled.
    ///
    /// If the `SideBand` or `SideBand64k` capability is present in the `capabilities` vector,
    /// the `from_bytes` data is transformed into the sideband format.
    /// The resulting packet data is returned in a `BytesMut` object.
    ///
    /// The `length` parameter represents the length of the `from_bytes` data.
    /// It is used to calculate the length of the transformed packet data.
    ///
    /// If the sideband format is enabled, the resulting packet data is constructed as follows:
    /// - The length of the packet data (including header) is calculated by adding 5 to the `length`.
    /// - The length value is formatted as a hexadecimal string and prepended to the `to_bytes` buffer.
    /// - The sideband type (`PackfileData`) is added as a single byte to the `to_bytes` buffer.
    /// - The `from_bytes` data is appended to the `to_bytes` buffer.
    /// - The `to_bytes` buffer containing the transformed packet data is returned.
    ///
    /// If the sideband format is not enabled, the `from_bytes` data is returned unchanged.
    pub fn build_side_band_format(&self, from_bytes: BytesMut, length: usize) -> BytesMut {
        let capabilities = &self.capabilities;
        if capabilities.contains(&Capability::SideBand)
            || capabilities.contains(&Capability::SideBand64k)
        {
            let mut to_bytes = BytesMut::new();
            let length = length + 5;
            to_bytes.put(Bytes::from(format!("{length:04x}")));
            to_bytes.put_u8(SideBind::PackfileData.value());
            to_bytes.put(from_bytes);
            return to_bytes;
        }
        from_bytes
    }

    /// Build the receive-pack response tail: the ADR-MC-05 no-op notice as a
    /// sideband progress frame (channel 2 — git clients render it with a
    /// `remote: ` prefix), then the report-status payload (channel 1 when a
    /// side-band capability was negotiated), then the final flush packet.
    ///
    /// Without a negotiated side-band capability there is no progress channel,
    /// so the notice is not sent at all (the server log already recorded it)
    /// rather than writing raw bytes that would corrupt the report stream.
    fn build_receive_pack_report(
        &self,
        mut report_status: BytesMut,
        notice: Option<String>,
    ) -> BytesMut {
        report_status.put(&PKT_LINE_END_MARKER[..]);
        let length = report_status.len();
        let report = self.build_side_band_format(report_status, length);

        let mut buf = BytesMut::new();
        let side_band = self.capabilities.contains(&Capability::SideBand)
            || self.capabilities.contains(&Capability::SideBand64k);
        if side_band && let Some(notice) = notice {
            let notice = if notice.ends_with('\n') {
                notice
            } else {
                format!("{notice}\n")
            };
            // pkt-line length covers the header, the channel byte, and the payload.
            buf.put(Bytes::from(format!("{:04x}", notice.len() + 5)));
            buf.put_u8(SideBind::ProgressInfo.value());
            buf.put(Bytes::from(notice.into_bytes()));
        }
        buf.extend_from_slice(&report);
        buf.put(&PKT_LINE_END_MARKER[..]);
        buf
    }

    pub fn build_smart_reply(&self, ref_list: &Vec<String>, service: String) -> BytesMut {
        let mut pkt_line_stream = BytesMut::new();
        if self.transport_protocol == TransportProtocol::Http {
            add_pkt_line_string(&mut pkt_line_stream, format!("# service={service}\n"));
            pkt_line_stream.put(&PKT_LINE_END_MARKER[..]);
        }

        for ref_line in ref_list {
            add_pkt_line_string(&mut pkt_line_stream, ref_line.to_string());
        }
        pkt_line_stream.put(&PKT_LINE_END_MARKER[..]);
        pkt_line_stream
    }

    pub fn parse_capabilities(&mut self, cap_str: &str) {
        let cap_vec: Vec<_> = cap_str.split(' ').collect();
        for cap in cap_vec {
            let res = cap.trim().parse::<Capability>();
            if let Ok(cap) = res {
                self.capabilities.insert(cap);
            }
        }
    }

    // the first line contains the capabilities
    pub fn parse_ref_command(pkt_line: &mut Bytes) -> RefCommand {
        RefCommand::new(
            read_until_white_space(pkt_line),
            read_until_white_space(pkt_line),
            read_until_white_space(pkt_line),
        )
    }

    /// Process commit bindings for successfully pushed commits
    // NOTE (plan-20260827, updated MC-06/Codex R3 P1): this protocol-layer
    // tip binding now serves ImportRepo only. Monorepo opts out via
    // `RepoHandler::bind_tip_after_receive` — its post-push pipeline binds
    // exactly the accepted chain's newly introduced commits
    // (`Monorepo::run_mono_post_push_pipeline`), so an unconditional upsert
    // here could still clobber a known tip's existing binding on an
    // empty-pack idempotent re-push or a known-tip push.
    async fn process_commit_bindings(&self, state: &ProtocolApiState, commands: &[RefCommand]) {
        for command in commands {
            // Only process successful branch updates (not tags or failed commands)
            if command.ref_type == RefTypeEnum::Branch
                && command.status == "ok"
                && command.new_id != ZERO_ID
                && let Err(e) = self.bind_commit_to_user(state, &command.new_id).await
            {
                tracing::warn!("Failed to bind commit {} to user: {}", command.new_id, e);
                // Don't fail the push on binding errors
            }
        }
    }

    /// Bind a single commit to a user based on authenticated user only (username-only model)
    async fn bind_commit_to_user(
        &self,
        state: &ProtocolApiState,
        commit_sha: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let commit_binding_storage = state.storage.commit_binding_storage();

        // If there is an authenticated user, bind to their username; otherwise anonymous
        let (matched_username, is_anonymous) =
            if let Some(authenticated_user) = &self.auth.authenticated_user {
                (Some(authenticated_user.username.clone()), false)
            } else {
                (None, true)
            };

        // Upsert the binding using the simplified storage API
        commit_binding_storage
            .upsert_binding(commit_sha, matched_username.clone(), is_anonymous)
            .await?;

        tracing::info!(
            "Bound commit {} -> {}",
            commit_sha,
            if is_anonymous {
                "anonymous".to_string()
            } else {
                matched_username.unwrap_or_else(|| "unknown".to_string())
            }
        );

        Ok(())
    }
}

// SmartProtocol struct removed; remaining codec helpers live on SmartSession.

fn read_until_white_space(bytes: &mut Bytes) -> String {
    let mut buf = Vec::new();
    while bytes.has_remaining() {
        let c = bytes.get_u8();
        if c.is_ascii_whitespace() || c == 0 {
            break;
        }
        buf.push(c);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

pub fn add_pkt_line_string(pkt_line_stream: &mut BytesMut, buf_str: String) {
    let buf_str_length = buf_str.len() + 4;
    pkt_line_stream.put(Bytes::from(format!("{buf_str_length:04x}")));
    pkt_line_stream.put(buf_str.as_bytes());
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PktLine {
    Data(Bytes),
    Flush,
    Delim,
    ResponseEnd,
}

pub fn try_read_pkt_line(bytes: &mut Bytes) -> Result<PktLine, ProtocolError> {
    if bytes.is_empty() {
        return Err(ProtocolError::InvalidInput(
            "no pkt-line data available".to_owned(),
        ));
    }
    if bytes.len() < 4 {
        return Err(ProtocolError::InvalidInput(
            "pkt-line length header is incomplete".to_owned(),
        ));
    }
    let pkt_length_bytes = &bytes[..4];
    if !pkt_length_bytes.iter().all(u8::is_ascii_hexdigit) {
        return Err(ProtocolError::InvalidInput(
            "pkt-line length header is not hexadecimal".to_owned(),
        ));
    }
    let pkt_length = usize::from_str_radix(
        core::str::from_utf8(pkt_length_bytes).map_err(|_| {
            ProtocolError::InvalidInput("pkt-line length header is invalid".to_owned())
        })?,
        16,
    )
    .map_err(|_| ProtocolError::InvalidInput("pkt-line length header is invalid".to_owned()))?;

    match pkt_length {
        0 => {
            bytes.advance(4);
            Ok(PktLine::Flush)
        }
        1 => {
            bytes.advance(4);
            Ok(PktLine::Delim)
        }
        2 => {
            bytes.advance(4);
            Ok(PktLine::ResponseEnd)
        }
        3 => Err(ProtocolError::InvalidInput(
            "pkt-line length 0x0003 is reserved".to_owned(),
        )),
        n if n < 4 => Err(ProtocolError::InvalidInput(
            "pkt-line length is smaller than header".to_owned(),
        )),
        n => {
            if bytes.len() < n {
                return Err(ProtocolError::InvalidInput(
                    "pkt-line payload is incomplete".to_owned(),
                ));
            }
            bytes.advance(4);
            let pkt_line = bytes.copy_to_bytes(n - 4);
            tracing::debug!("pkt line: {:?}", pkt_line);
            Ok(PktLine::Data(pkt_line))
        }
    }
}

#[cfg(test)]
pub mod test {
    use std::{process::Command, time::Duration};

    use bytes::{BufMut, Bytes, BytesMut};
    use futures::future;
    use git_internal::hash::HashKind;
    use tempfile::TempDir;
    use tokio::{task, time::sleep};

    use crate::{
        callisto::sea_orm_active_enums::RefTypeEnum,
        ceres::protocol::{
            Capability, ServiceType, SmartSession, TransportProtocol,
            import_refs::{CommandType, RefCommand},
            smart::{
                PKT_LINE_END_MARKER, PktLine, add_pkt_line_string, advertised_capabilities,
                read_until_white_space, try_read_pkt_line,
            },
        },
        common::errors::ProtocolError,
    };

    #[test]
    pub fn test_read_pkt_line() {
        let mut bytes = Bytes::from_static(b"001e# service=git-upload-pack\n");
        let pkt_line = try_read_pkt_line(&mut bytes).unwrap();
        assert_eq!(
            pkt_line,
            PktLine::Data(Bytes::from_static(b"# service=git-upload-pack\n"))
        );
    }

    #[test]
    pub fn try_read_pkt_line_rejects_non_hex_header() {
        let mut bytes = Bytes::from_static(b"zzzzwant");
        let err = try_read_pkt_line(&mut bytes).unwrap_err();

        assert!(matches!(err, ProtocolError::InvalidInput(_)));
        assert_eq!(&bytes[..], b"zzzzwant");
    }

    #[test]
    pub fn try_read_pkt_line_rejects_incomplete_header_without_consuming() {
        let mut bytes = Bytes::from_static(b"00f");
        let err = try_read_pkt_line(&mut bytes).unwrap_err();

        assert!(matches!(err, ProtocolError::InvalidInput(_)));
        assert_eq!(&bytes[..], b"00f");
    }

    #[test]
    pub fn try_read_pkt_line_rejects_incomplete_payload() {
        let mut bytes = Bytes::from_static(b"000babc");
        let err = try_read_pkt_line(&mut bytes).unwrap_err();

        assert!(matches!(err, ProtocolError::InvalidInput(_)));
        assert_eq!(&bytes[..], b"000babc");
    }

    #[test]
    pub fn try_read_pkt_line_parses_flush() {
        let mut bytes = Bytes::from_static(b"0000trailing");
        let pkt_line = try_read_pkt_line(&mut bytes).unwrap();
        assert_eq!(pkt_line, PktLine::Flush);
        assert_eq!(&bytes[..], b"trailing");
    }

    #[test]
    pub fn try_read_pkt_line_parses_delim() {
        let mut bytes = Bytes::from_static(b"0001trailing");
        let pkt_line = try_read_pkt_line(&mut bytes).unwrap();
        assert_eq!(pkt_line, PktLine::Delim);
        assert_eq!(&bytes[..], b"trailing");
    }

    #[test]
    pub fn try_read_pkt_line_parses_response_end() {
        let mut bytes = Bytes::from_static(b"0002trailing");
        let pkt_line = try_read_pkt_line(&mut bytes).unwrap();
        assert_eq!(pkt_line, PktLine::ResponseEnd);
        assert_eq!(&bytes[..], b"trailing");
    }

    #[test]
    pub fn try_read_pkt_line_rejects_reserved_length_3() {
        let mut bytes = Bytes::from_static(b"0003");
        let err = try_read_pkt_line(&mut bytes).unwrap_err();

        assert!(matches!(err, ProtocolError::InvalidInput(_)));
        assert!(err.to_string().contains("reserved"));
        assert_eq!(&bytes[..], b"0003");
    }

    #[test]
    pub fn try_read_pkt_line_rejects_length_smaller_than_header() {
        let mut bytes = Bytes::from_static(b"0003want");
        let err = try_read_pkt_line(&mut bytes).unwrap_err();

        assert!(matches!(err, ProtocolError::InvalidInput(_)));
        assert!(
            err.to_string().contains("reserved") || err.to_string().contains("smaller than header")
        );
        assert_eq!(&bytes[..], b"0003want");
    }

    #[test]
    pub fn try_read_pkt_line_rejects_empty_input() {
        let mut bytes = Bytes::new();
        let err = try_read_pkt_line(&mut bytes).unwrap_err();
        assert!(matches!(err, ProtocolError::InvalidInput(_)));
    }

    #[test]
    pub fn try_read_pkt_line_parses_data() {
        let mut bytes = Bytes::from_static(b"000Bexample");
        let pkt_line = try_read_pkt_line(&mut bytes).unwrap();
        assert_eq!(pkt_line, PktLine::Data(Bytes::from_static(b"example")));
    }

    #[test]
    pub fn test_build_smart_reply() {
        let mock = SmartSession::new(
            std::path::PathBuf::new(),
            ServiceType::UploadPack,
            TransportProtocol::Http,
        );
        let ref_list = vec![String::from(
            "7bdc783132575d5b3e78400ace9971970ff43a18 refs/heads/master\0report-status report-status-v2 thin-pack side-band side-band-64k ofs-delta shallow deepen-since deepen-not deepen-relative multi_ack_detailed no-done object-format=sha1\n",
        )];
        let pkt_line_stream = mock.build_smart_reply(&ref_list, String::from("git-upload-pack"));
        assert_eq!(&pkt_line_stream[..], b"001e# service=git-upload-pack\n000000e87bdc783132575d5b3e78400ace9971970ff43a18 refs/heads/master\0report-status report-status-v2 thin-pack side-band side-band-64k ofs-delta shallow deepen-since deepen-not deepen-relative multi_ack_detailed no-done object-format=sha1\n0000")
    }

    #[test]
    pub fn test_add_to_pkt_line() {
        let mut buf = BytesMut::new();
        add_pkt_line_string(
            &mut buf,
            format!(
                "ACK {} common\n",
                "7bdc783132575d5b3e78400ace9971970ff43a18"
            ),
        );
        add_pkt_line_string(
            &mut buf,
            format!("ACK {} ready\n", "7bdc783132575d5b3e78400ace9971970ff43a18"),
        );
        assert_eq!(&buf.freeze()[..], b"0038ACK 7bdc783132575d5b3e78400ace9971970ff43a18 common\n0037ACK 7bdc783132575d5b3e78400ace9971970ff43a18 ready\n");
    }

    #[test]
    pub fn test_read_until_white_space() {
        let mut bytes = Bytes::from("Mega - A Monorepo Platform Engine".as_bytes());
        let result = read_until_white_space(&mut bytes);
        assert_eq!(result, "Mega");

        let mut bytes = Bytes::from("Hello,World!".as_bytes());
        let result = read_until_white_space(&mut bytes);
        assert_eq!(result, "Hello,World!");

        let mut bytes = Bytes::from("".as_bytes());
        let result = read_until_white_space(&mut bytes);
        assert_eq!(result, "");
    }

    #[test]
    pub fn test_parse_ref_update() {
        let mut bytes = Bytes::from("0000000000000000000000000000000000000000 27dd8d4cf39f3868c6eee38b601bc9e9939304f5 refs/heads/main\0".as_bytes());
        let result = SmartSession::parse_ref_command(&mut bytes);

        let command = RefCommand {
            ref_name: String::from("refs/heads/main"),
            old_id: String::from("0000000000000000000000000000000000000000"),
            new_id: String::from("27dd8d4cf39f3868c6eee38b601bc9e9939304f5"),
            status: String::from("ok"),
            error_msg: String::new(),
            command_type: CommandType::Create,
            ref_type: RefTypeEnum::Branch,
            default_branch: false,
        };
        assert_eq!(result, command);
    }

    #[test]
    pub fn split_receive_pack_request_uses_flush_not_pack_magic() {
        let mut session = SmartSession::new(
            std::path::PathBuf::new(),
            ServiceType::ReceivePack,
            TransportProtocol::Http,
        );
        let mut request = BytesMut::new();
        add_pkt_line_string(
            &mut request,
            "0000000000000000000000000000000000000000 27dd8d4cf39f3868c6eee38b601bc9e9939304f5 refs/heads/main\0report-status agent=PACK-test\n".to_owned(),
        );
        request.extend_from_slice(PKT_LINE_END_MARKER);
        request.extend_from_slice(b"PACKpayload");

        let (commands, pack_bytes) = session
            .split_receive_pack_request(request.freeze())
            .unwrap();

        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].ref_name, "refs/heads/main");
        assert_eq!(&pack_bytes[..], b"PACKpayload");
        assert!(session.capabilities.contains(&Capability::ReportStatus));
    }

    #[test]
    pub fn split_receive_pack_request_requires_flush_pkt() {
        let mut session = SmartSession::new(
            std::path::PathBuf::new(),
            ServiceType::ReceivePack,
            TransportProtocol::Http,
        );
        let mut request = BytesMut::new();
        add_pkt_line_string(
            &mut request,
            "0000000000000000000000000000000000000000 27dd8d4cf39f3868c6eee38b601bc9e9939304f5 refs/heads/main\0report-status\n".to_owned(),
        );

        let err = session
            .split_receive_pack_request(request.freeze())
            .unwrap_err();

        assert!(matches!(err, ProtocolError::InvalidInput(_)));
        assert!(err.to_string().contains("missing flush-pkt"));
    }

    #[test]
    pub fn split_receive_pack_request_accepts_delete_only_without_pack_payload() {
        let mut session = SmartSession::new(
            std::path::PathBuf::new(),
            ServiceType::ReceivePack,
            TransportProtocol::Http,
        );
        let mut request = BytesMut::new();
        add_pkt_line_string(
            &mut request,
            "27dd8d4cf39f3868c6eee38b601bc9e9939304f5 0000000000000000000000000000000000000000 refs/heads/old\0report-status\n"
                .to_owned(),
        );
        request.extend_from_slice(PKT_LINE_END_MARKER);

        let (commands, pack_bytes) = session
            .split_receive_pack_request(request.freeze())
            .unwrap();

        assert!(pack_bytes.is_empty());
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].ref_name, "refs/heads/old");
        assert_eq!(commands[0].command_type, CommandType::Delete);
        assert!(SmartSession::is_delete_only_push(&commands));
    }

    #[test]
    pub fn split_receive_pack_request_rejects_non_delete_without_pack_payload() {
        let mut session = SmartSession::new(
            std::path::PathBuf::new(),
            ServiceType::ReceivePack,
            TransportProtocol::Http,
        );
        let mut request = BytesMut::new();
        add_pkt_line_string(
            &mut request,
            "0000000000000000000000000000000000000000 27dd8d4cf39f3868c6eee38b601bc9e9939304f5 refs/heads/main\0report-status\n"
                .to_owned(),
        );
        request.extend_from_slice(PKT_LINE_END_MARKER);

        let err = session
            .split_receive_pack_request(request.freeze())
            .unwrap_err();

        assert!(matches!(err, ProtocolError::InvalidInput(_)));
        assert!(err.to_string().contains("missing pack payload"));
    }

    #[test]
    pub fn split_receive_pack_request_rejects_non_delete_invalid_pack_magic() {
        let mut session = SmartSession::new(
            std::path::PathBuf::new(),
            ServiceType::ReceivePack,
            TransportProtocol::Http,
        );
        let mut request = BytesMut::new();
        add_pkt_line_string(
            &mut request,
            "0000000000000000000000000000000000000000 27dd8d4cf39f3868c6eee38b601bc9e9939304f5 refs/heads/main\0report-status\n"
                .to_owned(),
        );
        request.extend_from_slice(PKT_LINE_END_MARKER);
        request.extend_from_slice(b"NOPEpayload");

        let err = session
            .split_receive_pack_request(request.freeze())
            .unwrap_err();

        assert!(matches!(err, ProtocolError::InvalidInput(_)));
        assert!(err.to_string().contains("does not start with PACK"));
    }

    #[test]
    pub fn split_receive_pack_request_rejects_empty_command_list() {
        let mut session = SmartSession::new(
            std::path::PathBuf::new(),
            ServiceType::ReceivePack,
            TransportProtocol::Http,
        );
        let mut request = BytesMut::new();
        request.extend_from_slice(PKT_LINE_END_MARKER);
        request.extend_from_slice(b"PACKpayload");

        let err = session
            .split_receive_pack_request(request.freeze())
            .unwrap_err();

        assert!(matches!(err, ProtocolError::InvalidInput(_)));
        assert!(err.to_string().contains("no commands"));
    }

    #[test]
    pub fn test_parse_capabilities() {
        let mut mock = SmartSession::new(
            std::path::PathBuf::new(),
            ServiceType::UploadPack,
            TransportProtocol::Http,
        );
        mock.parse_capabilities("report-status-v2 side-band-64k object-format=sha10000");
        assert_eq!(
            mock.capabilities,
            std::collections::HashSet::from([Capability::ReportStatusv2, Capability::SideBand64k])
        );
    }

    #[test]
    pub fn receive_pack_advertises_only_supported_baseline_capabilities() {
        let caps = advertised_capabilities(ServiceType::ReceivePack, HashKind::Sha1);
        let tokens = caps.split_whitespace().collect::<Vec<_>>();

        assert!(tokens.contains(&"report-status"));
        assert!(tokens.contains(&"side-band-64k"));
        assert!(tokens.contains(&"ofs-delta"));
        assert!(tokens.contains(&"agent=mega/0.1.0"));
        assert!(tokens.contains(&"delete-refs"));
        assert!(!tokens.contains(&"report-status-v2"));
        assert!(!tokens.contains(&"quiet"));
        assert!(!tokens.contains(&"atomic"));
        assert!(!tokens.contains(&"no-thin"));
    }

    #[test]
    pub fn upload_pack_does_not_advertise_unimplemented_include_tag() {
        let caps = advertised_capabilities(ServiceType::UploadPack, HashKind::Sha1);
        let tokens = caps.split_whitespace().collect::<Vec<_>>();

        assert!(tokens.contains(&"multi_ack_detailed"));
        assert!(tokens.contains(&"no-done"));
        assert!(tokens.contains(&"side-band-64k"));
        assert!(!tokens.contains(&"include-tag"));
    }

    #[test]
    pub fn build_side_band_format_wraps_payload_when_side_band_64k_enabled() {
        let mut session = SmartSession::new(
            std::path::PathBuf::new(),
            ServiceType::ReceivePack,
            TransportProtocol::Http,
        );
        session.capabilities.insert(Capability::SideBand64k);

        let payload = BytesMut::from(&b"unpack ok\n"[..]);
        let length = payload.len();
        let framed = session.build_side_band_format(payload.clone(), length);

        let expected_total = 4 + 1 + payload.len();
        let header = format!("{expected_total:04x}");
        let mut expected = BytesMut::new();
        expected.extend_from_slice(header.as_bytes());
        expected.put_u8(0x01);
        expected.extend_from_slice(&payload);
        assert_eq!(&framed[..], &expected[..]);
    }

    #[test]
    pub fn build_side_band_format_passthrough_when_capability_absent() {
        let session = SmartSession::new(
            std::path::PathBuf::new(),
            ServiceType::ReceivePack,
            TransportProtocol::Http,
        );

        let payload = BytesMut::from(&b"unpack ok\n"[..]);
        let framed = session.build_side_band_format(payload.clone(), payload.len());
        assert_eq!(&framed[..], &payload[..]);
    }

    #[test]
    pub fn receive_pack_report_progress_frame_precedes_report_status() {
        let mut session = SmartSession::new(
            std::path::PathBuf::new(),
            ServiceType::ReceivePack,
            TransportProtocol::Http,
        );
        session.capabilities.insert(Capability::SideBand64k);

        let report_status = BytesMut::from(&b"unpack ok\n"[..]);
        let framed =
            session.build_receive_pack_report(report_status, Some("no new commits".to_string()));

        // channel-2 progress frame ("no new commits\n" = 15 bytes payload):
        // header + channel byte + payload = 20 = 0x14; git renders it as
        // `remote: no new commits`.
        let mut expected = BytesMut::new();
        expected.extend_from_slice(b"0014");
        expected.put_u8(0x02);
        expected.extend_from_slice(b"no new commits\n");
        // channel-1 report-status frame: 10 payload + 4 flush = 14, +5 = 19 = 0x13.
        expected.extend_from_slice(b"0013");
        expected.put_u8(0x01);
        expected.extend_from_slice(b"unpack ok\n");
        expected.extend_from_slice(PKT_LINE_END_MARKER);
        // final flush packet.
        expected.extend_from_slice(PKT_LINE_END_MARKER);
        assert_eq!(&framed[..], &expected[..]);
    }

    #[test]
    pub fn receive_pack_report_omits_progress_frame_without_side_band() {
        let session = SmartSession::new(
            std::path::PathBuf::new(),
            ServiceType::ReceivePack,
            TransportProtocol::Http,
        );

        let report_status = BytesMut::from(&b"unpack ok\n"[..]);
        let framed =
            session.build_receive_pack_report(report_status, Some("no new commits".to_string()));

        // No negotiated side-band capability: no channel byte may hit the
        // stream — the report payload passes through between flush packets.
        let mut expected = BytesMut::from(&b"unpack ok\n"[..]);
        expected.extend_from_slice(PKT_LINE_END_MARKER);
        expected.extend_from_slice(PKT_LINE_END_MARKER);
        assert_eq!(&framed[..], &expected[..]);
        assert!(!framed.contains(&0x02));
    }

    #[test]
    pub fn receive_pack_report_without_notice_emits_only_report_status() {
        let mut session = SmartSession::new(
            std::path::PathBuf::new(),
            ServiceType::ReceivePack,
            TransportProtocol::Http,
        );
        session.capabilities.insert(Capability::SideBand64k);

        let report_status = BytesMut::from(&b"unpack ok\n"[..]);
        let framed = session.build_receive_pack_report(report_status, None);

        let mut expected = BytesMut::new();
        expected.extend_from_slice(b"0013");
        expected.put_u8(0x01);
        expected.extend_from_slice(b"unpack ok\n");
        expected.extend_from_slice(PKT_LINE_END_MARKER);
        expected.extend_from_slice(PKT_LINE_END_MARKER);
        assert_eq!(&framed[..], &expected[..]);
        assert!(!framed.contains(&0x02));
    }

    #[test]
    pub fn parse_capabilities_recognizes_ofs_delta() {
        let mut session = SmartSession::new(
            std::path::PathBuf::new(),
            ServiceType::ReceivePack,
            TransportProtocol::Http,
        );
        session.parse_capabilities("report-status ofs-delta side-band-64k");
        assert!(session.capabilities.contains(&Capability::OfsDelta));
        assert!(session.capabilities.contains(&Capability::ReportStatus));
        assert!(session.capabilities.contains(&Capability::SideBand64k));
    }

    #[test]
    pub fn parse_capabilities_recognizes_shallow_depth_extensions() {
        let mut session = SmartSession::new(
            std::path::PathBuf::new(),
            ServiceType::UploadPack,
            TransportProtocol::Http,
        );
        session.parse_capabilities("shallow deepen-since deepen-not multi_ack_detailed");

        assert!(session.capabilities.contains(&Capability::Shallow));
        assert!(session.capabilities.contains(&Capability::DeepenSince));
        assert!(session.capabilities.contains(&Capability::DeepenNot));
        assert!(session.capabilities.contains(&Capability::MultiAckDetailed));
    }

    #[test]
    pub fn b3_02_capability_uses_injected_sha1_kind() {
        for service in [ServiceType::UploadPack, ServiceType::ReceivePack] {
            let caps = advertised_capabilities(service, HashKind::Sha1);
            let tokens = caps.split_whitespace().collect::<Vec<_>>();
            assert!(
                tokens.contains(&"object-format=sha1"),
                "injected Sha1 must advertise object-format=sha1 ({service:?}): {caps}"
            );
            assert!(
                !tokens
                    .iter()
                    .any(|t| *t == "object-format=sha256" || *t == "object-format=blake3"),
                "Sha1 advertisement must not leak another object-format ({service:?}): {caps}"
            );
        }

        let sha256 = advertised_capabilities(ServiceType::UploadPack, HashKind::Sha256);
        assert!(
            sha256.contains("object-format=sha256"),
            "injected Sha256 must drive capability construction: {sha256}"
        );
        assert!(
            !sha256.contains("object-format=sha1"),
            "injected Sha256 must not fall back to a sha1 literal: {sha256}"
        );

        let session = SmartSession::new(
            std::path::PathBuf::new(),
            ServiceType::UploadPack,
            TransportProtocol::Http,
        )
        .with_hash_kind(HashKind::Sha256);
        assert_eq!(session.hash_kind, HashKind::Sha256);
        assert_eq!(session.hash_kind.as_str(), "sha256");

        let v2_sha1 = crate::ceres::protocol::v2::build_v2_capability_advertisement(HashKind::Sha1);
        let v2_sha1 = String::from_utf8_lossy(&v2_sha1);
        assert!(
            v2_sha1.contains("object-format=sha1"),
            "v2 Sha1 advertisement: {v2_sha1}"
        );
        let v2_sha256 =
            crate::ceres::protocol::v2::build_v2_capability_advertisement(HashKind::Sha256);
        let v2_sha256 = String::from_utf8_lossy(&v2_sha256);
        assert!(
            v2_sha256.contains("object-format=sha256"),
            "v2 injected kind must not stay a sha1 literal: {v2_sha256}"
        );
        assert!(
            !v2_sha256.contains("object-format=sha1"),
            "v2 Sha256 advertisement must not include sha1: {v2_sha256}"
        );
    }

    #[test]
    pub fn set_authenticated_user_populates_auth_context_for_commit_binding() {
        let mut session = SmartSession::new(
            std::path::PathBuf::new(),
            ServiceType::ReceivePack,
            TransportProtocol::Ssh,
        );
        assert!(session.auth.authenticated_user.is_none());

        session.set_authenticated_user("alice".to_string());

        assert_eq!(
            session
                .auth
                .authenticated_user
                .as_ref()
                .map(|u| u.username.as_str()),
            Some("alice")
        );
        assert_eq!(session.auth.username.as_deref(), Some("alice"));
    }

    #[test]
    pub fn upload_pack_advertises_shallow_capability() {
        let caps = advertised_capabilities(ServiceType::UploadPack, HashKind::Sha1);
        let tokens = caps.split_whitespace().collect::<Vec<_>>();
        assert!(tokens.contains(&"shallow"));
    }

    #[test]
    pub fn receive_pack_does_not_advertise_shallow() {
        let caps = advertised_capabilities(ServiceType::ReceivePack, HashKind::Sha1);
        let tokens = caps.split_whitespace().collect::<Vec<_>>();
        assert!(!tokens.contains(&"shallow"));
    }

    #[test]
    pub fn parse_capabilities_recognizes_shallow() {
        let mut session = SmartSession::new(
            std::path::PathBuf::new(),
            ServiceType::UploadPack,
            TransportProtocol::Http,
        );
        session.parse_capabilities("shallow multi_ack_detailed");
        assert!(session.capabilities.contains(&Capability::Shallow));
        assert!(session.capabilities.contains(&Capability::MultiAckDetailed));
    }

    #[test]
    pub fn parse_capabilities_recognizes_deepen_since_and_deepen_not() {
        let mut session = SmartSession::new(
            std::path::PathBuf::new(),
            ServiceType::UploadPack,
            TransportProtocol::Http,
        );
        session.parse_capabilities("deepen-since deepen-not");
        assert!(session.capabilities.contains(&Capability::DeepenSince));
        assert!(session.capabilities.contains(&Capability::DeepenNot));
    }

    #[test]
    pub fn is_delete_only_push_detects_pure_delete_vs_mixed() {
        fn delete_cmd() -> RefCommand {
            RefCommand {
                ref_name: String::from("refs/heads/old"),
                old_id: String::from("27dd8d4cf39f3868c6eee38b601bc9e9939304f5"),
                new_id: String::from("0000000000000000000000000000000000000000"),
                status: String::from("ok"),
                error_msg: String::new(),
                command_type: CommandType::Delete,
                ref_type: RefTypeEnum::Branch,
                default_branch: false,
            }
        }
        let create = RefCommand {
            ref_name: String::from("refs/heads/new"),
            old_id: String::from("0000000000000000000000000000000000000000"),
            new_id: String::from("27dd8d4cf39f3868c6eee38b601bc9e9939304f5"),
            status: String::from("ok"),
            error_msg: String::new(),
            command_type: CommandType::Create,
            ref_type: RefTypeEnum::Branch,
            default_branch: false,
        };

        assert!(SmartSession::is_delete_only_push(&[delete_cmd()]));
        assert!(SmartSession::is_delete_only_push(&[
            delete_cmd(),
            delete_cmd()
        ]));
        assert!(!SmartSession::is_delete_only_push(&[delete_cmd(), create]));
        assert!(!SmartSession::is_delete_only_push(&[]));
    }

    async fn git_push_with_retry(repo_path: &std::path::Path) -> anyhow::Result<()> {
        let max_retries = 5;

        for attempt in 1..=max_retries {
            let status = Command::new("git")
                .args(["push", "origin", "main"])
                .current_dir(repo_path)
                .status()?;

            if status.success() {
                return Ok(());
            }

            eprintln!(
                "git push failed (attempt {}/{}) — retrying...",
                attempt, max_retries
            );

            // 1s, 2s, 4s, 8s...
            let delay = Duration::from_secs(1 << (attempt - 1));
            sleep(delay).await;
        }

        Err(anyhow::anyhow!("git push failed after retries"))
    }

    async fn init_and_push(repo_name: &str) -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let repo_path = tmp.path().join(repo_name);
        std::fs::create_dir_all(&repo_path)?;

        let remote_url = format!("http://localhost:8000/third-party/rust/src/{}", repo_name);

        // 1. git init
        Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(&repo_path)
            .status()?;

        // 2. add a file
        std::fs::write(repo_path.join("README.md"), format!("# {}\n", repo_name))?;

        // 3. git add .
        Command::new("git")
            .args(["add", "."])
            .current_dir(&repo_path)
            .status()?;

        Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(&repo_path)
            .status()?;

        // 4. git commit
        Command::new("git")
            .args(["commit", "-m", "init commit"])
            .current_dir(&repo_path)
            .status()?;

        // 5. git remote add
        Command::new("git")
            .args(["remote", "add", "origin", &remote_url])
            .current_dir(&repo_path)
            .status()?;

        // 6. git push
        git_push_with_retry(&repo_path).await?;

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 32)]
    #[ignore]
    async fn test_dynamic_repos_push() -> anyhow::Result<()> {
        let repo_count = 64;
        let repo_names: Vec<String> = (1..=repo_count).map(|i| format!("repo{}", i)).collect();

        // push
        let tasks = repo_names.into_iter().map(|name| {
            task::spawn(async move {
                init_and_push(&name).await.unwrap();
            })
        });

        future::join_all(tasks).await;

        Ok(())
    }
}
