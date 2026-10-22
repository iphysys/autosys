//! A clock that only moves when told to.

use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use contracts::Clock;

#[derive(Debug)]
pub struct ManualClock {
    now: Mutex<DateTime<Utc>>,
}

impl ManualClock {
    pub fn new(start: DateTime<Utc>) -> Self {
        Self {
            now: Mutex::new(start),
        }
    }

    /// Starts at 2026-01-01T00:00:00Z.
    pub fn at_epoch() -> Self {
        Self::new(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap())
    }

    pub fn advance(&self, by: Duration) {
        let mut now = self.now.lock().unwrap();
        *now += chrono::TimeDelta::from_std(by).unwrap();
    }

    pub fn set(&self, to: DateTime<Utc>) {
        *self.now.lock().unwrap() = to;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.now.lock().unwrap()
    }
}
