use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use chrono::Utc;
use hmac::{Hmac, KeyInit, Mac};
use idgenerator::IdInstance;
use reqwest::redirect::Policy;
use ring::{
    aead::{self, Aad, LessSafeKey, Nonce, UnboundKey},
    rand::{SecureRandom, SystemRandom},
};
use sea_orm::{ActiveEnum, ConnectionTrait, FromQueryResult, TransactionTrait};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::{
    net::lookup_host,
    task::JoinSet,
    time::{Instant, timeout, timeout_at},
};
use tokio_util::sync::CancellationToken;
use url::{Host, Url};

use crate::{
    callisto::{
        cl_webhook_outbox, mega_cl,
        sea_orm_active_enums::{MergeStatusEnum, WebhookEventTypeEnum},
    },
    common::errors::MegaError,
    config::{
        ParsedCidr, WEBHOOK_DELIVERY_LEASE_MARGIN_SECS, WEBHOOK_DELIVERY_MAX_MAX_IN_FLIGHT,
        WEBHOOK_DELIVERY_MAX_REDRIVE_AGE_SECS, WEBHOOK_DELIVERY_MAX_TOTAL_DEADLINE_SECS,
        WEBHOOK_DELIVERY_MIN_MAX_IN_FLIGHT, WEBHOOK_DELIVERY_MIN_REDRIVE_AGE_SECS,
        WEBHOOK_DELIVERY_MIN_TOTAL_DEADLINE_SECS, WebhookDeliveryConfig,
    },
    jupiter::storage::{base_storage::StorageConnector, webhook_storage::WebhookStorage},
};

const WEBHOOK_SECRET_ENC_PREFIX: &str = "enc:v1:";
const WEBHOOK_SECRET_ENC_KEY_ENV: &str = "MEGA_WEBHOOK_SECRET_ENC_KEY";
const WEBHOOK_SECRET_KEY_LEN: usize = 32;
const WEBHOOK_SECRET_NONCE_LEN: usize = 12;
const WEBHOOK_DELIVERY_MAX_ATTEMPTS: i32 = 3;
const WEBHOOK_DELIVERY_BACKOFF_BASE_SECS: u64 = 2;
const WEBHOOK_SINGLE_REQUEST_MAX_TIMEOUT_SECS: u64 = 10;
const WEBHOOK_OUTBOX_MAX_ROUNDS: i32 = 10;
const WEBHOOK_OUTBOX_POLL_IDLE_MILLIS: u64 = 500;
const WEBHOOK_OUTBOX_MAX_REDRIVE_LIMIT: usize = 100;

/// Canonical V2 HMAC-SHA256 signature envelope version (`ADR-CLB-02`).
pub const CANONICAL_V2_VERSION: &str = "orion-mega-v2";

