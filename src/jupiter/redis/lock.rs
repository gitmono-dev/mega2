#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::{
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use redis::{Script, aio::ConnectionManager};
use tokio::{
    sync::Notify,
    time::{Duration, sleep},
};
use uuid::Uuid;

use crate::common::errors::MegaError;

/// One acquisition lifecycle of a [`RedLock`]: a unique token plus its own
/// stop signal. The renewal task and the unlock path capture only the lease
/// they were created for, so reusing a `RedLock` instance can never revive a
/// stale renewer — its token no longer matches the key and its stop flag is
/// never re-armed.
struct Lease {
    token: String,
    stop: AtomicBool,
    stop_notify: Notify,
    /// Renewal-loop instrumentation for the generation-race regression tests
    /// (R3). Compiled out in production builds — zero cost there.
    #[cfg(test)]
    test_hooks: Arc<LeaseTestHooks>,
}

impl Lease {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            token: Uuid::new_v4().to_string(),
            stop: AtomicBool::new(false),
            stop_notify: Notify::new(),
            #[cfg(test)]
            test_hooks: Arc::new(LeaseTestHooks::default()),
        })
    }
}

/// Test-only instrumentation hung on a [`Lease`]: lets a test park the
/// renewal loop deterministically in the "about to attempt" window (the
/// missed-stop-signal scenario), counts renewal attempts, and exposes the
/// renewal task's join handle so its exit can be awaited.
#[cfg(test)]
#[derive(Default)]
struct LeaseTestHooks {
    /// Incremented immediately before every renewal attempt.
    renew_attempts: AtomicUsize,
    /// When armed, the renewal loop parks on `gate` right before its next
    /// attempt (one-shot: parking disarms it).
    gate_armed: AtomicBool,
    /// Set while the renewal loop is parked on `gate`.
    gated: AtomicBool,
    gate: Notify,
    /// Set once `unlock_lease` has flagged this lease to stop — the Drop-path
    /// test waits on this instead of guessing with a fixed sleep (R4).
    stop_requested: AtomicBool,
    /// Count of SET NX attempts on this lease (lock-retry backoff tests).
    acquire_attempts: AtomicUsize,
    /// The renewal task's handle, stored right after spawn.
    /// The renewal task's handle, stored right after spawn.
    task: StdMutex<Option<tokio::task::JoinHandle<()>>>,
}

#[cfg(test)]
impl LeaseTestHooks {
    fn renew_attempts(&self) -> usize {
        self.renew_attempts.load(Ordering::SeqCst)
    }

    fn arm_gate(&self) {
        self.gate_armed.store(true, Ordering::SeqCst);
    }

    fn is_gated(&self) -> bool {
        self.gated.load(Ordering::SeqCst)
    }

    /// Release a gated renewer. `notify_one` stores a permit when no waiter
    /// is parked yet, so both interleavings — open-then-park and
    /// park-then-open — let the single consumer through (R4: `notify_waiters`
    /// would lose an early open and wedge the renewer at the gate).
    fn open_gate(&self) {
        self.gate.notify_one();
    }

    fn stop_requested(&self) -> bool {
        self.stop_requested.load(Ordering::SeqCst)
    }

    fn acquire_attempts(&self) -> usize {
        self.acquire_attempts.load(Ordering::SeqCst)
    }

    fn set_task(&self, task: tokio::task::JoinHandle<()>) {
        *self.task.lock().expect("task mutex poisoned") = Some(task);
    }

    /// Await the renewal task's exit (bounded — a stranded task fails fast).
    async fn wait_task_exit(&self) {
        let task = self.task.lock().expect("task mutex poisoned").take();
        let Some(task) = task else {
            panic!("renewal task handle missing")
        };
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("renewal task must exit")
            .expect("renewal task panicked");
    }
}

#[derive(Clone)]
pub struct RedLock {
    connection: ConnectionManager,
    key: String,
    ttl_ms: u64,
    /// The lease of the latest `lock()` acquisition, or the initial lease of
    /// a `try_lock`-only handle. Bare `try_lock`/`unlock` calls operate on
    /// this current lease; `lock()` replaces it with a fresh one.
    current: Arc<StdMutex<Arc<Lease>>>,
}

