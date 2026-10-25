use std::num::NonZeroU64;
use std::time::Duration;

use async_trait::async_trait;
use contracts::ids::RunId;
use contracts::types::{Event, EventSchemaVersion};
use contracts::{
    Error, ErrorCode, EventKind, EventLog, ExecutionContext, NewEvent, StoreWrite, WriteAuthority,
    authorize, completion_key,
};
use sqlx::postgres::PgListener;
use sqlx::types::Json;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};

use super::Database;
use super::error::{corrupt, db_error, from_bigint, to_bigint};
use super::fence::verify_fence;

/// Notified by the `events_notify` trigger, with the run ID as payload.
const CHANNEL: &str = "autosys_events";
/// Notices buffered per waiter. A waiter that falls further behind re-checks
/// the log instead.
const NOTICE_BUFFER: usize = 256;
/// How often a waiter re-checks the log without a notice, in case one was
/// lost while the listener reconnected.
const RECHECK: Duration = Duration::from_secs(2);
/// Pause before the listener retries after a failed reconnect.
const RETRY_DELAY: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
enum Notice {
    /// An event was stored for this run.
    Appended(String),
    /// Notifications may have been missed; every waiter re-checks.
    Resync,
}

/// [`EventLog`] on Postgres. Sequence numbers come from a per-run counter row
/// that each append locks until commit, so they are gap-free and a run's
/// events become visible in sequence order. A completion event's key is
/// stored with it under a unique index, and checked under the same lock.
///
/// Waiting uses `LISTEN`: one listener connection per log, shared by all of
/// its waiters, so a waiting reader does not hold a pooled connection.
pub struct PgEventLog {
    db: Database,
    notices: broadcast::Sender<Notice>,
    listener: JoinHandle<()>,
}

impl PgEventLog {
    /// Starts listening before it returns, so no append committed after that
    /// is missed by a waiter.
    pub async fn connect(db: Database) -> Result<Self, Error> {
        let listener = db
            .bounded("listen for events", async {
                let mut listener = PgListener::connect_with(&db.pool)
                    .await
                    .map_err(|e| db_error("listen for events", e))?;
                listener
                    .listen(CHANNEL)
                    .await
                    .map_err(|e| db_error("listen for events", e))?;
                Ok(listener)
            })
            .await?;
        let (notices, _) = broadcast::channel(NOTICE_BUFFER);
        let listener = tokio::spawn(forward(listener, notices.clone()));
        Ok(Self {
            db,
            notices,
            listener,
        })
    }
}

impl Drop for PgEventLog {
    fn drop(&mut self) {
        self.listener.abort();
    }
}

/// Passes notifications on to waiters until the pool closes. Reconnects after
/// a lost connection and tells waiters to re-check, since notifications sent
/// meanwhile are gone.
async fn forward(mut listener: PgListener, notices: broadcast::Sender<Notice>) {
    loop {
        // A send fails only when nobody is waiting, which is fine.
        match listener.try_recv().await {
            Ok(Some(notification)) => {
                let _ = notices.send(Notice::Appended(notification.payload().to_owned()));
            }
            Ok(None) => {
                tracing::warn!("event listener lost its connection; reconnecting");
                let _ = notices.send(Notice::Resync);
            }
            Err(sqlx::Error::PoolClosed) => return,
            Err(error) => {
                tracing::warn!(%error, "event listener failed; retrying");
                let _ = notices.send(Notice::Resync);
                tokio::time::sleep(RETRY_DELAY).await;
            }
        }
    }
}