pub type WebhookEvent = WebhookEventTypeEnum;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedriveOutcome {
    pub redriven_count: u64,
    pub skipped_expired_count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct WebhookPayload {
    pub mega_version: String,
    pub event: String,
    pub timestamp: i64,
    pub cl: ClPayload,
    pub repository: RepositoryPayload,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepositoryPayload {
    pub path: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthorPayload {
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClPayload {
    pub id: i64,
    pub link: String,
    pub title: String,
    pub author: AuthorPayload,
    pub status: String,
    pub path: String,
    pub base_branch: String,
    pub head_commit: String,
    pub head_revision: i64,
}

impl From<&mega_cl::Model> for ClPayload {
    fn from(model: &mega_cl::Model) -> Self {
        Self {
            id: model.id,
            link: model.link.clone(),
            title: model.title.clone(),
            author: AuthorPayload {
                name: model.username.clone(),
            },
            status: merge_status_to_str(&model.status).to_string(),
            path: model.path.clone(),
            base_branch: model.base_branch.clone(),
            head_commit: model.to_hash.clone(),
            head_revision: model.head_revision,
        }
    }
}

/// Validates `[webhook_delivery]` hard bounds fail-closed (`CLB-12` / `CLB-18`).
pub fn validate_delivery_config(
    config: &WebhookDeliveryConfig,
) -> Result<Vec<ParsedCidr>, MegaError> {
    if !(WEBHOOK_DELIVERY_MIN_REDRIVE_AGE_SECS..=WEBHOOK_DELIVERY_MAX_REDRIVE_AGE_SECS)
        .contains(&config.max_redrive_age_secs)
    {
        return Err(MegaError::Other(format!(
            "webhook_delivery.max_redrive_age_secs must be in [{}, {}] seconds, got {}",
            WEBHOOK_DELIVERY_MIN_REDRIVE_AGE_SECS,
            WEBHOOK_DELIVERY_MAX_REDRIVE_AGE_SECS,
            config.max_redrive_age_secs
        )));
    }
    if !(WEBHOOK_DELIVERY_MIN_TOTAL_DEADLINE_SECS..=WEBHOOK_DELIVERY_MAX_TOTAL_DEADLINE_SECS)
        .contains(&config.total_deadline_secs)
    {
        return Err(MegaError::Other(format!(
            "webhook_delivery.total_deadline_secs must be in [{}, {}] seconds, got {}",
            WEBHOOK_DELIVERY_MIN_TOTAL_DEADLINE_SECS,
            WEBHOOK_DELIVERY_MAX_TOTAL_DEADLINE_SECS,
            config.total_deadline_secs
        )));
    }
    const WEBHOOK_OUTBOX_MAX_LEASE_SECS: u64 = 86_400;
    let min_lease_secs = config.total_deadline_secs + WEBHOOK_DELIVERY_LEASE_MARGIN_SECS;
    if config.outbox_lease_secs < min_lease_secs
        || config.outbox_lease_secs > WEBHOOK_OUTBOX_MAX_LEASE_SECS
    {
        return Err(MegaError::Other(format!(
            "webhook_delivery.outbox_lease_secs ({}) must be in [total_deadline_secs + {} ({}), {}] seconds",
            config.outbox_lease_secs,
            WEBHOOK_DELIVERY_LEASE_MARGIN_SECS,
            min_lease_secs,
            WEBHOOK_OUTBOX_MAX_LEASE_SECS
        )));
    }
    if !(WEBHOOK_DELIVERY_MIN_MAX_IN_FLIGHT..=WEBHOOK_DELIVERY_MAX_MAX_IN_FLIGHT)
        .contains(&config.max_in_flight)
    {
        return Err(MegaError::Other(format!(
            "webhook_delivery.max_in_flight must be in [{}, {}], got {}",
            WEBHOOK_DELIVERY_MIN_MAX_IN_FLIGHT,
            WEBHOOK_DELIVERY_MAX_MAX_IN_FLIGHT,
            config.max_in_flight
        )));
    }
    let mut parsed_cidrs = Vec::with_capacity(config.allowed_private_cidrs.len());
    for raw_cidr in &config.allowed_private_cidrs {
        let parsed = ParsedCidr::parse(raw_cidr)?;
        if parsed.is_wildcard() {
            return Err(MegaError::Other(format!(
                "webhook_delivery.allowed_private_cidrs must not contain wildcard CIDR '{raw_cidr}'"
            )));
        }
        if parsed.overlaps_link_local_metadata() {
            return Err(MegaError::Other(format!(
                "webhook_delivery.allowed_private_cidrs must not overlap cloud metadata range 169.254.0.0/16: '{raw_cidr}'"
            )));
        }
        parsed_cidrs.push(parsed);
    }
    Ok(parsed_cidrs)
}

#[derive(Clone)]
pub struct WebhookService {
    storage: WebhookStorage,
    delivery_config: WebhookDeliveryConfig,
    allowed_private_cidrs: Vec<ParsedCidr>,
    outbox_max_rounds: i32,
}

impl WebhookService {
    pub fn new(
        storage: WebhookStorage,
        delivery_config: WebhookDeliveryConfig,
    ) -> Result<Self, MegaError> {
        let allowed_private_cidrs = validate_delivery_config(&delivery_config)?;
        Ok(Self {
            storage,
            delivery_config,
            allowed_private_cidrs,
            outbox_max_rounds: WEBHOOK_OUTBOX_MAX_ROUNDS,
        })
    }

    pub fn mock(storage: WebhookStorage) -> Self {
        Self {
            storage,
            delivery_config: WebhookDeliveryConfig::default(),
            allowed_private_cidrs: Vec::new(),
            outbox_max_rounds: WEBHOOK_OUTBOX_MAX_ROUNDS,
        }
    }

    #[cfg(test)]
    fn with_outbox_max_rounds(mut self, max_rounds: i32) -> Self {
        self.outbox_max_rounds = max_rounds.max(1);
        self
    }

    pub fn delivery_config(&self) -> &WebhookDeliveryConfig {
        &self.delivery_config
    }

    /// Spawns the durable `cl_webhook_outbox` background dispatcher bound to
    /// `shutdown` (`CLB-18`). Maintains a continuous bounded concurrency pool
    /// of up to `max_in_flight` rows so a slow row never stalls available slots
    /// or blocks shutdown.
    pub fn spawn_outbox_worker(&self, shutdown: CancellationToken) -> tokio::task::JoinHandle<()> {
        let svc = self.clone();
        let lease_owner = format!("mega2-webhook-{}", uuid::Uuid::new_v4());
        tokio::spawn(async move {
            svc.run_outbox_loop(lease_owner, shutdown).await;
        })
    }

    async fn run_outbox_loop(&self, lease_owner: String, shutdown: CancellationToken) {
        let idle = Duration::from_millis(WEBHOOK_OUTBOX_POLL_IDLE_MILLIS);
        let max_in_flight = self.delivery_config.max_in_flight;
        let mut in_flight: JoinSet<()> = JoinSet::new();

        loop {
            if shutdown.is_cancelled() {
                break;
            }

            let available = max_in_flight.saturating_sub(in_flight.len());
            let mut claimed_any = false;
            if available > 0 {
                // Do NOT wrap `claim_outbox_batch_limit_cancellable` in an outer
                // `select!`: cancellation is handled inside on every pre-COMMIT
                // step, while `COMMIT` itself runs uninterrupted and releases
                // any freshly committed rows if `shutdown` fired mid-COMMIT.
                let claim_step = self
                    .claim_outbox_batch_limit_cancellable(&lease_owner, available, &shutdown)
                    .await;
                if shutdown.is_cancelled() {
                    if let Ok(rows) = claim_step {
                        self.release_unstarted_leases(&rows, &lease_owner).await;
                    }
                    break;
                }
                match claim_step {
                    Ok(rows) if !rows.is_empty() => {
                        claimed_any = true;
                        for row in rows {
                            let svc = self.clone();
                            let owner = lease_owner.clone();
                            let task_shutdown = shutdown.clone();
                            in_flight.spawn(async move {
                                svc.deliver_outbox_row(row, &owner, task_shutdown).await;
                            });
                        }
                    }
                    Ok(_) => {}
                    Err(err) => {
                        tracing::warn!(
                            lease_owner = %lease_owner,
                            error = %err,
                            "cl_webhook_outbox poll cycle failed"
                        );
                    }
                }
            }

            // Reap any already-completed tasks immediately without sleeping.
            while let Some(res) = in_flight.try_join_next() {
                if let Err(err) = res {
                    tracing::error!(error = %err, "cl_webhook_outbox worker task panicked");
                }
            }

            if claimed_any && in_flight.len() < max_in_flight {
                continue;
            }

            if in_flight.len() >= max_in_flight {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    Some(res) = in_flight.join_next() => {
                        if let Err(err) = res {
                            tracing::error!(error = %err, "cl_webhook_outbox worker task panicked");
                        }
                    }
                }
            } else if !in_flight.is_empty() {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    Some(res) = in_flight.join_next() => {
                        if let Err(err) = res {
                            tracing::error!(error = %err, "cl_webhook_outbox worker task panicked");
                        }
                    }
                    _ = tokio::time::sleep(idle) => {}
                }
            } else {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = tokio::time::sleep(idle) => {}
                }
            }
        }

        // Shutdown requested: tasks observe `shutdown.cancelled()` and exit promptly;
        // abort any stragglers after a short grace window so shutdown never hangs.
        let _ = timeout(Duration::from_secs(2), async {
            while in_flight.join_next().await.is_some() {}
        })
        .await;
        in_flight.abort_all();
    }

    /// Claims up to `batch_size = max_in_flight` due/expired `cl_webhook_outbox`
    /// rows using PostgreSQL DB time (`NOW() AT TIME ZONE 'UTC'`) and
    /// `FOR UPDATE SKIP LOCKED`, atomically bumping `lease_epoch` and setting
    /// `lease_expires_at = db_now + outbox_lease_secs`.
    pub async fn claim_outbox_batch(
        &self,
        lease_owner: &str,
    ) -> Result<Vec<cl_webhook_outbox::Model>, MegaError> {
        let never_cancel = CancellationToken::new();
        self.claim_outbox_batch_limit_cancellable(
            lease_owner,
            self.delivery_config.max_in_flight,
            &never_cancel,
        )
        .await
    }

    /// Runs the claim inside an explicit PostgreSQL transaction bounded by
    /// `SET LOCAL lock_timeout = '500ms'` and `SET LOCAL statement_timeout = '5000ms'`.
    /// All pre-`COMMIT` steps race against `shutdown.cancelled()` so an aborted
    /// claim rolls back cleanly; `COMMIT` itself runs uninterrupted, and if
    /// `shutdown` fires during the `COMMIT` round-trip, the freshly claimed
    /// leases are immediately released back to `pending` under `(lease_owner,
    /// lease_epoch)` fencing so zero orphaned leases can remain.
    async fn claim_outbox_batch_limit_cancellable(
        &self,
        lease_owner: &str,
        limit: usize,
        shutdown: &CancellationToken,
    ) -> Result<Vec<cl_webhook_outbox::Model>, MegaError> {
        let clamped_limit = limit.min(self.delivery_config.max_in_flight) as i64;
        if clamped_limit <= 0 || shutdown.is_cancelled() {
            return Ok(Vec::new());
        }
        let lease_secs = self.delivery_config.outbox_lease_secs as i64;

        let txn = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return Ok(Vec::new()),
            res = timeout(Duration::from_secs(5), self.storage.get_connection().begin()) => {
                match res {
                    Ok(Ok(txn)) => txn,
                    Ok(Err(err)) => return Err(err.into()),
                    Err(_) => return Err(MegaError::Other("cl_webhook_outbox claim begin timed out".to_owned())),
                }
            }
        };

        let set_res = tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                drop(txn);
                return Ok(Vec::new());
            }
            res = txn.execute_unprepared(
                "SET LOCAL lock_timeout = '500ms'; SET LOCAL statement_timeout = '5000ms'",
            ) => res,
        };
        set_res?;

        let stmt = sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r#"
            WITH candidate AS (
                SELECT id
                  FROM cl_webhook_outbox
                 WHERE (state = 'pending' AND next_retry_at <= (clock_timestamp() AT TIME ZONE 'UTC'))
                    OR (state = 'leased' AND lease_expires_at <= (clock_timestamp() AT TIME ZONE 'UTC'))
                 ORDER BY next_retry_at ASC, id ASC
                 LIMIT $1
                 FOR UPDATE SKIP LOCKED
            )
            UPDATE cl_webhook_outbox o
               SET state = 'leased',
                   lease_owner = $2,
                   lease_epoch = o.lease_epoch + 1,
                   lease_expires_at = (clock_timestamp() AT TIME ZONE 'UTC') + ($3::BIGINT * INTERVAL '1 second'),
                   updated_at = (clock_timestamp() AT TIME ZONE 'UTC')
              FROM candidate c
             WHERE o.id = c.id
            RETURNING o.id, o.event_id, o.event_type, o.cl_link, o.cl_path,
                      o.head_revision, o.payload_json, o.state, o.lease_owner,
                      o.lease_epoch, o.lease_expires_at, o.attempt_count,
                      o.next_retry_at, o.last_error, o.created_at, o.updated_at
            "#,
            [
                clamped_limit.into(),
                lease_owner.to_owned().into(),
                lease_secs.into(),
            ],
        );

        let rows_res = tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                drop(txn);
                return Ok(Vec::new());
            }
            res = cl_webhook_outbox::Model::find_by_statement(stmt).all(&txn) => res,
        };
        let rows = rows_res?;

        if shutdown.is_cancelled() {
            let _ = timeout(Duration::from_secs(2), txn.rollback()).await;
            return Ok(Vec::new());
        }

        // Run `COMMIT` under a tight client-side bound (not cancelled mid-flight
        // by shutdown) so a stalled DB socket cannot hang shutdown, while any
        // ambiguous commit error/timeout or post-commit shutdown triggers a
        // single bulk `(lease_owner, id, lease_epoch)`-fenced release query.
        let commit_res = timeout(Duration::from_secs(2), txn.commit()).await;
        match commit_res {
            Ok(Ok(())) => {
                if shutdown.is_cancelled() {
                    self.release_unstarted_leases(&rows, lease_owner).await;
                    return Ok(Vec::new());
                }
                Ok(rows)
            }
            Ok(Err(err)) => {
                // PostgreSQL may have already committed before the connection
                // error; run the fenced bulk release so no lease is orphaned.
                self.release_unstarted_leases(&rows, lease_owner).await;
                Err(err.into())
            }
            Err(_) => {
                // Commit timed out on the client; attempt the fenced bulk
                // release in case the server committed the transaction.
                self.release_unstarted_leases(&rows, lease_owner).await;
                Err(MegaError::Other(
                    "cl_webhook_outbox claim commit timed out".to_owned(),
                ))
            }
        }
    }

    /// Best-effort single-round-trip fenced release of rows whose claim
    /// committed (or may have committed) concurrently with shutdown or a
    /// transport error before any delivery task was spawned. Uses a single
    /// `jsonb_to_recordset` statement bounded by a 2s client timeout +
    /// server-side `lock_timeout`/`statement_timeout` so releasing up to
    /// `max_in_flight` rows never adds per-row latency; if the database is
    /// unreachable, the rows naturally recover upon `lease_expires_at` expiry
    /// under `lease_epoch` fencing.
    async fn release_unstarted_leases(&self, rows: &[cl_webhook_outbox::Model], lease_owner: &str) {
        if rows.is_empty() {
            return;
        }
        let targets: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| serde_json::json!({ "id": r.id, "lease_epoch": r.lease_epoch }))
            .collect();
        let Ok(targets_json) = serde_json::to_string(&targets) else {
            return;
        };
        let release_fut = async {
            let txn = self.storage.get_connection().begin().await?;
            txn.execute_unprepared(
                "SET LOCAL lock_timeout = '500ms'; SET LOCAL statement_timeout = '2000ms'",
            )
            .await?;
            let stmt = sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r#"
                UPDATE cl_webhook_outbox o
                   SET state = 'pending',
                       lease_owner = NULL,
                       lease_expires_at = NULL,
                       updated_at = (clock_timestamp() AT TIME ZONE 'UTC')
                  FROM jsonb_to_recordset($1::jsonb) AS target(id BIGINT, lease_epoch BIGINT)
                 WHERE o.id = target.id
                   AND o.state = 'leased'
                   AND o.lease_owner = $2
                   AND o.lease_epoch = target.lease_epoch
                "#,
                [targets_json.into(), lease_owner.to_owned().into()],
            );
            txn.execute_raw(stmt).await?;
            txn.commit().await?;
            Ok::<(), sea_orm::DbErr>(())
        };
        match timeout(Duration::from_secs(2), release_fut).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                tracing::warn!(
                    lease_owner = %lease_owner,
                    row_count = rows.len(),
                    error = %err,
                    "failed to release unstarted outbox leases on shutdown; leases will expire naturally under lease_epoch fencing"
                );
            }
            Err(_) => {
                tracing::warn!(
                    lease_owner = %lease_owner,
                    row_count = rows.len(),
                    "timed out releasing unstarted outbox leases on shutdown; leases will expire naturally under lease_epoch fencing"
                );
            }
        }
    }

    /// `lease_epoch`-fenced transition to `delivered`: acquires the row lock
    /// `FOR UPDATE` (bounded by server-side `lock_timeout` / `statement_timeout`)
    /// first and THEN evaluates `clock_timestamp() AT TIME ZONE 'UTC'` so a
    /// statement delayed on a lock across lease expiry can never commit against
    /// a stale transaction-start timestamp or pin a worker slot indefinitely.
    pub async fn mark_outbox_delivered(
        &self,
        id: i64,
        lease_owner: &str,
        lease_epoch: i64,
    ) -> Result<bool, MegaError> {
        let txn = self.storage.get_connection().begin().await?;
        txn.execute_unprepared(
            "SET LOCAL lock_timeout = '2000ms'; SET LOCAL statement_timeout = '5000ms'",
        )
        .await?;
        let locked = txn
            .execute_raw(sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT 1 FROM cl_webhook_outbox WHERE id = $1 FOR UPDATE",
                [id.into()],
            ))
            .await?;
        if locked.rows_affected() == 0 {
            let _ = txn.rollback().await;
            return Ok(false);
        }
        let res = txn
            .execute_raw(sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r#"
                UPDATE cl_webhook_outbox
                   SET state = 'delivered',
                       lease_owner = NULL,
                       lease_expires_at = NULL,
                       last_error = NULL,
                       updated_at = (clock_timestamp() AT TIME ZONE 'UTC')
                 WHERE id = $1
                   AND state = 'leased'
                   AND lease_owner = $2
                   AND lease_epoch = $3
                   AND lease_expires_at > (clock_timestamp() AT TIME ZONE 'UTC')
                "#,
                [id.into(), lease_owner.to_owned().into(), lease_epoch.into()],
            ))
            .await?;
        txn.commit().await?;
        Ok(res.rows_affected() == 1)
    }

    /// `lease_epoch`-fenced transition back to `pending` with lock-first
    /// `clock_timestamp()` expiry verification and server-side lock timeout.
    pub async fn reschedule_outbox_retry(
        &self,
        id: i64,
        lease_owner: &str,
        lease_epoch: i64,
        backoff_secs: u64,
        last_error: &str,
    ) -> Result<bool, MegaError> {
        let txn = self.storage.get_connection().begin().await?;
        txn.execute_unprepared(
            "SET LOCAL lock_timeout = '2000ms'; SET LOCAL statement_timeout = '5000ms'",
        )
        .await?;
        let locked = txn
            .execute_raw(sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT 1 FROM cl_webhook_outbox WHERE id = $1 FOR UPDATE",
                [id.into()],
            ))
            .await?;
        if locked.rows_affected() == 0 {
            let _ = txn.rollback().await;
            return Ok(false);
        }
        let res = txn
            .execute_raw(sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r#"
                UPDATE cl_webhook_outbox
                   SET state = 'pending',
                       lease_owner = NULL,
                       lease_expires_at = NULL,
                       attempt_count = attempt_count + 1,
                       next_retry_at = (clock_timestamp() AT TIME ZONE 'UTC') + ($1::BIGINT * INTERVAL '1 second'),
                       last_error = $2,
                       updated_at = (clock_timestamp() AT TIME ZONE 'UTC')
                 WHERE id = $3
                   AND state = 'leased'
                   AND lease_owner = $4
                   AND lease_epoch = $5
                   AND lease_expires_at > (clock_timestamp() AT TIME ZONE 'UTC')
                "#,
                [
                    (backoff_secs as i64).into(),
                    last_error.to_owned().into(),
                    id.into(),
                    lease_owner.to_owned().into(),
                    lease_epoch.into(),
                ],
            ))
            .await?;
        txn.commit().await?;
        Ok(res.rows_affected() == 1)
    }

    /// `lease_epoch`-fenced transition to `dead_letter` with lock-first
    /// `clock_timestamp()` expiry verification, server-side lock timeout, and
    /// structured alert.
    pub async fn mark_outbox_dead_letter(
        &self,
        id: i64,
        lease_owner: &str,
        lease_epoch: i64,
        last_error: &str,
    ) -> Result<bool, MegaError> {
        let txn = self.storage.get_connection().begin().await?;
        txn.execute_unprepared(
            "SET LOCAL lock_timeout = '2000ms'; SET LOCAL statement_timeout = '5000ms'",
        )
        .await?;
        let locked = txn
            .execute_raw(sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT 1 FROM cl_webhook_outbox WHERE id = $1 FOR UPDATE",
                [id.into()],
            ))
            .await?;
        if locked.rows_affected() == 0 {
            let _ = txn.rollback().await;
            return Ok(false);
        }
        let res = txn
            .execute_raw(sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r#"
                UPDATE cl_webhook_outbox
                   SET state = 'dead_letter',
                       lease_owner = NULL,
                       lease_expires_at = NULL,
                       attempt_count = attempt_count + 1,
                       last_error = $1,
                       updated_at = (clock_timestamp() AT TIME ZONE 'UTC')
                 WHERE id = $2
                   AND state = 'leased'
                   AND lease_owner = $3
                   AND lease_epoch = $4
                   AND lease_expires_at > (clock_timestamp() AT TIME ZONE 'UTC')
                "#,
                [
                    last_error.to_owned().into(),
                    id.into(),
                    lease_owner.to_owned().into(),
                    lease_epoch.into(),
                ],
            ))
            .await?;
        txn.commit().await?;
        let transitioned = res.rows_affected() == 1;
        if transitioned {
            tracing::error!(
                outbox_id = id,
                lease_owner = %lease_owner,
                lease_epoch,
                last_error = %last_error,
                "CL webhook outbox delivery exhausted retries and moved to dead_letter"
            );
        }
        Ok(transitioned)
    }

    /// Bounded recovery of `dead_letter` `cl_webhook_outbox` rows (`limit <= 100`,
    /// `max_redrive_age <= webhook_delivery.max_redrive_age_secs`) evaluated
    /// against PostgreSQL DB time (`NOW() AT TIME ZONE 'UTC'`).
    pub async fn redrive_dead_letter_cl_webhooks(
        &self,
        limit: usize,
        max_redrive_age: Option<Duration>,
    ) -> Result<RedriveOutcome, MegaError> {
        if limit == 0 || limit > WEBHOOK_OUTBOX_MAX_REDRIVE_LIMIT {
            return Err(MegaError::Other(format!(
                "redrive limit must be in [1, {WEBHOOK_OUTBOX_MAX_REDRIVE_LIMIT}], got {limit}"
            )));
        }
        let configured_max_secs = self.delivery_config.max_redrive_age_secs;
        let effective_age_secs = match max_redrive_age {
            Some(dur) => {
                let secs = dur.as_secs();
                if secs < WEBHOOK_DELIVERY_MIN_REDRIVE_AGE_SECS || secs > configured_max_secs {
                    return Err(MegaError::Other(format!(
                        "max_redrive_age ({secs}s) must be in [{WEBHOOK_DELIVERY_MIN_REDRIVE_AGE_SECS}, {configured_max_secs}]"
                    )));
                }
                secs
            }
            None => configured_max_secs,
        };

        #[derive(FromQueryResult)]
        struct CountRow {
            cnt: i64,
        }

        let skipped_row = CountRow::find_by_statement(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r#"
            SELECT COUNT(*)::BIGINT AS cnt
              FROM cl_webhook_outbox
             WHERE state = 'dead_letter'
               AND created_at < ((clock_timestamp() AT TIME ZONE 'UTC') - ($1::BIGINT * INTERVAL '1 second'))
            "#,
            [(effective_age_secs as i64).into()],
        ))
        .one(self.storage.get_connection())
        .await?;
        let skipped_expired_count = skipped_row.map(|r| r.cnt.max(0) as u64).unwrap_or(0);

        let update_res = self
            .storage
            .get_connection()
            .execute_raw(sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r#"
                WITH candidate AS (
                    SELECT id
                      FROM cl_webhook_outbox
                     WHERE state = 'dead_letter'
                       AND created_at >= ((clock_timestamp() AT TIME ZONE 'UTC') - ($1::BIGINT * INTERVAL '1 second'))
                     ORDER BY created_at ASC, id ASC
                     LIMIT $2
                     FOR UPDATE SKIP LOCKED
                )
                UPDATE cl_webhook_outbox o
                   SET state = 'pending',
                       lease_owner = NULL,
                       lease_expires_at = NULL,
                       attempt_count = 0,
                       next_retry_at = (clock_timestamp() AT TIME ZONE 'UTC'),
                       updated_at = (clock_timestamp() AT TIME ZONE 'UTC')
                  FROM candidate c
                 WHERE o.id = c.id
                "#,
                [(effective_age_secs as i64).into(), (limit as i64).into()],
            ))
            .await?;

        Ok(RedriveOutcome {
            redriven_count: update_res.rows_affected(),
            skipped_expired_count,
        })
    }

    /// Claims up to `max_in_flight` rows and dispatches all of them concurrently
    /// via `JoinSet` (zero head-of-line blocking within the batch).
    pub async fn process_outbox_once(&self, lease_owner: &str) -> Result<usize, MegaError> {
        let batch = self.claim_outbox_batch(lease_owner).await?;
        let count = batch.len();
        if count == 0 {
            return Ok(0);
        }

        let shutdown = CancellationToken::new();
        let mut join_set = JoinSet::new();
        for row in batch {
            let svc = self.clone();
            let owner = lease_owner.to_owned();
            let sd = shutdown.clone();
            join_set.spawn(async move {
                svc.deliver_outbox_row(row, &owner, sd).await;
            });
        }
        while let Some(res) = join_set.join_next().await {
            if let Err(err) = res {
                tracing::error!(error = %err, "cl_webhook_outbox worker task panicked");
            }
        }
        Ok(count)
    }

    async fn deliver_outbox_row(
        &self,
        row: cl_webhook_outbox::Model,
        lease_owner: &str,
        shutdown: CancellationToken,
    ) {
        let round_deadline =
            Instant::now() + Duration::from_secs(self.delivery_config.total_deadline_secs);

        let Ok(event_type) = WebhookEventTypeEnum::try_from_value(&row.event_type) else {
            self.run_bounded_transition(
                &shutdown,
                row.id,
                lease_owner,
                "mark_dead_letter(invalid_event)",
                self.mark_outbox_dead_letter(
                    row.id,
                    lease_owner,
                    row.lease_epoch,
                    &format!("invalid event_type '{}'", row.event_type),
                ),
            )
            .await;
            return;
        };

        let webhooks_res = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            res = timeout_at(
                round_deadline,
                self.storage.find_matching_webhooks(event_type.clone(), &row.cl_path),
            ) => res,
        };
        let webhooks = match webhooks_res {
            Ok(Ok(w)) => w,
            Ok(Err(err)) => {
                let msg = format!("failed to load matching webhooks: {err}");
                self.run_bounded_failure(&shutdown, &row, lease_owner, &msg)
                    .await;
                return;
            }
            Err(_) => {
                let msg = format!(
                    "loading matching webhooks exceeded total_deadline_secs ({}s)",
                    self.delivery_config.total_deadline_secs
                );
                self.run_bounded_failure(&shutdown, &row, lease_owner, &msg)
                    .await;
                return;
            }
        };

        if webhooks.is_empty() {
            self.run_bounded_transition(
                &shutdown,
                row.id,
                lease_owner,
                "mark_delivered(no_subscribers)",
                self.mark_outbox_delivered(row.id, lease_owner, row.lease_epoch),
            )
            .await;
            return;
        }

        // Fan out to all matching subscribers concurrently under the shared
        // `round_deadline` so one slow/failing endpoint can never starve
        // healthy subscribers.
        let mut sub_tasks: JoinSet<Result<(), String>> = JoinSet::new();
        for webhook in webhooks {
            let svc = self.clone();
            let ev_type = event_type.clone();
            let ev_id = row.event_id.clone();
            let ev_type_str = row.event_type.clone();
            let payload_json = row.payload_json.clone();
            let sd = shutdown.clone();
            sub_tasks.spawn(async move {
                svc.deliver_to_single_subscriber(
                    webhook,
                    ev_type,
                    ev_id,
                    ev_type_str,
                    payload_json,
                    round_deadline,
                    sd,
                )
                .await
            });
        }

        let mut all_succeeded = true;
        let mut last_error: Option<String> = None;
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    sub_tasks.abort_all();
                    return;
                }
                next = sub_tasks.join_next() => {
                    match next {
                        Some(Ok(Ok(()))) => {}
                        Some(Ok(Err(err_msg))) => {
                            all_succeeded = false;
                            last_error = Some(err_msg);
                        }
                        Some(Err(join_err)) => {
                            all_succeeded = false;
                            last_error = Some(format!("subscriber task failed: {join_err}"));
                        }
                        None => break,
                    }
                }
            }
        }

        if shutdown.is_cancelled() {
            return;
        }

        if all_succeeded {
            self.run_bounded_transition(
                &shutdown,
                row.id,
                lease_owner,
                "mark_delivered",
                self.mark_outbox_delivered(row.id, lease_owner, row.lease_epoch),
            )
            .await;
        } else {
            let msg = last_error.unwrap_or_else(|| "webhook delivery failed".to_owned());
            self.run_bounded_failure(&shutdown, &row, lease_owner, &msg)
                .await;
        }
    }

    async fn run_bounded_transition<F>(
        &self,
        shutdown: &CancellationToken,
        outbox_id: i64,
        lease_owner: &str,
        op: &'static str,
        fut: F,
    ) where
        F: std::future::Future<Output = Result<bool, MegaError>>,
    {
        let outcome = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            res = timeout(Duration::from_secs(5), fut) => res,
        };
        match outcome {
            Ok(Ok(true)) => {}
            Ok(Ok(false)) => {
                tracing::warn!(
                    outbox_id,
                    lease_owner = %lease_owner,
                    op,
                    "outbox transition fenced out (lease lost or expired)"
                );
            }
            Ok(Err(err)) => {
                tracing::warn!(
                    outbox_id,
                    lease_owner = %lease_owner,
                    op,
                    error = %err,
                    "outbox transition failed (e.g. lock timeout)"
                );
            }
            Err(_) => {
                tracing::warn!(
                    outbox_id,
                    lease_owner = %lease_owner,
                    op,
                    "outbox transition timed out"
                );
            }
        }
    }

    async fn run_bounded_failure(
        &self,
        shutdown: &CancellationToken,
        row: &cl_webhook_outbox::Model,
        lease_owner: &str,
        msg: &str,
    ) {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            res = timeout(Duration::from_secs(5), self.handle_outbox_failure(row, lease_owner, msg)) => {
                if res.is_err() {
                    tracing::warn!(
                        outbox_id = row.id,
                        lease_owner = %lease_owner,
                        "outbox failure transition timed out"
                    );
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn deliver_to_single_subscriber(
        &self,
        webhook: crate::callisto::mega_webhook::Model,
        event_type: WebhookEventTypeEnum,
        event_id: String,
        event_type_str: String,
        payload_json: String,
        round_deadline: Instant,
        shutdown: CancellationToken,
    ) -> Result<(), String> {
        let mut last_error: Option<String> = None;

        for attempt in 1..=WEBHOOK_DELIVERY_MAX_ATTEMPTS {
            if shutdown.is_cancelled() {
                return Err("cancelled during shutdown".to_owned());
            }
            let remaining = round_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(last_error.unwrap_or_else(|| {
                    format!(
                        "webhook delivery round exceeded total_deadline_secs ({}s)",
                        self.delivery_config.total_deadline_secs
                    )
                }));
            }
            let req_timeout =
                remaining.min(Duration::from_secs(WEBHOOK_SINGLE_REQUEST_MAX_TIMEOUT_SECS));

            let delivery_step = tokio::select! {
                _ = shutdown.cancelled() => return Err("cancelled during shutdown".to_owned()),
                res = timeout(
                    req_timeout,
                    self.deliver_signed_v2(
                        &webhook.target_url,
                        &webhook.secret,
                        &event_id,
                        &event_type_str,
                        &payload_json,
                        req_timeout,
                    ),
                ) => res,
            };

            let (delivery_record, succeeded) = match delivery_step {
                Ok(Ok((status, body))) => {
                    let success = (200..300).contains(&status);
                    let error_message = if success {
                        None
                    } else {
                        Some(format!("webhook endpoint returned HTTP {status}"))
                    };
                    if !success {
                        last_error = error_message.clone();
                    }
                    (
                        crate::callisto::mega_webhook_delivery::Model {
                            id: IdInstance::next_id(),
                            webhook_id: webhook.id,
                            event_type: event_type.clone(),
                            payload: payload_json.clone(),
                            response_status: Some(status as i32),
                            response_body: Some(body),
                            success,
                            attempt,
                            error_message,
                            created_at: Utc::now().naive_utc(),
                        },
                        success,
                    )
                }
                Ok(Err(err)) => {
                    let error_message = err.to_string();
                    last_error = Some(error_message.clone());
                    (
                        crate::callisto::mega_webhook_delivery::Model {
                            id: IdInstance::next_id(),
                            webhook_id: webhook.id,
                            event_type: event_type.clone(),
                            payload: payload_json.clone(),
                            response_status: None,
                            response_body: None,
                            success: false,
                            attempt,
                            error_message: Some(error_message),
                            created_at: Utc::now().naive_utc(),
                        },
                        false,
                    )
                }
                Err(_) => {
                    let error_message = format!(
                        "webhook delivery attempt timed out after {}ms",
                        req_timeout.as_millis()
                    );
                    last_error = Some(error_message.clone());
                    (
                        crate::callisto::mega_webhook_delivery::Model {
                            id: IdInstance::next_id(),
                            webhook_id: webhook.id,
                            event_type: event_type.clone(),
                            payload: payload_json.clone(),
                            response_status: None,
                            response_body: None,
                            success: false,
                            attempt,
                            error_message: Some(error_message),
                            created_at: Utc::now().naive_utc(),
                        },
                        false,
                    )
                }
            };

            // Bound delivery-record persistence by the same round deadline so a
            // stalled DB write cannot hold the lease past `total_deadline_secs`.
            let save_res =
                timeout_at(round_deadline, self.storage.save_delivery(delivery_record)).await;
            if let Ok(Err(e)) = save_res {
                tracing::warn!("failed to save webhook delivery record: {e}");
            } else if save_res.is_err() {
                tracing::warn!("saving webhook delivery record exceeded round deadline");
            }

            if succeeded {
                return Ok(());
            }

            if attempt < WEBHOOK_DELIVERY_MAX_ATTEMPTS {
                let remaining_after = round_deadline.saturating_duration_since(Instant::now());
                if remaining_after.is_zero() {
                    break;
                }
                let backoff =
                    Duration::from_secs(WEBHOOK_DELIVERY_BACKOFF_BASE_SECS.pow(attempt as u32));
                tokio::select! {
                    _ = shutdown.cancelled() => return Err("cancelled during shutdown".to_owned()),
                    _ = tokio::time::sleep(backoff.min(remaining_after)) => {}
                }
            }
        }

        Err(last_error.unwrap_or_else(|| "webhook delivery failed".to_owned()))
    }

    async fn handle_outbox_failure(
        &self,
        row: &cl_webhook_outbox::Model,
        lease_owner: &str,
        error_msg: &str,
    ) {
        let next_attempt = row.attempt_count + 1;
        if next_attempt >= self.outbox_max_rounds {
            let _ = self
                .mark_outbox_dead_letter(row.id, lease_owner, row.lease_epoch, error_msg)
                .await;
        } else {
            let backoff_secs = (2u64.pow(next_attempt.clamp(1, 6) as u32)).min(60);
            let _ = self
                .reschedule_outbox_retry(
                    row.id,
                    lease_owner,
                    row.lease_epoch,
                    backoff_secs,
                    error_msg,
                )
                .await;
        }
    }

    /// Best-effort dispatch for non-outbox webhook events (e.g. comments, merge,
    /// title/status updates). Uses the same DNS-pinned client and canonical V2
    /// signing policy, fanning out concurrently to matching subscribers under
    /// `total_deadline_secs`.
    pub fn dispatch(&self, event_type: WebhookEvent, cl_model: &mega_cl::Model) {
        let svc = self.clone();
        let cl_payload = ClPayload::from(cl_model);
        let path = cl_model.path.clone();

        tokio::spawn(async move {
            if let Err(e) = svc.dispatch_inner(event_type, cl_payload, &path).await {
                tracing::error!("webhook dispatch error: {e}");
            }
        });
    }

    async fn dispatch_inner(
        &self,
        event_type: WebhookEvent,
        cl_payload: ClPayload,
        path: &str,
    ) -> Result<(), MegaError> {
        let event_type_str = event_type.to_value();
        let round_deadline =
            Instant::now() + Duration::from_secs(self.delivery_config.total_deadline_secs);
        let webhooks = timeout_at(
            round_deadline,
            self.storage
                .find_matching_webhooks(event_type.clone(), path),
        )
        .await
        .map_err(|_| MegaError::Other("webhook lookup timed out".to_owned()))??;

        let event_id = format!("clwh-{}", uuid::Uuid::new_v4());
        let repo_name = extract_repo_name(path).to_string();
        let payload = WebhookPayload {
            mega_version: env!("CARGO_PKG_VERSION").to_string(),
            event: event_type_str.clone(),
            timestamp: Utc::now().timestamp(),
            cl: cl_payload,
            repository: RepositoryPayload {
                path: path.to_string(),
                name: repo_name,
            },
        };
        let payload_json = serde_json::to_string(&payload)?;
        let shutdown = CancellationToken::new();

        let mut tasks = JoinSet::new();
        for webhook in webhooks {
            let svc = self.clone();
            let ev_type = event_type.clone();
            let ev_id = event_id.clone();
            let ev_type_str = event_type_str.clone();
            let p_json = payload_json.clone();
            let sd = shutdown.clone();
            let wid = webhook.id;
            tasks.spawn(async move {
                (
                    wid,
                    svc.deliver_to_single_subscriber(
                        webhook,
                        ev_type,
                        ev_id,
                        ev_type_str,
                        p_json,
                        round_deadline,
                        sd,
                    )
                    .await,
                )
            });
        }
        while let Some(res) = tasks.join_next().await {
            if let Ok((webhook_id, Err(error_message))) = res {
                tracing::warn!(
                    "webhook delivery failed after {} attempts for webhook_id={}: {}",
                    WEBHOOK_DELIVERY_MAX_ATTEMPTS,
                    webhook_id,
                    error_message
                );
            }
        }

        Ok(())
    }

    async fn deliver_signed_v2(
        &self,
        url_str: &str,
        secret: &str,
        event_id: &str,
        event_type: &str,
        payload: &str,
        request_timeout: Duration,
    ) -> Result<(u16, String), MegaError> {
        let (url, client) = build_pinned_webhook_client(
            url_str,
            &self.delivery_config,
            &self.allowed_private_cidrs,
            request_timeout,
        )
        .await?;
        let signing_secret = decrypt_webhook_secret(secret)?;
        let fresh_ts = Utc::now().timestamp().to_string();
        let normalized_path = if url.path().is_empty() {
            "/"
        } else {
            url.path()
        };
        let sig_v1 = compute_hmac_signature(&signing_secret, payload);
        let sig_v2 = compute_canonical_v2_signature(
            &signing_secret,
            "POST",
            normalized_path,
            &fresh_ts,
            event_id,
            event_type,
            payload.as_bytes(),
        );

        let resp = client
            .post(url)
            .header("Content-Type", "application/json")
            .header("X-Mega-Event", event_type)
            .header("X-Mega-Event-Id", event_id)
            .header("X-Mega-Timestamp", fresh_ts)
            .header("X-Mega-Signature", format!("sha256={sig_v1}"))
            .header("X-Mega-Signature-V2", sig_v2)
            .body(payload.to_string())
            .send()
            .await
            .map_err(|e| MegaError::Other(e.to_string()))?;

        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        Ok((status, body))
    }
}

/// Computes the canonical V2 HMAC-SHA256 signature (`sha256=<64-lowercase-hex>`)
/// matching `orion2`'s `POST /v1/integrations/mega2/cl-webhooks` ingress (`ADR-CLB-02`).
pub fn compute_canonical_v2_signature(
    secret: &str,
    method_upper: &str,
    normalized_path: &str,
    timestamp_ascii: &str,
    event_id: &str,
    event_type: &str,
    body_bytes: &[u8],
) -> String {
    let mut sha = Sha256::new();
    sha.update(body_bytes);
    let body_sha256 = hex::encode(sha.finalize());

    let canonical = format!(
        "{CANONICAL_V2_VERSION}\n{method_upper}\n{normalized_path}\n{timestamp_ascii}\n{event_id}\n{event_type}\n{body_sha256}"
    );
    type HmacSha256 = Hmac<Sha256>;
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC can take key of any size");
    mac.update(canonical.as_bytes());
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

fn merge_status_to_str(status: &MergeStatusEnum) -> &'static str {
    match status {
        MergeStatusEnum::Open => "open",
        MergeStatusEnum::Merged => "merged",
        MergeStatusEnum::Closed => "closed",
        MergeStatusEnum::Draft => "draft",
    }
}

fn extract_repo_name(path: &str) -> &str {
    path.trim_matches('/')
        .rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or("unknown")
}

fn compute_hmac_signature(secret: &str, payload: &str) -> String {
    type HmacSha256 = Hmac<Sha256>;
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC can take key of any size");
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Static webhook target URL validation using default strict public-HTTPS policy.
pub fn validate_webhook_target_url(target_url: &str) -> Result<(), MegaError> {
    validate_webhook_target_url_with_config(target_url, &WebhookDeliveryConfig::default())?;
    Ok(())
}

/// Static webhook target URL validation sharing `[webhook_delivery]` policy
/// between `webhook_router.rs` and `WebhookService` (`CLB-18`).
pub fn validate_webhook_target_url_with_config(
    target_url: &str,
    cfg: &WebhookDeliveryConfig,
) -> Result<Url, MegaError> {
    let allowed_cidrs = validate_delivery_config(cfg)?;
    let url = parse_webhook_url(target_url)?;
    validate_webhook_url_static_with_config(&url, cfg, &allowed_cidrs)?;
    Ok(url)
}

pub fn encrypt_webhook_secret(secret: &str) -> Result<String, MegaError> {
    if secret.is_empty() {
        return Err(MegaError::Other(
            "webhook secret cannot be empty".to_string(),
        ));
    }

    let key_bytes = load_webhook_secret_key_from_env()?;
    encrypt_webhook_secret_with_key(secret, &key_bytes)
}

pub fn decrypt_webhook_secret(secret: &str) -> Result<String, MegaError> {
    if !secret.starts_with(WEBHOOK_SECRET_ENC_PREFIX) {
        // Backward compatibility for existing plaintext secrets.
        return Ok(secret.to_string());
    }

    let key_bytes = load_webhook_secret_key_from_env()?;
    decrypt_webhook_secret_with_key(secret, &key_bytes)
}

async fn build_pinned_webhook_client(
    target_url: &str,
    cfg: &WebhookDeliveryConfig,
    allowed_cidrs: &[ParsedCidr],
    request_timeout: Duration,
) -> Result<(Url, reqwest::Client), MegaError> {
    let deadline = Instant::now() + request_timeout;
    let url = parse_webhook_url(target_url)?;
    validate_webhook_url_static_with_config(&url, cfg, allowed_cidrs)?;

    let port = url
        .port_or_known_default()
        .ok_or_else(|| MegaError::Other("webhook target_url has no known port".to_string()))?;

    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(Policy::none());

    match url.host() {
        Some(Host::Domain(domain)) => {
            let resolved: Vec<SocketAddr> = timeout_at(deadline, lookup_host((domain, port)))
                .await
                .map_err(|_| {
                    MegaError::Other("webhook target_url DNS resolution timed out".to_string())
                })?
                .map_err(|e| {
                    MegaError::Other(format!("failed to resolve webhook target_url host: {e}"))
                })?
                .collect();
            if resolved.is_empty() {
                return Err(MegaError::Other(
                    "webhook target_url host did not resolve to any IP".to_string(),
                ));
            }
            for addr in &resolved {
                validate_ip_against_config(addr.ip(), cfg, allowed_cidrs)?;
            }
            builder = builder.resolve_to_addrs(domain, &resolved);
        }
        Some(Host::Ipv4(v4)) => {
            validate_ip_against_config(IpAddr::V4(v4), cfg, allowed_cidrs)?;
        }
        Some(Host::Ipv6(v6)) => {
            validate_ip_against_config(IpAddr::V6(v6), cfg, allowed_cidrs)?;
        }
        None => {
            return Err(MegaError::Other(
                "webhook target_url must include a host".to_string(),
            ));
        }
    }

    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(MegaError::Other(
            "webhook target_url DNS resolution exhausted request timeout".to_string(),
        ));
    }
    let client = builder
        .timeout(remaining)
        .build()
        .map_err(|e| MegaError::Other(format!("failed to build pinned reqwest client: {e}")))?;
    Ok((url, client))
}

fn parse_webhook_url(target_url: &str) -> Result<Url, MegaError> {
    Url::parse(target_url).map_err(|e| MegaError::Other(format!("invalid webhook target_url: {e}")))
}

fn load_webhook_secret_key_from_env() -> Result<[u8; WEBHOOK_SECRET_KEY_LEN], MegaError> {
    let encoded_key = std::env::var(WEBHOOK_SECRET_ENC_KEY_ENV).map_err(|_| {
        MegaError::Other(format!(
            "{WEBHOOK_SECRET_ENC_KEY_ENV} is not set for webhook secret encryption"
        ))
    })?;

    let decoded_key = BASE64_STANDARD.decode(encoded_key).map_err(|_| {
        MegaError::Other(format!(
            "{WEBHOOK_SECRET_ENC_KEY_ENV} must be a valid base64-encoded 32-byte key"
        ))
    })?;

    let key: [u8; WEBHOOK_SECRET_KEY_LEN] = decoded_key.try_into().map_err(|_| {
        MegaError::Other(format!(
            "{WEBHOOK_SECRET_ENC_KEY_ENV} must decode to exactly {WEBHOOK_SECRET_KEY_LEN} bytes"
        ))
    })?;
    Ok(key)
}

fn encrypt_webhook_secret_with_key(
    secret: &str,
    key_bytes: &[u8; WEBHOOK_SECRET_KEY_LEN],
) -> Result<String, MegaError> {
    let unbound_key = UnboundKey::new(&aead::AES_256_GCM, key_bytes)
        .map_err(|_| MegaError::Other("invalid webhook encryption key".to_string()))?;
    let key = LessSafeKey::new(unbound_key);

    let mut nonce_bytes = [0u8; WEBHOOK_SECRET_NONCE_LEN];
    SystemRandom::new()
        .fill(&mut nonce_bytes)
        .map_err(|_| MegaError::Other("failed to generate webhook secret nonce".to_string()))?;

    let mut in_out = secret.as_bytes().to_vec();
    key.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce_bytes),
        Aad::empty(),
        &mut in_out,
    )
    .map_err(|_| MegaError::Other("failed to encrypt webhook secret".to_string()))?;

    let mut payload = Vec::with_capacity(WEBHOOK_SECRET_NONCE_LEN + in_out.len());
    payload.extend_from_slice(&nonce_bytes);
    payload.extend_from_slice(&in_out);

    Ok(format!(
        "{WEBHOOK_SECRET_ENC_PREFIX}{}",
        BASE64_STANDARD.encode(payload)
    ))
}