impl RedLock {
    pub fn new(connection: ConnectionManager, key: impl Into<String>, ttl_ms: u64) -> Self {
        Self {
            connection,
            key: key.into(),
            ttl_ms,
            current: Arc::new(StdMutex::new(Lease::new())),
        }
    }

    fn current_lease(&self) -> Arc<Lease> {
        self.current.lock().expect("lease mutex poisoned").clone()
    }

    /// Test-only handle to the current lease's renewal instrumentation (R3).
    #[cfg(test)]
    fn lease_test_hooks(&self) -> Arc<LeaseTestHooks> {
        self.current_lease().test_hooks.clone()
    }

    /// Try lock: SET key <current lease token> NX PX TTL
    pub async fn try_lock(&self) -> Result<bool, MegaError> {
        self.try_lock_lease(&self.current_lease()).await
    }

    async fn try_lock_lease(&self, lease: &Lease) -> Result<bool, MegaError> {
        #[cfg(test)]
        lease
            .test_hooks
            .acquire_attempts
            .fetch_add(1, Ordering::SeqCst);
        let mut conn = self.connection.clone();
        // SET returns "OK" or Nil
        let result: Option<String> = redis::cmd("SET")
            .arg(&self.key)
            .arg(&lease.token)
            .arg("NX")
            .arg("PX")
            .arg(self.ttl_ms)
            .query_async(&mut conn)
            .await?;

        Ok(result.is_some())
    }

    /// Retry interval while waiting for SET NX. A 200ms sleep turned a ~25ms
    /// hold into 200/400/600ms waits for anyone who missed the unlock.
    const LOCK_RETRY_SLEEP: Duration = Duration::from_millis(10);

    /// Lock with retry
    pub async fn lock(self: Arc<Self>) -> Result<RedLockGuard, MegaError> {
        let t0 = Instant::now();
        // Every acquisition gets a fresh lease: a new token, stop flag and
        // Notify. The renewer and the guard capture this lease only, so a
        // renewer from a previous acquisition on this same instance stays
        // dead (its stop flag remains set and its token no longer matches).
        let lease = Lease::new();
        *self.current.lock().expect("lease mutex poisoned") = lease.clone();
        while !self.try_lock_lease(&lease).await? {
            sleep(Self::LOCK_RETRY_SLEEP).await;
        }

        self.spawn_auto_renew(lease.clone());

        tracing::info!(
            lock_key = %self.key,
            ttl_ms = self.ttl_ms,
            waited_ms = t0.elapsed().as_millis(),
            "redlock acquired"
        );
        Ok(RedLockGuard {
            lock: self,
            lease,
            released: AtomicBool::new(false),
            acquired_at: t0,
        })
    }

    /// Unlock the current lease via Lua atomic script
    pub async fn unlock(&self) -> Result<bool, MegaError> {
        self.unlock_lease(&self.current_lease()).await
    }

    /// Unlock one specific lease: stop its renewer reliably (flag first, then
    /// notify — the renew loop re-checks the flag after every wait, so even a
    /// notification lost while the task is mid-renewal terminates it on the
    /// next iteration), then delete the key only if it still carries this
    /// lease's token.
    async fn unlock_lease(&self, lease: &Lease) -> Result<bool, MegaError> {
        lease.stop.store(true, Ordering::SeqCst);
        lease.stop_notify.notify_waiters();
        // Test-only marker: Drop-path tests wait on this before re-locking
        // the same instance instead of guessing with a fixed sleep (R4).
        #[cfg(test)]
        lease
            .test_hooks
            .stop_requested
            .store(true, Ordering::SeqCst);

        let script = Script::new(
            r#"
                if redis.call("GET", KEYS[1]) == ARGV[1] then
                    return redis.call("DEL", KEYS[1])
                else
                    return 0
                end
            "#,
        );

        let mut conn = self.connection.clone();
        let deleted: i32 = script
            .key(&self.key)
            .arg(&lease.token)
            .invoke_async::<_>(&mut conn)
            .await?;

        Ok(deleted == 1)
    }