#[async_trait]
impl EventLog for PgEventLog {
    async fn append(
        &self,
        event: NewEvent,
        authority: Option<&WriteAuthority>,
    ) -> Result<Event, Error> {
        let fence = authorize(authority, StoreWrite::Event(&event.payload))?;
        let key = completion_key(&event.payload);
        self.db
            .bounded("append event", async {
                let mut tx = self
                    .db
                    .pool
                    .begin()
                    .await
                    .map_err(|e| db_error("append event", e))?;
                if let Some(fence) = fence {
                    verify_fence(&mut tx, fence, "append event").await?;
                }
                // Locks the run's counter row until commit: concurrent appends
                // to the run wait here, then take the next number.
                let seq = sqlx::query_scalar!(
                    r#"
                    INSERT INTO event_sequences (run_id, last_seq) VALUES ($1, 1)
                    ON CONFLICT (run_id) DO UPDATE SET last_seq = event_sequences.last_seq + 1
                    RETURNING last_seq
                    "#,
                    event.run_id.as_str(),
                )
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| db_error("append event", e))?;
                if let Some(key) = &key {
                    // Under the counter lock, an earlier append of this key
                    // has committed if it exists. Rolling back undoes the
                    // number just taken, so no gap is left.
                    let stored = sqlx::query_scalar!(
                        r#"
                        SELECT document AS "document!: Json<Event>"
                        FROM events
                        WHERE run_id = $1 AND dedupe_key = $2
                        "#,
                        event.run_id.as_str(),
                        key,
                    )
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|e| db_error("append event", e))?;
                    if let Some(stored) = stored {
                        tx.rollback()
                            .await
                            .map_err(|e| db_error("append event", e))?;
                        return Ok(stored.0);
                    }
                }
                let seq = NonZeroU64::new(from_bigint(seq, "sequence number")?)
                    .ok_or_else(|| corrupt("stored sequence number is 0"))?;
                let stored = Event {
                    schema_version: EventSchemaVersion::X01,
                    run_id: event.run_id,
                    seq,
                    occurred_at: event.occurred_at,
                    correlation_id: event.correlation_id,
                    actor: event.actor,
                    payload: event.payload,
                };
                sqlx::query!(
                    "INSERT INTO events (run_id, seq, document, dedupe_key) VALUES ($1, $2, $3, $4)",
                    stored.run_id.as_str(),
                    to_bigint(seq.get(), "sequence number")?,
                    Json(&stored) as _,
                    key.as_deref(),
                )
                .execute(&mut *tx)
                .await
                .map_err(|e| db_error("append event", e))?;
                tx.commit().await.map_err(|e| db_error("append event", e))?;
                Ok(stored)
            })
            .await
    }

    async fn read(&self, run_id: &RunId, after: u64, limit: usize) -> Result<Vec<Event>, Error> {
        // Past the bigint range there is nothing to read.
        let Ok(after) = i64::try_from(after) else {
            return Ok(Vec::new());
        };
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.db
            .bounded("read events", async {
                let rows = sqlx::query_scalar!(
                    r#"
                    SELECT document AS "document!: Json<Event>"
                    FROM events
                    WHERE run_id = $1 AND seq > $2
                    ORDER BY seq
                    LIMIT $3
                    "#,
                    run_id.as_str(),
                    after,
                    limit,
                )
                .fetch_all(&self.db.pool)
                .await
                .map_err(|e| db_error("read events", e))?;
                Ok(rows.into_iter().map(|row| row.0).collect())
            })
            .await
    }

    async fn read_kinds(
        &self,
        run_id: &RunId,
        after: u64,
        kinds: &[EventKind],
        limit: usize,
    ) -> Result<Vec<Event>, Error> {
        let Ok(after) = i64::try_from(after) else {
            return Ok(Vec::new());
        };
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let kinds: Vec<String> = kinds.iter().map(|k| k.as_str().to_owned()).collect();
        self.db
            .bounded("read events by kind", async {
                let rows = sqlx::query_scalar!(
                    r#"
                    SELECT document AS "document!: Json<Event>"
                    FROM events
                    WHERE run_id = $1 AND kind = ANY($2) AND seq > $3
                    ORDER BY seq
                    LIMIT $4
                    "#,
                    run_id.as_str(),
                    &kinds,
                    after,
                    limit,
                )
                .fetch_all(&self.db.pool)
                .await
                .map_err(|e| db_error("read events by kind", e))?;
                Ok(rows.into_iter().map(|row| row.0).collect())
            })
            .await
    }

    async fn last_seq(&self, run_id: &RunId) -> Result<u64, Error> {
        self.db
            .bounded("read last sequence number", async {
                let last = sqlx::query_scalar!(
                    "SELECT last_seq FROM event_sequences WHERE run_id = $1",
                    run_id.as_str(),
                )
                .fetch_optional(&self.db.pool)
                .await
                .map_err(|e| db_error("read last sequence number", e))?;
                last.map_or(Ok(0), |seq| from_bigint(seq, "sequence number"))
            })
            .await
    }

    async fn wait_for(
        &self,
        run_id: &RunId,
        after: u64,
        ctx: &ExecutionContext,
    ) -> Result<(), Error> {
        ctx.ensure_not_cancelled()?;
        // Subscribe before checking, so an append between the check and the
        // wait is not missed.
        let mut notices = self.notices.subscribe();
        let mut recheck = tokio::time::interval_at(Instant::now() + RECHECK, RECHECK);
        recheck.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            if self.last_seq(run_id).await? > after {
                return Ok(());
            }
            loop {
                tokio::select! {
                    () = ctx.cancelled() => return ctx.ensure_not_cancelled(),
                    _ = recheck.tick() => break,
                    notice = notices.recv() => match notice {
                        Ok(Notice::Appended(run)) if run == run_id.as_str() => break,
                        Ok(Notice::Appended(_)) => {}
                        Ok(Notice::Resync) | Err(RecvError::Lagged(_)) => break,
                        Err(RecvError::Closed) => {
                            return Err(Error::new(
                                ErrorCode::Unavailable,
                                "the event listener stopped",
                            ));
                        }
                    },
                }
            }
        }
    }
}