fn decrypt_webhook_secret_with_key(
    encrypted_secret: &str,
    key_bytes: &[u8; WEBHOOK_SECRET_KEY_LEN],
) -> Result<String, MegaError> {
    let encoded_payload = encrypted_secret
        .strip_prefix(WEBHOOK_SECRET_ENC_PREFIX)
        .ok_or_else(|| MegaError::Other("invalid encrypted webhook secret prefix".to_string()))?;

    let payload = BASE64_STANDARD
        .decode(encoded_payload)
        .map_err(|_| MegaError::Other("invalid encrypted webhook secret format".to_string()))?;

    if payload.len() <= WEBHOOK_SECRET_NONCE_LEN {
        return Err(MegaError::Other(
            "invalid encrypted webhook secret payload".to_string(),
        ));
    }

    let mut nonce_bytes = [0u8; WEBHOOK_SECRET_NONCE_LEN];
    nonce_bytes.copy_from_slice(&payload[..WEBHOOK_SECRET_NONCE_LEN]);
    let mut cipher_and_tag = payload[WEBHOOK_SECRET_NONCE_LEN..].to_vec();

    let unbound_key = UnboundKey::new(&aead::AES_256_GCM, key_bytes)
        .map_err(|_| MegaError::Other("invalid webhook encryption key".to_string()))?;
    let key = LessSafeKey::new(unbound_key);

    let plaintext = key
        .open_in_place(
            Nonce::assume_unique_for_key(nonce_bytes),
            Aad::empty(),
            &mut cipher_and_tag,
        )
        .map_err(|_| MegaError::Other("failed to decrypt webhook secret".to_string()))?;

    String::from_utf8(plaintext.to_vec())
        .map_err(|_| MegaError::Other("decrypted webhook secret is not valid UTF-8".to_string()))
}