    /// Spawn the TTL renew task for one lease.
    ///
    /// Renewal is ownership-checked and atomic: the Lua script refreshes the
    /// TTL only when the lock value still equals this lease's token, so a
    /// stale owner that lost the lock can never extend the new owner's hold.
    /// The task exits when the lease's `unlock` (explicit or guard `Drop`)
    /// stops it, or when a renewal reports the lock is gone / owned by
    /// someone else.
    fn spawn_auto_renew(&self, lease: Arc<Lease>) {
        let mut conn = self.connection.clone();
        let key = self.key.clone();
        let ttl_ms = self.ttl_ms;
        #[cfg(test)]
        let test_hooks = lease.test_hooks.clone();
        let task = tokio::spawn(async move {
            let half = ttl_ms / 2;

            loop {
                tokio::select! {
                    _ = sleep(Duration::from_millis(half)) => {},
                    _ = lease.stop_notify.notified() => break,
                }

                // The notify may have raced the sleep branch; the flag is the
                // source of truth so a lost notification cannot leak the task.
                if lease.stop.load(Ordering::SeqCst) {
                    break;
                }

                // Test-only instrumentation (R3): when the gate is armed, park
                // right before the attempt — the deterministic "missed the
                // stop signal" window — then count the attempt. Compiled out
                // in production builds.
                #[cfg(test)]
                {
                    if lease.test_hooks.gate_armed.load(Ordering::SeqCst) {
                        lease.test_hooks.gate_armed.store(false, Ordering::SeqCst);
                        lease.test_hooks.gated.store(true, Ordering::SeqCst);
                        lease.test_hooks.gate.notified().await;
                        lease.test_hooks.gated.store(false, Ordering::SeqCst);
                    }
                    lease
                        .test_hooks
                        .renew_attempts
                        .fetch_add(1, Ordering::SeqCst);
                }

                // Renew only if we still own the lock (token compare and
                // PEXPIRE in one atomic script). Returns 1 when the TTL was
                // refreshed, 0 when the lock is gone or changed hands.
                let script = Script::new(
                    r#"
                        if redis.call("GET", KEYS[1]) == ARGV[1] then
                            return redis.call("PEXPIRE", KEYS[1], ARGV[2])
                        else
                            return 0
                        end
                    "#,
                );
                let renewed: i32 = match script
                    .key(&key)
                    .arg(&lease.token)
                    .arg(ttl_ms)
                    .invoke_async::<_>(&mut conn)
                    .await
                {
                    Ok(renewed) => renewed,
                    Err(e) => {
                        // Transient Redis error: keep renewing while we still
                        // hold the lock (next window re-checks ownership).
                        tracing::warn!(lock_key = %key, "redlock renewal failed: {e}");
                        continue;
                    }
                };
                if renewed == 0 {
                    tracing::warn!(
                        lock_key = %key,
                        "redlock renewal stopped: lock lost or ownership changed"
                    );
                    break;
                }
            }
        });
        #[cfg(test)]
        test_hooks.set_task(task);
        #[cfg(not(test))]
        let _task = task; // detach: the join handle is only used by tests
    }
}

pub struct RedLockGuard {
    lock: Arc<RedLock>,
    /// The acquisition this guard owns; renewal and unlock act on it only.
    lease: Arc<Lease>,
    /// Tracks whether the lock has been explicitly released to prevent double-unlock
    released: AtomicBool,
    acquired_at: Instant,
}

impl RedLockGuard {
    pub async fn unlock(self) -> Result<(), MegaError> {
        // If already released, return immediately
        if self.released.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        tracing::info!(
            lock_key = %self.lock.key,
            held_ms = self.acquired_at.elapsed().as_millis(),
            "redlock unlocking"
        );
        self.lock.unlock_lease(&self.lease).await?;
        Ok(())
    }
}

impl Drop for RedLockGuard {
    fn drop(&mut self) {
        // Cannot await in Drop, so spawn an async task
        let lock = self.lock.clone();
        let lease = self.lease.clone();
        let acquired_at = self.acquired_at;

        // Only unlock if it hasn't been released already (atomically set the flag)
        if !self.released.swap(true, Ordering::SeqCst) {
            tokio::spawn(async move {
                tracing::info!(
                    lock_key = %lock.key,
                    held_ms = acquired_at.elapsed().as_millis(),
                    "redlock dropping guard; unlocking"
                );
                let _ = lock.unlock_lease(&lease).await;
            });
        }
    }
}

#[cfg(test)]
mod tests {

    use std::{process::Command, sync::Arc, time::Instant};

