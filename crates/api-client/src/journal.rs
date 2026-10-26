//! A run's events on disk, one JSON line each, which doubles as the cursor
//! of a client that stops and starts again (ADR-0034).
//!
//! Each event is appended in a single write, before the caller shows it to
//! anyone, so a process killed at any point leaves every event it received
//! once, at most followed by a torn line. Opening the journal drops a torn
//! last line; the next stream starts after the last complete one.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use contracts::types::Event;

use crate::ClientError;

/// A run's received events, `1..=last_seq` without gaps.
#[derive(Debug)]
pub struct EventJournal {
    path: PathBuf,
    file: File,
    last_seq: u64,
}

impl EventJournal {
    /// Opens the journal at `path`, creating it and its directory if
    /// needed. A torn last line is removed. Fails if a complete line is not
    /// an event or the events are not `1, 2, 3, ...`.
    pub fn open(path: &Path) -> Result<Self, ClientError> {
        let fail = |reason: String| ClientError::Journal {
            path: path.to_owned(),
            reason,
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| fail(format!("cannot create: {e}")))?;
        }
        // Read and write rather than append: Windows refuses to truncate a
        // file opened for appending. Every write goes to the end anyway.
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|e| fail(format!("cannot open: {e}")))?;
        let mut text = String::new();
        file.read_to_string(&mut text)
            .map_err(|e| fail(format!("cannot read: {e}")))?;
        let complete = text.rfind('\n').map_or(0, |end| end + 1);
        if complete < text.len() {
            tracing::warn!(path = %path.display(), "dropping a torn last line");
            file.set_len(complete as u64)
                .map_err(|e| fail(format!("cannot drop the torn last line: {e}")))?;
        }
        file.seek(SeekFrom::End(0))
            .map_err(|e| fail(format!("cannot seek to the end: {e}")))?;
        let mut last_seq = 0;
        for (number, line) in text[..complete].lines().enumerate() {
            let event: Event = serde_json::from_str(line)
                .map_err(|e| fail(format!("line {}: not an event: {e}", number + 1)))?;
            let seq = event.seq.get();
            if seq != last_seq + 1 {
                return Err(fail(format!(
                    "line {}: event {seq} follows event {last_seq}",
                    number + 1
                )));
            }
            last_seq = seq;
        }
        Ok(Self {
            path: path.to_owned(),
            file,
            last_seq,
        })
    }

    /// The sequence number of the last event, 0 if there is none.
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends `event`, which must be the one after the last.
    pub fn append(&mut self, event: &Event) -> Result<(), ClientError> {
        let seq = event.seq.get();
        let fail = |reason: String| ClientError::Journal {
            path: self.path.clone(),
            reason,
        };
        if seq != self.last_seq + 1 {
            return Err(fail(format!(
                "event {seq} cannot follow event {}",
                self.last_seq
            )));
        }
        let mut line = serde_json::to_string(event)
            .map_err(|e| fail(format!("cannot serialize event {seq}: {e}")))?;
        line.push('\n');
        self.file
            .write_all(line.as_bytes())
            .map_err(|e| fail(format!("cannot append event {seq}: {e}")))?;
        self.last_seq = seq;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "tests")]

    use serde_json::json;

    use super::*;

    fn event(seq: u64) -> Event {
        serde_json::from_value(json!({
            "schema_version": "0.1",
            "run_id": "run-1",
            "seq": seq,
            "occurred_at": "2026-10-26T00:00:00Z",
            "correlation_id": "c-1",
            "actor": { "kind": "user", "user_id": "dev" },
            "payload": { "type": "approval_answered", "approval_id": "a-1", "answer": "approve" }
        }))
        .expect("an event")
    }

    fn journal() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("a directory");
        let path = dir.path().join("runs/run-1/events.jsonl");
        (dir, path)
    }

    #[test]
    fn a_new_journal_starts_at_zero_and_resumes_where_it_stopped() {
        let (_dir, path) = journal();
        let mut journal = EventJournal::open(&path).expect("created");
        assert_eq!(journal.last_seq(), 0);
        for seq in 1..=3 {
            journal.append(&event(seq)).expect("appended");
        }
        drop(journal);
        let journal = EventJournal::open(&path).expect("reopened");
        assert_eq!(journal.last_seq(), 3);
    }

    #[test]
    fn a_torn_last_line_is_dropped() {
        let (_dir, path) = journal();
        let mut journal = EventJournal::open(&path).expect("created");
        journal.append(&event(1)).expect("appended");
        drop(journal);
        let whole = serde_json::to_string(&event(2)).expect("json");
        let mut file = OpenOptions::new().append(true).open(&path).expect("open");
        file.write_all(&whole.as_bytes()[..whole.len() / 2])
            .expect("a torn write");
        drop(file);

        let mut journal = EventJournal::open(&path).expect("reopened");
        assert_eq!(journal.last_seq(), 1);
        journal.append(&event(2)).expect("event 2 again");
        drop(journal);
        let text = std::fs::read_to_string(&path).expect("read");
        assert_eq!(text.lines().count(), 2);
        assert_eq!(EventJournal::open(&path).expect("again").last_seq(), 2);
    }

    #[test]
    fn only_the_next_event_can_be_appended() {
        let (_dir, path) = journal();
        let mut journal = EventJournal::open(&path).expect("created");
        journal.append(&event(1)).expect("appended");
        assert!(matches!(
            journal.append(&event(1)),
            Err(ClientError::Journal { .. })
        ));
        assert!(matches!(
            journal.append(&event(3)),
            Err(ClientError::Journal { .. })
        ));
        assert_eq!(journal.last_seq(), 1);
    }

    #[test]
    fn a_journal_with_a_gap_or_a_bad_line_is_refused() {
        let (_dir, path) = journal();
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("dir");
        let lines = [&event(1), &event(3)]
            .map(|e| serde_json::to_string(e).expect("json"))
            .join("\n");
        std::fs::write(&path, format!("{lines}\n")).expect("written");
        assert!(EventJournal::open(&path).is_err());
        std::fs::write(&path, "not json\n").expect("written");
        assert!(EventJournal::open(&path).is_err());
    }
}