fn validate_webhook_url_static_with_config(
    url: &Url,
    cfg: &WebhookDeliveryConfig,
    allowed_cidrs: &[ParsedCidr],
) -> Result<(), MegaError> {
    match url.scheme() {
        "https" => {}
        "http" if cfg.allow_http => {}
        _ => {
            return Err(MegaError::Other(
                "webhook target_url must use https".to_string(),
            ));
        }
    }

    if !url.username().is_empty() || url.password().is_some() {
        return Err(MegaError::Other(
            "webhook target_url must not contain userinfo".to_string(),
        ));
    }

    let host = url
        .host()
        .ok_or_else(|| MegaError::Other("webhook target_url must include a host".to_string()))?;

    match host {
        Host::Domain(domain) => {
            let domain_lc = domain.to_ascii_lowercase();
            if domain_lc == "localhost" || domain_lc.ends_with(".localhost") {
                if !cfg.allow_loopback {
                    return Err(MegaError::Other(
                        "webhook target_url host is not allowed".to_string(),
                    ));
                }
            } else if domain_lc.ends_with(".local") {
                return Err(MegaError::Other(
                    "webhook target_url host is not allowed".to_string(),
                ));
            }
        }
        Host::Ipv4(ip) => validate_ip_against_config(IpAddr::V4(ip), cfg, allowed_cidrs)?,
        Host::Ipv6(ip) => validate_ip_against_config(IpAddr::V6(ip), cfg, allowed_cidrs)?,
    }

    Ok(())
}