    use futures::future::join_all;
    use redis::{AsyncCommands, aio::ConnectionManager};
    use redis_test::server::RedisServer;
    use tokio::time::{Duration, sleep, timeout};

    use crate::jupiter::redis::lock::RedLock;

    fn redis_server_available() -> bool {
        Command::new("redis-server")
            .arg("--version")
            .output()
            .is_ok()
    }

    async fn init_server() -> Option<(RedisServer, ConnectionManager)> {
        if !redis_server_available() {
            eprintln!("redis-server not found; skipping redis lock tests");
            return None;
        }
        let server = RedisServer::new();
        let url = server.client_addr().to_owned();
        println!("starting redis mock server at: {}", url);
        let client = redis::Client::open(url).unwrap();
        let conn = ConnectionManager::new(client).await.unwrap();
        Some((server, conn))
    }

    #[tokio::test]
    async fn test_basic() {
        let Some((_server, mut conn)) = init_server().await else {
            return;
        };
        conn.set::<&str, _, String>("foo", "bar").await.unwrap();
        let v: String = conn.get("foo").await.unwrap();
        assert_eq!(v, "bar");
    }

    #[test]
    fn lock_retry_sleep_is_ten_millis() {
        assert_eq!(RedLock::LOCK_RETRY_SLEEP, Duration::from_millis(10));
    }

    #[tokio::test]
    async fn contended_lock_uses_short_backoff_not_busy_spin() {
        let Some((_server, conn)) = init_server().await else {
            return;
        };

        let holder = Arc::new(RedLock::new(conn.clone(), "backoff".to_string(), 3000));
        let waiter = Arc::new(RedLock::new(conn, "backoff".to_string(), 3000));
        let guard = holder.clone().lock().await.unwrap();

        let started = Instant::now();
        let waiter_task = tokio::spawn(async move { waiter.lock().await.unwrap() });
        sleep(Duration::from_millis(50)).await;
        guard.unlock().await.unwrap();
        let waiter_guard = timeout(Duration::from_millis(120), waiter_task)
            .await
            .expect("200ms lock retry would miss a 50ms hold")
            .expect("waiter task");
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(50),
            "waiter must not acquire while the holder still owns the key"
        );
        assert!(
            waited < Duration::from_millis(150),
            "10ms backoff should acquire shortly after unlock, not after 200ms: {waited:?}"
        );

