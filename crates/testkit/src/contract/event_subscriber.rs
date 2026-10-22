use chrono::{TimeZone, Utc};
use contracts::EventSubscriber;

use super::ctx;
use crate::builders::every_event;

/// The `EventSubscriber` contract: it accepts an event of every type.
pub async fn event_subscriber(subscriber: &dyn EventSubscriber) {
    let at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    for event in every_event("run-contract", at) {
        subscriber
            .on_event(&event, &ctx())
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "subscriber {}: rejected event {:?}: {e}",
                    subscriber.id().as_str(),
                    event.payload
                )
            });
    }
}