fn validate_ip_against_config(
    ip: IpAddr,
    cfg: &WebhookDeliveryConfig,
    allowed_cidrs: &[ParsedCidr],
) -> Result<(), MegaError> {
    // Normalize IPv4-mapped IPv6 addresses so `::ffff:169.254.169.254`,
    // `::ffff:127.0.0.1`, and `::ffff:10.0.0.1` undergo the exact IPv4 checks.
    let canonical_ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        IpAddr::V4(_) => ip,
    };

    if is_always_forbidden_ip(canonical_ip) {
        return Err(MegaError::Other(
            "webhook target_url IP is unconditionally forbidden (metadata/link-local/unspecified/multicast)"
                .to_string(),
        ));
    }

    if canonical_ip.is_loopback() {
        if cfg.allow_loopback || ip_matches_any_cidr(ip, canonical_ip, allowed_cidrs) {
            return Ok(());
        }
        return Err(MegaError::Other(
            "webhook target_url loopback IP is not allowed".to_string(),
        ));
    }

    if is_private_or_internal_ip(canonical_ip) {
        if ip_matches_any_cidr(ip, canonical_ip, allowed_cidrs) {
            return Ok(());
        }
        return Err(MegaError::Other(
            "webhook target_url private IP is not allowed".to_string(),
        ));
    }

    Ok(())
}

fn ip_matches_any_cidr(orig: IpAddr, canonical: IpAddr, cidrs: &[ParsedCidr]) -> bool {
    cidrs
        .iter()
        .any(|cidr| cidr.contains(orig) || cidr.contains(canonical))
}

fn is_always_forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            v4.is_link_local() // 169.254.0.0/16 (cloud metadata)
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || (a == 0)
                || (a == 192 && b == 0 && c == 0)
                || (a == 192 && b == 0 && c == 2)
                || (a == 198 && b == 51 && c == 100)
                || (a == 203 && b == 0 && c == 113)
                || a >= 240
        }
        IpAddr::V6(v6) => v6.is_unspecified() || v6.is_unicast_link_local() || v6.is_multicast(),
    }
}