        let attempts = waiter_guard.lock.lease_test_hooks().acquire_attempts();
        assert!(
            attempts >= 2,
            "waiter should SET NX more than once while blocked: {attempts}"
        );
        assert!(
            attempts <= 20,
            "busy-spin would issue thousands of SET NX calls in 50ms, got {attempts}"
        );
        waiter_guard.unlock().await.unwrap();
    }

    #[tokio::test]
    async fn test_try_lock() {
        let Some((_server, conn)) = init_server().await else {
            return;
        };
        let lock = Arc::new(RedLock::new(conn, "try_lock".to_string(), 3000));
        assert!(lock.try_lock().await.unwrap());
        assert!(!lock.try_lock().await.unwrap());
    }

    #[tokio::test]
    async fn test_unlock_script() {
        let Some((_server, conn)) = init_server().await else {
            return;
        };
        let lock = Arc::new(RedLock::new(conn, "unlock_script".to_string(), 3000));
        lock.try_lock().await.unwrap();
        assert!(lock.unlock().await.unwrap());
    }

    #[tokio::test]
    async fn test_auto_renew() {
        let Some((_server, mut conn)) = init_server().await else {
            return;
        };

        let lock = Arc::new(RedLock::new(conn.clone(), "renew".to_string(), 1000));

        let _guard = lock.clone().lock().await.unwrap();

        tokio::time::sleep(Duration::from_secs(2)).await;

        let ttl: i64 = redis::cmd("PTTL")
            .arg("renew")
            .query_async(&mut conn)
            .await
            .unwrap();

        assert!(ttl > 0);
    }

    #[tokio::test]
    async fn test_concurrent_locks() {
        let Some((_server, conn)) = init_server().await else {
            return;
        };

        let mut tasks = vec![];

        for _ in 0..64 {
            let lock = Arc::new(RedLock::new(conn.clone(), "race".to_string(), 3000));
            tasks.push(tokio::spawn(async move { lock.try_lock().await.unwrap() }));
        }

        let results = futures::future::join_all(tasks).await;

        let success_count = results
            .into_iter()
            .filter(|r| r.as_ref().unwrap() == &true)
            .count();

        assert_eq!(success_count, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn test_multi_pod_lock_sequential_acquisition() {
        let Some((_server, conn)) = init_server().await else {
            return;
        };

        let mut tasks = vec![];

        for pod_id in 0..3 {
            let conn = conn.clone();
            tasks.push(tokio::spawn(async move {
                let lock = Arc::new(RedLock::new(conn, "test-lock".to_string(), 1000));
                for i in 0..2 {
                    let guard = lock.clone().lock().await.unwrap();

                    println!("[pod-{pod_id}] acquired lock {i}");

                    // Sleep for longer than the TTL (1000ms) to test auto-renewal, but keep test fast.
                    sleep(Duration::from_millis(1200)).await;

                    guard.unlock().await.unwrap();
                }

                pod_id
            }));
        }

        let result = join_all(tasks).await;

        for r in result {
            assert!(r.is_ok());
        }
    }

    #[tokio::test]
    async fn test_single_pod_lock_starvation() {
        let Some((_server, conn)) = init_server().await else {
            return;
        };
        let lock = Arc::new(RedLock::new(conn, "test_lock", 1000));
        let guard = lock.clone().lock().await.unwrap();
        drop(guard);

        // Give the background unlock task time to complete to avoid race condition.
        sleep(Duration::from_millis(500)).await;

        let result = timeout(Duration::from_secs(2), lock.clone().lock()).await;
        match result {
            Ok(_) => println!("acquire lock successful"),
            Err(_) => panic!("acquire lock timeout, drop guard didn't unlock"),
        }
    }

    /// A stale owner must not renew a lock it lost: the renewal script
    /// compares the token atomically and the renewer stops once the lock has
    /// changed hands. A takes the lock (renewer running), B force-takes it
    /// with a fresh token and never renews (`try_lock` spawns no renewer) —
    /// if A's still-running renewer could extend B's hold, the key would
    /// survive past B's TTL.
    #[tokio::test]
    async fn test_stale_owner_cannot_renew_after_ownership_change() {
        let Some((_server, mut conn)) = init_server().await else {
            return;
        };

        let ttl_ms = 900;
        let key = "stale-renew";
        let lock_a = Arc::new(RedLock::new(conn.clone(), key, ttl_ms));
        let _guard_a = lock_a.clone().lock().await.unwrap();

        // Failover: A loses the lock (crash / TTL expiry) and B takes it.
        let _: () = conn.del(key).await.unwrap();
        let lock_b = Arc::new(RedLock::new(conn.clone(), key, ttl_ms));
        assert!(lock_b.try_lock().await.unwrap());
        let owner: String = conn.get(key).await.unwrap();
        assert_eq!(owner, lock_b.current_lease().token, "B owns the lock now");

        // Past B's TTL the key must be gone; A's renewer firing at ttl/2 must
        // have refused to extend a lock whose token is no longer A's.
        sleep(Duration::from_millis(ttl_ms + 400)).await;
        let after: Option<String> = conn.get(key).await.unwrap();
        assert_eq!(
            after, None,
            "the stale owner's renewer must not extend the new owner's lock"
        );
    }

    /// An explicit unlock terminates the renewer: a subsequent owner's lock
    /// with the same name must expire naturally instead of being extended by
    /// a leaked renewal task.
    #[tokio::test]
    async fn test_no_renew_task_left_after_explicit_unlock() {
        let Some((_server, mut conn)) = init_server().await else {
            return;
        };

        let ttl_ms = 900;
        let key = "unlock-stops-renewer";
        let lock = Arc::new(RedLock::new(conn.clone(), key, ttl_ms));
        let guard = lock.clone().lock().await.unwrap();
        guard.unlock().await.unwrap();

        let lock_b = Arc::new(RedLock::new(conn.clone(), key, ttl_ms));
        assert!(lock_b.try_lock().await.unwrap());
        sleep(Duration::from_millis(ttl_ms + 400)).await;
        let after: Option<String> = conn.get(key).await.unwrap();
        assert_eq!(after, None, "no renewer may survive unlock");
    }

    /// Same guarantee through the early-exit Drop path: the spawned unlock
    /// stops the renewer, so a subsequent owner's lock expires naturally.
    #[tokio::test]
    async fn test_no_renew_task_left_after_guard_drop() {
        let Some((_server, mut conn)) = init_server().await else {
            return;
        };

        let ttl_ms = 900;
        let key = "drop-stops-renewer";
        let lock = Arc::new(RedLock::new(conn.clone(), key, ttl_ms));
        let guard = lock.clone().lock().await.unwrap();
        drop(guard);
        // The guard's Drop spawns the unlock task; give it time to run.
        sleep(Duration::from_millis(500)).await;

        let lock_b = Arc::new(RedLock::new(conn.clone(), key, ttl_ms));
        assert!(lock_b.try_lock().await.unwrap());
        sleep(Duration::from_millis(ttl_ms + 400)).await;
        let after: Option<String> = conn.get(key).await.unwrap();
        assert_eq!(after, None, "no renewer may survive the guard drop");
    }

    /// Same-instance reuse (R2): an old acquisition's renewer must never
    /// revive when the SAME `RedLock` instance is re-locked. Explicit-unlock
    /// path: unlock, then immediately re-acquire on the same instance through
    /// `try_lock` (which spawns no renewer) — past the TTL the key must be
    /// gone, because any zombie renewer from the first hold would have kept
    /// extending it (the token matched before the per-lease fix).
    #[tokio::test]
    async fn test_same_instance_relock_after_unlock_leaves_no_zombie_renewer() {
        let Some((_server, mut conn)) = init_server().await else {
            return;
        };

        let ttl_ms = 900;
        let key = "same-instance-relock-unlock";
        let lock = Arc::new(RedLock::new(conn.clone(), key, ttl_ms));
        let guard = lock.clone().lock().await.unwrap();
        guard.unlock().await.unwrap();

        // Immediately re-acquire on the same instance, without a renewer.
        assert!(lock.try_lock().await.unwrap());
        sleep(Duration::from_millis(ttl_ms + 400)).await;
        let after: Option<String> = conn.get(key).await.unwrap();
        assert_eq!(
            after, None,
            "a zombie renewer from the first hold extended the re-acquired lock"
        );
    }

    /// Same-instance reuse through the Drop early-exit path: the spawned
    /// unlock stops the old lease's renewer; the re-acquired (renewer-less)
    /// lock expires naturally.
    #[tokio::test]
    async fn test_same_instance_relock_after_guard_drop_leaves_no_zombie_renewer() {
        let Some((_server, mut conn)) = init_server().await else {
            return;
        };

        let ttl_ms = 900;
        let key = "same-instance-relock-drop";
        let lock = Arc::new(RedLock::new(conn.clone(), key, ttl_ms));
        let guard = lock.clone().lock().await.unwrap();
        drop(guard);
        // The guard's Drop spawns the unlock task; give it time to run.
        sleep(Duration::from_millis(500)).await;

        assert!(lock.try_lock().await.unwrap());
        sleep(Duration::from_millis(ttl_ms + 400)).await;
        let after: Option<String> = conn.get(key).await.unwrap();
        assert_eq!(
            after, None,
            "a zombie renewer from the dropped guard extended the re-acquired lock"
        );
    }

    /// Spin until `cond` holds (bounded — a stuck generation handoff fails
    /// fast instead of hanging the suite).
    async fn wait_until(description: &str, mut cond: impl FnMut() -> bool) {
        timeout(Duration::from_secs(5), async {
            while !cond() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {description}"));
    }

    /// R3: generation isolation on same-instance reuse, explicit-unlock path.
    /// The generation-1 renewer is parked deterministically in the "about to
    /// attempt" window (gate armed), the stop notification is lost (it waits
    /// on the gate, not on `stop_notify`), and the same instance is then
    /// re-locked via `lock()` — under the pre-lease design that reset the
    /// shared stop flag and the old renewer would revive and keep renewing
    /// (its attempt counter would keep growing and the task would never
    /// exit, failing `wait_task_exit` / the frozen-counter assertion). With
    /// per-lease state the stranded attempt is rejected by the token compare
    /// and the renewer exits; generation 2's lock stays alive on generation
    /// 2's renewals only.
    #[tokio::test]
    async fn test_same_instance_relock_after_unlock_strands_old_lease_renewer() {
        let Some((_server, mut conn)) = init_server().await else {
            return;
        };

        let ttl_ms = 900;
        let key = "same-instance-lease-unlock";
        let lock = Arc::new(RedLock::new(conn.clone(), key, ttl_ms));

        let guard1 = lock.clone().lock().await.unwrap();
        let hooks1 = lock.lease_test_hooks();
        // Pin gen-1's renewer at the gate before its first attempt.
        hooks1.arm_gate();
        wait_until("gen-1 renewer parked at the gate", || hooks1.is_gated()).await;

        // The unlock's stop notify is lost on the gated renewer — the exact
        // missed-signal window — then the same instance is re-locked.
        guard1.unlock().await.unwrap();
        let guard2 = lock.clone().lock().await.unwrap();
        let hooks2 = lock.lease_test_hooks();

        // Release the stranded renewer: it must exit after at most the one
        // in-flight attempt (rejected by the token compare).
        hooks1.open_gate();
        hooks1.wait_task_exit().await;
        let gen1_attempts = hooks1.renew_attempts();
        assert!(
            (1..=2).contains(&gen1_attempts),
            "gen-1 may attempt at most its stranded renewal, got {gen1_attempts}"
        );

        // Past gen-2's TTL the lock is still alive — extended only by
        // gen-2's renewer — while gen-1's attempt counter stays frozen.
        sleep(Duration::from_millis(ttl_ms + 400)).await;
        assert_eq!(
            hooks1.renew_attempts(),
            gen1_attempts,
            "gen-1 renewer must stay dead after the relock"
        );
        assert!(
            hooks2.renew_attempts() >= 1,
            "gen-2 renewer must be the one extending the lock"
        );
        let owner: String = conn.get(key).await.unwrap();
        assert_eq!(
            owner,
            lock.current_lease().token,
            "the live lock belongs to generation 2"
        );
        guard2.unlock().await.unwrap();
    }

    /// R3: same guarantee through the guard-Drop early-exit path — the
    /// spawned unlock loses its notify on the gated gen-1 renewer, the
    /// instance is re-locked via `lock()`, and the old lease's renewer exits
    /// after its stranded (token-rejected) attempt. The relock is gated on
    /// the `stop_requested` hook (R4), not on a fixed sleep, so the
    /// interleaving is constructed deterministically.
    #[tokio::test]
    async fn test_same_instance_relock_after_guard_drop_strands_old_lease_renewer() {
        let Some((_server, mut conn)) = init_server().await else {
            return;
        };

        let ttl_ms = 900;
        let key = "same-instance-lease-drop";
        let lock = Arc::new(RedLock::new(conn.clone(), key, ttl_ms));

        let guard1 = lock.clone().lock().await.unwrap();
        let hooks1 = lock.lease_test_hooks();
        hooks1.arm_gate();
        wait_until("gen-1 renewer parked at the gate", || hooks1.is_gated()).await;

        drop(guard1);
        // The guard's Drop spawns the unlock task; wait until it has actually
        // flagged gen-1's lease to stop before re-locking (the relock's
        // try-lock retry loop covers the DEL still being in flight).
        wait_until("gen-1 stop requested", || hooks1.stop_requested()).await;
        let guard2 = lock.clone().lock().await.unwrap();
        let hooks2 = lock.lease_test_hooks();

        hooks1.open_gate();
        hooks1.wait_task_exit().await;
        let gen1_attempts = hooks1.renew_attempts();
        assert!(
            (1..=2).contains(&gen1_attempts),
            "gen-1 may attempt at most its stranded renewal, got {gen1_attempts}"
        );

        sleep(Duration::from_millis(ttl_ms + 400)).await;
        assert_eq!(
            hooks1.renew_attempts(),
            gen1_attempts,
            "gen-1 renewer must stay dead after the relock"
        );
        assert!(
            hooks2.renew_attempts() >= 1,
            "gen-2 renewer must be the one extending the lock"
        );
        let owner: String = conn.get(key).await.unwrap();
        assert_eq!(
            owner,
            lock.current_lease().token,
            "the live lock belongs to generation 2"
        );
        guard2.unlock().await.unwrap();
    }
}