fn is_private_or_internal_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, _, _] = v4.octets();
            v4.is_private()
                || (a == 100 && (64..=127).contains(&b)) // CGNAT 100.64.0.0/10
                || (a == 198 && (18..=19).contains(&b)) // benchmark 198.18.0.0/15
        }
        IpAddr::V6(v6) => v6.is_unique_local(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use axum::{Router, body::Bytes, extract::State, http::HeaderMap, routing::post};
    use chrono::{DateTime, NaiveDateTime, TimeDelta};
    use sea_orm::{ActiveModelTrait, EntityTrait, Set};
    use tokio::{net::TcpListener, sync::Mutex};

    use super::*;
    use crate::{
        callisto::{
            cl_webhook_outbox, mega_webhook,
            sea_orm_active_enums::{MergeStatusEnum, WebhookEventTypeEnum},
        },
        jupiter::{
            migration::apply_migrations, storage::base_storage::BaseStorage,
            tests::test_db_connection,
        },
    };

    #[test]
    fn test_validate_webhook_target_url_accepts_public_https_domain() {
        let result = validate_webhook_target_url("https://example.com/webhook");
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_webhook_target_url_rejects_non_https() {
        let result = validate_webhook_target_url("http://example.com/webhook");
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_webhook_target_url_rejects_loopback_ip() {
        let result = validate_webhook_target_url("https://127.0.0.1:8443/webhook");
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_webhook_target_url_rejects_private_ipv4() {
        let result = validate_webhook_target_url("https://192.168.1.2/webhook");
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_webhook_target_url_rejects_localhost_domain() {
        let result = validate_webhook_target_url("https://localhost/webhook");
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_webhook_target_url_rejects_loopback_ipv6() {
        let result = validate_webhook_target_url("https://[::1]/webhook");
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_webhook_target_url_rejects_link_local_ipv6() {
        let result = validate_webhook_target_url("https://[fe80::1]/webhook");
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_webhook_target_url_rejects_cloud_metadata_even_with_opt_in() {
        let cfg = WebhookDeliveryConfig {
            allow_http: true,
            allow_loopback: true,
            allowed_private_cidrs: vec!["10.0.0.0/8".to_owned(), "172.16.0.0/12".to_owned()],
            ..WebhookDeliveryConfig::default()
        };
        assert!(validate_webhook_target_url_with_config("http://10.1.2.3:8080/wh", &cfg).is_ok());
        assert!(validate_webhook_target_url_with_config("http://127.0.0.1:8080/wh", &cfg).is_ok());
        assert!(
            validate_webhook_target_url_with_config("http://169.254.169.254/latest", &cfg).is_err()
        );
        assert!(
            validate_webhook_target_url_with_config("http://[::ffff:169.254.169.254]/latest", &cfg)
                .is_err()
        );
        assert!(
            validate_webhook_target_url_with_config("http://192.168.1.5:8080/wh", &cfg).is_err()
        );
    }

    #[test]
    fn test_webhook_service_rejects_illegal_lease_margin_at_startup() {
        let bad = WebhookDeliveryConfig {
            total_deadline_secs: 60,
            outbox_lease_secs: 89,
            ..WebhookDeliveryConfig::default()
        };
        let err = validate_delivery_config(&bad).expect_err("lease < deadline + 30 must fail");
        assert!(err.to_string().contains("outbox_lease_secs"), "{err}");

        let storage = WebhookStorage {
            base: BaseStorage::mock(),
        };
        let err = WebhookService::new(storage, bad)
            .err()
            .expect("WebhookService::new must fail closed on lease < deadline + 30");
        assert!(err.to_string().contains("outbox_lease_secs"), "{err}");

        for overflow_lease in [86_401u64, i64::MAX as u64, u64::MAX] {
            let too_large = WebhookDeliveryConfig {
                total_deadline_secs: 60,
                outbox_lease_secs: overflow_lease,
                ..WebhookDeliveryConfig::default()
            };
            let err = validate_delivery_config(&too_large)
                .expect_err("outbox_lease_secs above safe PostgreSQL interval cap must fail");
            assert!(err.to_string().contains("outbox_lease_secs"), "{err}");
            let err = WebhookService::new(
                WebhookStorage {
                    base: BaseStorage::mock(),
                },
                too_large,
            )
            .err()
            .expect("WebhookService::new must fail closed on overflow outbox_lease_secs");
            assert!(err.to_string().contains("outbox_lease_secs"), "{err}");
        }
    }

    #[test]
    fn test_canonical_v2_signature_matches_orion2_golden_vector() {
        // Exact same inputs as `orion2/crates/orion-server/src/mega2_webhook.rs`.
        let secret = "mega2-webhook-test-secret-at-least-32-bytes";
        let body = br#"{"event":"cl.created","cl":{"link":"cl01","status":"open","head_commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_revision":1}}"#;
        let sig = compute_canonical_v2_signature(
            secret,
            "POST",
            "/v1/integrations/mega2/cl-webhooks",
            "1700000000",
            "evt-01",
            "cl.created",
            body,
        );
        assert_eq!(
            sig,
            "sha256=320cd25d1a2015a7e08f1d25738a55c7a7f523d6d13f79c0e30ceefd1a2de091"
        );
    }

    #[test]
    fn test_encrypt_decrypt_webhook_secret_round_trip() {
        let key = [7u8; 32];
        let secret = "test-webhook-secret";

        let encrypted = encrypt_webhook_secret_with_key(secret, &key).expect("encrypt failed");
        assert!(encrypted.starts_with("enc:v1:"));

        let decrypted = decrypt_webhook_secret_with_key(&encrypted, &key).expect("decrypt failed");
        assert_eq!(decrypted, secret);
    }

    #[test]
    fn test_decrypt_webhook_secret_plaintext_is_backward_compatible() {
        let plaintext = "legacy-plaintext-secret";
        let decrypted = decrypt_webhook_secret(plaintext).expect("decrypt failed");
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_extract_repo_name_normal_path() {
        assert_eq!(extract_repo_name("/mono/repo"), "repo");
    }

    #[test]
    fn test_extract_repo_name_trailing_slash() {
        assert_eq!(extract_repo_name("/mono/repo/"), "repo");
    }

    #[test]
    fn test_extract_repo_name_root_path_fallback() {
        assert_eq!(extract_repo_name("/"), "unknown");
    }

    #[test]
    fn test_extract_repo_name_empty_path_fallback() {
        assert_eq!(extract_repo_name(""), "unknown");
    }

    #[test]
    fn test_cl_payload_from_model_uses_contract_fields() {
        let model = crate::callisto::mega_cl::Model {
            id: 1,
            link: "CL123".to_string(),
            title: "Test".to_string(),
            merge_date: None,
            status: MergeStatusEnum::Open,
            path: "/repo/path".to_string(),
            base_branch: "main".to_string(),
            from_hash: "abc".to_string(),
            to_hash: "def".to_string(),
            created_at: DateTime::from_timestamp(0, 0).unwrap().naive_utc(),
            updated_at: DateTime::from_timestamp(0, 0).unwrap().naive_utc(),
            username: "alice".to_string(),
            revision: 0,
            head_revision: 3,
        };

        let payload = ClPayload::from(&model);
        assert_eq!(payload.status, "open");
        assert_eq!(payload.path, "/repo/path");
        assert_eq!(payload.head_revision, 3);
    }

    async fn seed_outbox_row(
        conn: &sea_orm::DatabaseConnection,
        id: i64,
        event_id: &str,
        state: &str,
        created_at: NaiveDateTime,
        next_retry_at: NaiveDateTime,
        payload_json: &str,
    ) {
        cl_webhook_outbox::ActiveModel {
            id: Set(id),
            event_id: Set(event_id.to_owned()),
            event_type: Set("cl.created".to_owned()),
            cl_link: Set("CLWH001".to_owned()),
            cl_path: Set("/project/demo".to_owned()),
            head_revision: Set(1),
            payload_json: Set(payload_json.to_owned()),
            state: Set(state.to_owned()),
            lease_owner: Set(None),
            lease_epoch: Set(0),
            lease_expires_at: Set(None),
            attempt_count: Set(0),
            next_retry_at: Set(next_retry_at),
            last_error: Set(None),
            created_at: Set(created_at),
            updated_at: Set(created_at),
        }
        .insert(conn)
        .await
        .expect("seed outbox row");
    }

    #[tokio::test]
    async fn outbox_concurrent_batch_dispatch_avoids_head_of_line_blocking() {
        let temp = tempfile::TempDir::new().expect("temp dir");
        let conn = Arc::new(test_db_connection(temp.path()).await);
        apply_migrations(conn.as_ref(), true)
            .await
            .expect("migrations");
        let webhook_storage = WebhookStorage {
            base: BaseStorage::new(conn.clone()),
        };

        let secret = "clb18-concurrent-batch-secret-32-bytes-min";
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_seen_concurrent = Arc::new(AtomicUsize::new(0));
        let received = Arc::new(Mutex::new(
            Vec::<(String, String, String, String, String)>::new(),
        ));

        #[derive(Clone)]
        struct MockState {
            in_flight: Arc<AtomicUsize>,
            max_seen: Arc<AtomicUsize>,
            received: Arc<Mutex<Vec<(String, String, String, String, String)>>>,
        }
        let app = Router::new()
            .route(
                "/v1/integrations/mega2/cl-webhooks",
                post(
                    |State(st): State<MockState>, headers: HeaderMap, body: Bytes| async move {
                        let cur = st.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                        st.max_seen.fetch_max(cur, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        st.in_flight.fetch_sub(1, Ordering::SeqCst);

                        let evt = headers
                            .get("x-mega-event")
                            .unwrap()
                            .to_str()
                            .unwrap()
                            .to_owned();
                        let evt_id = headers
                            .get("x-mega-event-id")
                            .unwrap()
                            .to_str()
                            .unwrap()
                            .to_owned();
                        let ts = headers
                            .get("x-mega-timestamp")
                            .unwrap()
                            .to_str()
                            .unwrap()
                            .to_owned();
                        let sig_v2 = headers
                            .get("x-mega-signature-v2")
                            .unwrap()
                            .to_str()
                            .unwrap()
                            .to_owned();
                        let body_str = String::from_utf8(body.to_vec()).unwrap();
                        st.received
                            .lock()
                            .await
                            .push((evt, evt_id, ts, sig_v2, body_str));
                        axum::http::StatusCode::OK
                    },
                ),
            )
            .with_state(MockState {
                in_flight: in_flight.clone(),
                max_seen: max_seen_concurrent.clone(),
                received: received.clone(),
            });

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().unwrap();
        let server_handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let target_url = format!("http://{addr}/v1/integrations/mega2/cl-webhooks");
        let now = Utc::now().naive_utc();
        webhook_storage
            .create_webhook(
                mega_webhook::Model {
                    id: IdInstance::next_id(),
                    target_url,
                    secret: secret.to_owned(),
                    event_types: "[]".to_owned(),
                    path_filter: Some("/project".to_owned()),
                    active: true,
                    created_at: now,
                    updated_at: now,
                },
                vec![WebhookEventTypeEnum::ClCreated],
            )
            .await
            .expect("create webhook");

        for i in 1..=4 {
            let payload = format!(r#"{{"event":"cl.created","seq":{i}}}"#);
            seed_outbox_row(
                conn.as_ref(),
                1000 + i,
                &format!("evt-batch-{i}"),
                "pending",
                now,
                now - TimeDelta::seconds(5),
                &payload,
            )
            .await;
        }

        let cfg = WebhookDeliveryConfig {
            allow_http: true,
            allow_loopback: true,
            max_in_flight: 4,
            total_deadline_secs: 10,
            outbox_lease_secs: 45,
            ..WebhookDeliveryConfig::default()
        };
        let svc = WebhookService::new(webhook_storage, cfg).expect("service");
        let started = Instant::now();
        let processed = svc
            .process_outbox_once("worker-concurrent")
            .await
            .expect("process batch");
        let elapsed = started.elapsed();
        assert_eq!(processed, 4);
        assert!(
            max_seen_concurrent.load(Ordering::SeqCst) >= 2,
            "full batch must dispatch concurrently, max_seen={}",
            max_seen_concurrent.load(Ordering::SeqCst)
        );
        assert!(
            elapsed < Duration::from_millis(850),
            "4x250ms concurrent batch should finish well under serial 1000ms, took {elapsed:?}"
        );

        let rec = received.lock().await;
        assert_eq!(rec.len(), 4);
        for (evt, evt_id, ts, sig_v2, body) in rec.iter() {
            let expected = compute_canonical_v2_signature(
                secret,
                "POST",
                "/v1/integrations/mega2/cl-webhooks",
                ts,
                evt_id,
                evt,
                body.as_bytes(),
            );
            assert_eq!(sig_v2, &expected);
        }

        let rows = cl_webhook_outbox::Entity::find()
            .all(conn.as_ref())
            .await
            .unwrap();
        assert!(rows.iter().all(|r| r.state == "delivered"));

        server_handle.abort();
    }

    #[tokio::test]
    async fn outbox_slow_failing_subscriber_does_not_starve_healthy_subscriber() {
        let temp = tempfile::TempDir::new().expect("temp dir");
        let conn = Arc::new(test_db_connection(temp.path()).await);
        apply_migrations(conn.as_ref(), true)
            .await
            .expect("migrations");
        let webhook_storage = WebhookStorage {
            base: BaseStorage::new(conn.clone()),
        };

        let healthy_hits = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/slow-fail",
                post(|| async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR
                }),
            )
            .route(
                "/healthy",
                post({
                    let hits = healthy_hits.clone();
                    move || {
                        let hits = hits.clone();
                        async move {
                            hits.fetch_add(1, Ordering::SeqCst);
                            axum::http::StatusCode::OK
                        }
                    }
                }),
            );

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().unwrap();
        let server_handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let now = Utc::now().naive_utc();
        let secret = "clb18-multi-subscriber-secret-32-bytes!!";
        // Register the slow/failing subscriber FIRST, then the healthy subscriber.
        webhook_storage
            .create_webhook(
                mega_webhook::Model {
                    id: 5001,
                    target_url: format!("http://{addr}/slow-fail"),
                    secret: secret.to_owned(),
                    event_types: "[]".to_owned(),
                    path_filter: Some("/project".to_owned()),
                    active: true,
                    created_at: now,
                    updated_at: now,
                },
                vec![WebhookEventTypeEnum::ClCreated],
            )
            .await
            .unwrap();
        webhook_storage
            .create_webhook(
                mega_webhook::Model {
                    id: 5002,
                    target_url: format!("http://{addr}/healthy"),
                    secret: secret.to_owned(),
                    event_types: "[]".to_owned(),
                    path_filter: Some("/project".to_owned()),
                    active: true,
                    created_at: now,
                    updated_at: now,
                },
                vec![WebhookEventTypeEnum::ClCreated],
            )
            .await
            .unwrap();

        seed_outbox_row(
            conn.as_ref(),
            5100,
            "evt-fanout-1",
            "pending",
            now,
            now - TimeDelta::seconds(5),
            r#"{"event":"cl.created"}"#,
        )
        .await;

        let cfg = WebhookDeliveryConfig {
            allow_http: true,
            allow_loopback: true,
            total_deadline_secs: 5,
            outbox_lease_secs: 35,
            ..WebhookDeliveryConfig::default()
        };
        let svc = WebhookService::new(webhook_storage, cfg).expect("service");
        let started = Instant::now();
        svc.process_outbox_once("worker-fanout").await.unwrap();
        let elapsed = started.elapsed();

        assert_eq!(
            healthy_hits.load(Ordering::SeqCst),
            1,
            "healthy subscriber must still receive delivery even when earlier subscriber stalls"
        );
        assert!(
            elapsed < Duration::from_secs(7),
            "round must stay bounded by total_deadline_secs (5s), took {elapsed:?}"
        );

        server_handle.abort();
    }

    #[tokio::test]
    async fn outbox_lease_epoch_fences_stale_worker_on_db_clock_expiry_and_takeover() {
        let temp = tempfile::TempDir::new().expect("temp dir");
        let conn = Arc::new(test_db_connection(temp.path()).await);
        apply_migrations(conn.as_ref(), true)
            .await
            .expect("migrations");
        let webhook_storage = WebhookStorage {
            base: BaseStorage::new(conn.clone()),
        };
        let cfg = WebhookDeliveryConfig {
            total_deadline_secs: 5,
            outbox_lease_secs: 35,
            ..WebhookDeliveryConfig::default()
        };
        let svc = WebhookService::new(webhook_storage, cfg).expect("service");

        let past = Utc::now().naive_utc() - TimeDelta::seconds(5);
        seed_outbox_row(
            conn.as_ref(),
            2001,
            "evt-fence-1",
            "pending",
            past,
            past,
            "{}",
        )
        .await;

        // Worker A claims -> lease_epoch = 1.
        let batch_a = svc.claim_outbox_batch("worker-a").await.unwrap();
        assert_eq!(batch_a.len(), 1);
        assert_eq!(batch_a[0].lease_epoch, 1);

        // Set Worker A's lease to expire in 150ms, then hold a FOR UPDATE lock on the row
        // for 350ms while Worker A starts `mark_outbox_delivered` BEFORE expiry. Once the
        // lock releases AFTER expiry, lock-first `clock_timestamp()` must fence Worker A out!
        conn.execute_raw(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE cl_webhook_outbox SET lease_expires_at = (clock_timestamp() AT TIME ZONE 'UTC') + INTERVAL '150 milliseconds' WHERE id = $1",
            [2001i64.into()],
        ))
        .await
        .unwrap();
        let blocker_txn = conn.begin().await.unwrap();
        blocker_txn
            .execute_raw(sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT 1 FROM cl_webhook_outbox WHERE id = $1 FOR UPDATE",
                [2001i64.into()],
            ))
            .await
            .unwrap();
        let svc_clone = svc.clone();
        let blocked_update = tokio::spawn(async move {
            svc_clone
                .mark_outbox_delivered(2001, "worker-a", 1)
                .await
                .unwrap()
        });
        tokio::time::sleep(Duration::from_millis(350)).await;
        blocker_txn.rollback().await.unwrap();
        assert!(
            !blocked_update.await.unwrap(),
            "transition blocked on a lock across lease expiry must be fenced out via clock_timestamp()"
        );

        // Worker B takes over -> lease_epoch = 2.
        let batch_b = svc.claim_outbox_batch("worker-b").await.unwrap();
        assert_eq!(batch_b.len(), 1);
        assert_eq!(batch_b[0].lease_epoch, 2);
        assert_eq!(batch_b[0].lease_owner.as_deref(), Some("worker-b"));

        // Stale Worker A attempts to deliver/reschedule/dead-letter using epoch=1 -> ALL return false (0 rows)!
        assert!(
            !svc.mark_outbox_delivered(2001, "worker-a", 1)
                .await
                .unwrap()
        );
        assert!(
            !svc.reschedule_outbox_retry(2001, "worker-a", 1, 5, "stale err")
                .await
                .unwrap()
        );
        assert!(
            !svc.mark_outbox_dead_letter(2001, "worker-a", 1, "stale err")
                .await
                .unwrap()
        );

        // Row is still leased by Worker B at epoch=2.
        let mid = cl_webhook_outbox::Entity::find_by_id(2001)
            .one(conn.as_ref())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(mid.state, "leased");
        assert_eq!(mid.lease_owner.as_deref(), Some("worker-b"));
        assert_eq!(mid.lease_epoch, 2);

        // Worker B commits with epoch=2 -> succeeds!
        assert!(
            svc.mark_outbox_delivered(2001, "worker-b", 2)
                .await
                .unwrap()
        );
        let final_row = cl_webhook_outbox::Entity::find_by_id(2001)
            .one(conn.as_ref())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(final_row.state, "delivered");
    }

    #[tokio::test]
    async fn outbox_outage_to_dead_letter_then_redrive_resigns_with_fresh_timestamp() {
        let temp = tempfile::TempDir::new().expect("temp dir");
        let conn = Arc::new(test_db_connection(temp.path()).await);
        apply_migrations(conn.as_ref(), true)
            .await
            .expect("migrations");
        let webhook_storage = WebhookStorage {
            base: BaseStorage::new(conn.clone()),
        };

        let secret = "clb18-redrive-fresh-sign-secret-32-bytes!";
        let original_payload =
            r#"{"event":"cl.created","cl":{"link":"CLWH001","head_revision":1}}"#;
        let outage_active = Arc::new(AtomicBool::new(true));
        let received_ts = Arc::new(Mutex::new(Option::<i64>::None));
        let received_evt_id = Arc::new(Mutex::new(Option::<String>::None));

        #[derive(Clone)]
        struct RedriveMock {
            secret: String,
            expected_payload: String,
            outage_active: Arc<AtomicBool>,
            received_ts: Arc<Mutex<Option<i64>>>,
            received_evt_id: Arc<Mutex<Option<String>>>,
        }
        let app = Router::new()
            .route(
                "/v1/integrations/mega2/cl-webhooks",
                post(
                    |State(st): State<RedriveMock>, headers: HeaderMap, body: Bytes| async move {
                        if st.outage_active.load(Ordering::SeqCst) {
                            return axum::http::StatusCode::SERVICE_UNAVAILABLE;
                        }
                        let ts_str = headers.get("x-mega-timestamp").unwrap().to_str().unwrap();
                        let ts: i64 = ts_str.parse().unwrap();
                        let now_ts = Utc::now().timestamp();
                        // Strictly enforce 300s skew window just like orion2!
                        if (now_ts - ts).abs() > 300 {
                            return axum::http::StatusCode::UNAUTHORIZED;
                        }
                        let evt = headers.get("x-mega-event").unwrap().to_str().unwrap();
                        let evt_id = headers.get("x-mega-event-id").unwrap().to_str().unwrap();
                        let sig_v2 = headers
                            .get("x-mega-signature-v2")
                            .unwrap()
                            .to_str()
                            .unwrap();
                        let expected_sig = compute_canonical_v2_signature(
                            &st.secret,
                            "POST",
                            "/v1/integrations/mega2/cl-webhooks",
                            ts_str,
                            evt_id,
                            evt,
                            &body,
                        );
                        assert_eq!(sig_v2, expected_sig);
                        assert_eq!(std::str::from_utf8(&body).unwrap(), st.expected_payload);
                        *st.received_ts.lock().await = Some(ts);
                        *st.received_evt_id.lock().await = Some(evt_id.to_owned());
                        axum::http::StatusCode::OK
                    },
                ),
            )
            .with_state(RedriveMock {
                secret: secret.to_owned(),
                expected_payload: original_payload.to_owned(),
                outage_active: outage_active.clone(),
                received_ts: received_ts.clone(),
                received_evt_id: received_evt_id.clone(),
            });

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().unwrap();
        let server_handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let now = Utc::now().naive_utc();
        webhook_storage
            .create_webhook(
                mega_webhook::Model {
                    id: IdInstance::next_id(),
                    target_url: format!("http://{addr}/v1/integrations/mega2/cl-webhooks"),
                    secret: secret.to_owned(),
                    event_types: "[]".to_owned(),
                    path_filter: Some("/project".to_owned()),
                    active: true,
                    created_at: now,
                    updated_at: now,
                },
                vec![WebhookEventTypeEnum::ClCreated],
            )
            .await
            .expect("create webhook");

        // Row 1: created 600s ago (> 300s skew window), starts pending and hits real outage -> dead_letter!
        let outage_created = now - TimeDelta::seconds(600);
        seed_outbox_row(
            conn.as_ref(),
            3001,
            "evt-outage-600s",
            "pending",
            outage_created,
            outage_created,
            original_payload,
        )
        .await;

        // Row 2: dead-lettered 8 days ago (> 604800s max_redrive_age_secs) -> must be skipped!
        let expired_created = now - TimeDelta::days(8);
        seed_outbox_row(
            conn.as_ref(),
            3002,
            "evt-expired-8d",
            "dead_letter",
            expired_created,
            expired_created,
            original_payload,
        )
        .await;

        let cfg = WebhookDeliveryConfig {
            allow_http: true,
            allow_loopback: true,
            total_deadline_secs: 5,
            outbox_lease_secs: 35,
            max_redrive_age_secs: 604_800,
            ..WebhookDeliveryConfig::default()
        };
        let svc = WebhookService::new(webhook_storage, cfg)
            .expect("service")
            .with_outbox_max_rounds(1);

        // Process during outage -> transitions organically to dead_letter!
        svc.process_outbox_once("worker-outage").await.unwrap();
        let dl_row = cl_webhook_outbox::Entity::find_by_id(3001)
            .one(conn.as_ref())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(dl_row.state, "dead_letter");
        assert!(dl_row.last_error.is_some());

        // End outage and verify invalid redrive limits fail closed.
        outage_active.store(false, Ordering::SeqCst);
        assert!(svc.redrive_dead_letter_cl_webhooks(0, None).await.is_err());
        assert!(
            svc.redrive_dead_letter_cl_webhooks(101, None)
                .await
                .is_err()
        );

        // Redrive eligible row and skip expired 8-day-old row.
        let outcome = svc
            .redrive_dead_letter_cl_webhooks(100, None)
            .await
            .expect("redrive");
        assert_eq!(outcome.redriven_count, 1);
        assert_eq!(outcome.skipped_expired_count, 1);

        // Process outbox -> delivers with fresh timestamp & immutable event_id/payload!
        let processed = svc
            .process_outbox_once("worker-redrive")
            .await
            .expect("process redriven");
        assert_eq!(processed, 1);

        let delivered_ts = received_ts.lock().await.expect("received webhook");
        assert!(
            delivered_ts > outage_created.and_utc().timestamp() + 300,
            "redriven delivery must carry a fresh timestamp, not original outage timestamp"
        );
        assert_eq!(
            received_evt_id.lock().await.as_deref(),
            Some("evt-outage-600s")
        );

        let r1 = cl_webhook_outbox::Entity::find_by_id(3001)
            .one(conn.as_ref())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r1.state, "delivered");

        let r2 = cl_webhook_outbox::Entity::find_by_id(3002)
            .one(conn.as_ref())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r2.state, "dead_letter");

        server_handle.abort();
    }

    #[tokio::test]
    async fn outbox_worker_loop_shuts_down_promptly_during_slow_delivery() {
        let temp = tempfile::TempDir::new().expect("temp dir");
        let conn = Arc::new(test_db_connection(temp.path()).await);
        apply_migrations(conn.as_ref(), true)
            .await
            .expect("migrations");
        let webhook_storage = WebhookStorage {
            base: BaseStorage::new(conn.clone()),
        };

        let started_notify = Arc::new(tokio::sync::Notify::new());
        let app = Router::new().route(
            "/stall",
            post({
                let notify = started_notify.clone();
                move || {
                    let notify = notify.clone();
                    async move {
                        notify.notify_one();
                        tokio::time::sleep(Duration::from_secs(60)).await;
                        axum::http::StatusCode::OK
                    }
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().unwrap();
        let server_handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let now = Utc::now().naive_utc();
        webhook_storage
            .create_webhook(
                mega_webhook::Model {
                    id: 6001,
                    target_url: format!("http://{addr}/stall"),
                    secret: "clb18-shutdown-secret-32-bytes-minimum!!".to_owned(),
                    event_types: "[]".to_owned(),
                    path_filter: Some("/project".to_owned()),
                    active: true,
                    created_at: now,
                    updated_at: now,
                },
                vec![WebhookEventTypeEnum::ClCreated],
            )
            .await
            .unwrap();
        seed_outbox_row(
            conn.as_ref(),
            6100,
            "evt-stall-1",
            "pending",
            now,
            now - TimeDelta::seconds(5),
            r#"{"event":"cl.created"}"#,
        )
        .await;

        let cfg = WebhookDeliveryConfig {
            allow_http: true,
            allow_loopback: true,
            total_deadline_secs: 60,
            outbox_lease_secs: 90,
            ..WebhookDeliveryConfig::default()
        };
        let svc = WebhookService::new(webhook_storage, cfg).expect("service");
        let shutdown = CancellationToken::new();
        let worker_handle = svc.spawn_outbox_worker(shutdown.clone());

        timeout(Duration::from_secs(5), started_notify.notified())
            .await
            .expect("worker started slow delivery");
        let cancel_at = Instant::now();
        shutdown.cancel();
        timeout(Duration::from_secs(3), worker_handle)
            .await
            .expect("worker loop must exit promptly on shutdown")
            .expect("worker task did not panic");
        assert!(cancel_at.elapsed() < Duration::from_secs(2));

        server_handle.abort();
    }

    #[tokio::test]
    async fn outbox_worker_loop_shuts_down_promptly_while_claim_is_blocked() {
        let temp = tempfile::TempDir::new().expect("temp dir");
        let conn = Arc::new(test_db_connection(temp.path()).await);
        apply_migrations(conn.as_ref(), true)
            .await
            .expect("migrations");
        let webhook_storage = WebhookStorage {
            base: BaseStorage::new(conn.clone()),
        };

        let now = Utc::now().naive_utc();
        seed_outbox_row(
            conn.as_ref(),
            7001,
            "evt-blocked-claim-1",
            "pending",
            now,
            now - TimeDelta::seconds(5),
            r#"{"event":"cl.created"}"#,
        )
        .await;

        // Hold an ACCESS EXCLUSIVE table lock in a separate transaction so the
        // worker's claim blocks on the DB lock.
        let blocker_txn = conn.begin().await.unwrap();
        blocker_txn
            .execute_unprepared("LOCK TABLE cl_webhook_outbox IN ACCESS EXCLUSIVE MODE")
            .await
            .unwrap();

        let svc = WebhookService::new(webhook_storage, WebhookDeliveryConfig::default())
            .expect("service");
        let shutdown = CancellationToken::new();
        let worker_handle = svc.spawn_outbox_worker(shutdown.clone());

        tokio::time::sleep(Duration::from_millis(150)).await;
        let cancel_at = Instant::now();
        shutdown.cancel();
        timeout(Duration::from_secs(2), worker_handle)
            .await
            .expect("worker loop must cancel blocked claim and exit promptly on shutdown")
            .expect("worker task did not panic");
        assert!(cancel_at.elapsed() < Duration::from_secs(1));

        // Release the table lock and verify NO orphaned lease was committed!
        blocker_txn.rollback().await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let row = cl_webhook_outbox::Entity::find_by_id(7001)
            .one(conn.as_ref())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.state, "pending",
            "cancelled claim must never commit an orphaned lease"
        );
        assert!(row.lease_owner.is_none());
        assert_eq!(row.lease_epoch, 0);
    }

    #[tokio::test]
    async fn outbox_locked_transition_row_does_not_starve_unrelated_rows() {
        let temp = tempfile::TempDir::new().expect("temp dir");
        let conn = Arc::new(test_db_connection(temp.path()).await);
        apply_migrations(conn.as_ref(), true)
            .await
            .expect("migrations");
        let webhook_storage = WebhookStorage {
            base: BaseStorage::new(conn.clone()),
        };

        let delivered_a = Arc::new(tokio::sync::Notify::new());
        let delivered_b = Arc::new(tokio::sync::Notify::new());
        let app = Router::new().route(
            "/wh",
            post({
                let notify_a = delivered_a.clone();
                let notify_b = delivered_b.clone();
                move |headers: HeaderMap| {
                    let notify_a = notify_a.clone();
                    let notify_b = notify_b.clone();
                    async move {
                        let evt_id = headers
                            .get("x-mega-event-id")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("");
                        if evt_id == "evt-lock-a" {
                            notify_a.notify_one();
                            // Give the test a brief moment to acquire FOR UPDATE on row A
                            // before HTTP response returns so mark_outbox_delivered blocks.
                            tokio::time::sleep(Duration::from_millis(250)).await;
                        } else if evt_id == "evt-lock-b" {
                            notify_b.notify_one();
                        }
                        axum::http::StatusCode::OK
                    }
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().unwrap();
        let server_handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let now = Utc::now().naive_utc();
        webhook_storage
            .create_webhook(
                mega_webhook::Model {
                    id: 8001,
                    target_url: format!("http://{addr}/wh"),
                    secret: "clb18-transition-lock-secret-32-bytes!!".to_owned(),
                    event_types: "[]".to_owned(),
                    path_filter: Some("/project".to_owned()),
                    active: true,
                    created_at: now,
                    updated_at: now,
                },
                vec![WebhookEventTypeEnum::ClCreated],
            )
            .await
            .unwrap();

        // Seed Row A first (earlier next_retry_at) and Row B second, with max_in_flight = 1.
        seed_outbox_row(
            conn.as_ref(),
            8101,
            "evt-lock-a",
            "pending",
            now,
            now - TimeDelta::seconds(10),
            r#"{"event":"cl.created","row":"a"}"#,
        )
        .await;
        seed_outbox_row(
            conn.as_ref(),
            8102,
            "evt-lock-b",
            "pending",
            now,
            now - TimeDelta::seconds(5),
            r#"{"event":"cl.created","row":"b"}"#,
        )
        .await;

        let cfg = WebhookDeliveryConfig {
            allow_http: true,
            allow_loopback: true,
            max_in_flight: 1,
            total_deadline_secs: 5,
            outbox_lease_secs: 35,
            ..WebhookDeliveryConfig::default()
        };
        let svc = WebhookService::new(webhook_storage, cfg).expect("service");
        let shutdown = CancellationToken::new();
        let worker_handle = svc.spawn_outbox_worker(shutdown.clone());

        // Wait until Row A is in-flight at the mock endpoint, then lock Row A FOR UPDATE
        // so Row A's `mark_outbox_delivered` hits the 2s server-side lock_timeout.
        timeout(Duration::from_secs(5), delivered_a.notified())
            .await
            .expect("row A dispatched");
        let row_a_blocker = conn.begin().await.unwrap();
        row_a_blocker
            .execute_raw(sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT 1 FROM cl_webhook_outbox WHERE id = $1 FOR UPDATE",
                [8101i64.into()],
            ))
            .await
            .unwrap();

        // Despite max_in_flight=1 and Row A remaining locked for the entire time,
        // Row A's transition aborts via lock_timeout and Row B progresses to delivered!
        timeout(Duration::from_secs(8), delivered_b.notified())
            .await
            .expect("unrelated Row B must progress despite Row A transition lock");

        // Poll briefly for Row B's DB state to commit as delivered.
        let mut row_b_delivered = false;
        for _ in 0..20 {
            let b = cl_webhook_outbox::Entity::find_by_id(8102)
                .one(conn.as_ref())
                .await
                .unwrap()
                .unwrap();
            if b.state == "delivered" {
                row_b_delivered = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(row_b_delivered, "Row B must reach delivered state");

        shutdown.cancel();
        let _ = timeout(Duration::from_secs(3), worker_handle).await;
        row_a_blocker.rollback().await.unwrap();
        server_handle.abort();
    }

    #[tokio::test]
    async fn outbox_release_unstarted_leases_returns_claimed_rows_to_pending_in_one_batch() {
        let temp = tempfile::TempDir::new().expect("temp dir");
        let conn = Arc::new(test_db_connection(temp.path()).await);
        apply_migrations(conn.as_ref(), true)
            .await
            .expect("migrations");
        let webhook_storage = WebhookStorage {
            base: BaseStorage::new(conn.clone()),
        };
        let now = Utc::now().naive_utc();

        for (id, ev) in [
            (9101, "evt-rel-1"),
            (9102, "evt-rel-2"),
            (9103, "evt-rel-3"),
        ] {
            seed_outbox_row(
                conn.as_ref(),
                id,
                ev,
                "pending",
                now,
                now - TimeDelta::seconds(5),
                r#"{"event":"cl.created"}"#,
            )
            .await;
        }

        let cfg = WebhookDeliveryConfig {
            max_in_flight: 4,
            total_deadline_secs: 5,
            outbox_lease_secs: 35,
            ..WebhookDeliveryConfig::default()
        };
        let svc = WebhookService::new(webhook_storage, cfg).expect("service");
        let claimed = svc.claim_outbox_batch("worker-release").await.unwrap();
        assert_eq!(claimed.len(), 3);

        // Simulate takeover of row 9103 (bumped lease_epoch + new owner) so
        // bulk release must release 9101 & 9102 while fencing out 9103!
        conn.execute_raw(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE cl_webhook_outbox SET lease_owner = 'worker-takeover', lease_epoch = lease_epoch + 1 WHERE id = 9103",
            [],
        ))
        .await
        .unwrap();

        svc.release_unstarted_leases(&claimed, "worker-release")
            .await;

        let r1 = cl_webhook_outbox::Entity::find_by_id(9101)
            .one(conn.as_ref())
            .await
            .unwrap()
            .unwrap();
        let r2 = cl_webhook_outbox::Entity::find_by_id(9102)
            .one(conn.as_ref())
            .await
            .unwrap()
            .unwrap();
        let r3 = cl_webhook_outbox::Entity::find_by_id(9103)
            .one(conn.as_ref())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(r1.state, "pending");
        assert!(r1.lease_owner.is_none());
        assert!(r1.lease_expires_at.is_none());
        assert_eq!(r2.state, "pending");
        assert!(r2.lease_owner.is_none());
        assert!(r2.lease_expires_at.is_none());
        assert_eq!(r3.state, "leased");
        assert_eq!(r3.lease_owner.as_deref(), Some("worker-takeover"));
    }
}
