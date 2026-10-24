//! Making the context fit its budget (blueprint section 8, ADR-0029).
//!
//! Compaction here is a rebuild, not a summary. Nothing is condensed by a
//! model: when the context is too big, only items that are not the record of
//! the design are dropped, oldest first, and the context says what is missing.
//! The task, the job's progress and the whole SDR slice are rebuilt from their
//! stores every time, so none of them can be lost.
//!
//! The order is fixed:
//! 1. Each trimmable section is cut to its own cap.
//! 2. What the fixed sections and the open turn leave of the total is the
//!    pool. The open turn may exceed its own cap by borrowing from it.
//! 3. While the trimmable sections together exceed the pool, untrusted items
//!    go first, oldest first, then history items, oldest first.
//!
//! An item is dropped whole, never cut. If the open turn alone does not fit,
//! or nothing is left to drop and the context still does not fit, the build
//! fails with `context_overflow`.

use contracts::types::ContextSectionName;

use crate::error::ContextError;
use crate::render::SectionBody;
use crate::token_budget::TokenCounter;

/// What the total leaves for the trimmable sections.
pub(crate) struct Pool {
    pub total: u64,
    /// Tool definitions and the like, counted by the caller.
    pub reserved: u64,
    /// Instructions, task contract, job state and design state, as they are.
    pub fixed: u64,
    pub open_turn: u64,
}

/// A trimmable section and its own cap.
pub(crate) struct Trim<'a> {
    pub name: ContextSectionName,
    pub body: &'a mut SectionBody,
    pub cap: u64,
}

/// The sizes of the trimmable sections after fitting.
pub(crate) struct Fitted {
    pub run_history: u64,
    pub untrusted_content: u64,
}

fn tokens(body: &SectionBody, counter: &dyn TokenCounter) -> u64 {
    counter.count(&body.render())
}

/// Drops the oldest items until the section fits `cap`.
fn cut_to(section: &mut Trim<'_>, counter: &dyn TokenCounter) -> Result<u64, ContextError> {
    let mut size = tokens(section.body, counter);
    while size > section.cap && section.body.dropped < section.body.items.len() {
        section.body.dropped += 1;
        size = tokens(section.body, counter);
    }
    if size > section.cap {
        // Even the tags and the omission line do not fit the cap.
        return Err(ContextError::Overflow {
            section: section.name,
            needed: size,
            limit: section.cap,
        });
    }
    Ok(size)
}

/// Trims `history` and `untrusted` until the context fits `pool`.
pub(crate) fn fit(
    pool: &Pool,
    mut history: Trim<'_>,
    mut untrusted: Trim<'_>,
    counter: &dyn TokenCounter,
) -> Result<Fitted, ContextError> {
    let mut history_size = cut_to(&mut history, counter)?;
    let mut untrusted_size = cut_to(&mut untrusted, counter)?;

    let used = pool
        .reserved
        .saturating_add(pool.fixed)
        .saturating_add(pool.open_turn);
    let Some(room) = pool.total.checked_sub(used) else {
        // The open turn is the one part that cannot shrink and that the
        // caller controls, so it is the section named.
        return Err(ContextError::Overflow {
            section: ContextSectionName::OpenTurn,
            needed: used,
            limit: pool.total,
        });
    };

    while history_size.saturating_add(untrusted_size) > room {
        if untrusted.body.dropped < untrusted.body.items.len() {
            untrusted.body.dropped += 1;
            untrusted_size = tokens(untrusted.body, counter);
        } else if history.body.dropped < history.body.items.len() {
            history.body.dropped += 1;
            history_size = tokens(history.body, counter);
        } else {
            return Err(ContextError::Overflow {
                section: ContextSectionName::RunHistory,
                needed: history_size.saturating_add(untrusted_size),
                limit: room,
            });
        }
    }
    Ok(Fitted {
        run_history: history_size,
        untrusted_content: untrusted_size,
    })
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use super::*;
    use crate::render::Item;
    use crate::token_budget::ByteEstimate;

    /// One byte is one token, so sizes can be read off the text.
    fn counter() -> ByteEstimate {
        ByteEstimate::new(NonZeroU64::new(1).expect("non-zero"))
    }

    fn body(tag: &str, count: u64, text_len: usize) -> SectionBody {
        let items = (1..=count)
            .map(|seq| Item {
                provenance: format!("event:{seq}"),
                seq: Some(seq),
                text: "x".repeat(text_len),
            })
            .collect();
        SectionBody::new(tag.to_owned(), items)
    }

    fn pool(total: u64) -> Pool {
        Pool {
            total,
            reserved: 0,
            fixed: 0,
            open_turn: 0,
        }
    }

    fn run(
        pool: &Pool,
        history: &mut SectionBody,
        history_cap: u64,
        untrusted: &mut SectionBody,
        untrusted_cap: u64,
    ) -> Result<Fitted, ContextError> {
        fit(
            pool,
            Trim {
                name: ContextSectionName::RunHistory,
                body: history,
                cap: history_cap,
            },
            Trim {
                name: ContextSectionName::UntrustedContent,
                body: untrusted,
                cap: untrusted_cap,
            },
            &counter(),
        )
    }

    #[test]
    fn nothing_is_dropped_when_everything_fits() {
        let mut history = body("<h>", 3, 10);
        let mut untrusted = body("<u>", 2, 10);
        let fitted = run(&pool(1000), &mut history, 500, &mut untrusted, 500).expect("fits");
        assert_eq!((history.dropped, untrusted.dropped), (0, 0));
        assert_eq!(fitted.run_history, counter().count(&history.render()));
        assert_eq!(
            fitted.untrusted_content,
            counter().count(&untrusted.render())
        );
    }

    #[test]
    fn each_section_is_cut_to_its_own_cap_oldest_first() {
        let mut history = body("<h>", 10, 10);
        let mut untrusted = body("<u>", 10, 10);
        // "<h>\n" + note + 10 bytes per kept item + newline + close tag.
        let fitted = run(&pool(10_000), &mut history, 100, &mut untrusted, 10_000).expect("fits");
        assert!(history.dropped > 0 && history.dropped < 10);
        assert!(fitted.run_history <= 100, "{}", fitted.run_history);
        assert_eq!(untrusted.dropped, 0, "the other section keeps its room");
        // The oldest were dropped: the first kept item is the one after them.
        let first = history.kept()[0].seq;
        assert_eq!(
            first,
            Some(u64::try_from(history.dropped).expect("small") + 1)
        );
    }

    /// The size of `body` with its `dropped` oldest items left out.
    fn size_with(body: &SectionBody, dropped: usize) -> u64 {
        let mut body = body.clone();
        body.dropped = dropped;
        counter().count(&body.render())
    }

    #[test]
    fn the_pool_shrinks_untrusted_content_before_history() {
        // Items are large next to the tags and the omission line, so each
        // drop makes a section smaller.
        let mut history = body("<h>", 5, 100);
        let mut untrusted = body("<u>", 5, 100);
        // Room for the history in full and part of the untrusted text.
        let room = size_with(&history, 0) + size_with(&untrusted, 3);
        run(&pool(room), &mut history, 10_000, &mut untrusted, 10_000).expect("fits");
        assert_eq!(
            history.dropped, 0,
            "history is only touched when untrusted is gone"
        );
        assert_eq!(untrusted.dropped, 3);
    }

    #[test]
    fn history_is_trimmed_only_after_untrusted_content_is_gone() {
        let mut history = body("<h>", 5, 100);
        let mut untrusted = body("<u>", 5, 100);
        // Room for no untrusted item and only part of the history.
        let room = size_with(&untrusted, 5) + size_with(&history, 3);
        let fitted = run(&pool(room), &mut history, 10_000, &mut untrusted, 10_000).expect("fits");
        assert_eq!(untrusted.dropped, 5, "all untrusted items go first");
        assert_eq!(history.dropped, 3);
        assert!(fitted.run_history + fitted.untrusted_content <= room);
    }

    #[test]
    fn the_reserved_tokens_and_the_open_turn_come_out_of_the_pool() {
        let mut history = body("<h>", 5, 100);
        let mut untrusted = body("<u>", 0, 0);
        let full = size_with(&history, 0) + size_with(&untrusted, 0);
        let mut tight = pool(full + 100);
        tight.reserved = 60;
        tight.open_turn = 40;
        run(&tight, &mut history, 10_000, &mut untrusted, 10_000).expect("fits exactly");
        assert_eq!(history.dropped, 0);

        // Tokens spent on the fixed sections leave less for the history.
        tight.fixed = 200;
        run(&tight, &mut history, 10_000, &mut untrusted, 10_000).expect("fits");
        assert!(history.dropped > 0);
        assert!(
            size_with(&history, history.dropped) + size_with(&untrusted, 0) <= full - 100,
            "the trimmed context fits the room"
        );
    }

    #[test]
    fn an_open_turn_that_does_not_fit_overflows_instead_of_being_cut() {
        let mut history = body("<h>", 1, 1);
        let mut untrusted = body("<u>", 1, 1);
        let mut over = pool(100);
        over.open_turn = 101;
        match run(&over, &mut history, 1000, &mut untrusted, 1000) {
            Err(ContextError::Overflow {
                section,
                needed,
                limit,
            }) => {
                assert_eq!(section, ContextSectionName::OpenTurn);
                assert_eq!((needed, limit), (101, 100));
            }
            other => panic!("expected Overflow, got {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn overflow_when_even_the_omission_lines_do_not_fit() {
        let mut history = body("<h>", 3, 10);
        let mut untrusted = body("<u>", 3, 10);
        match run(&pool(10), &mut history, 1000, &mut untrusted, 1000) {
            Err(ContextError::Overflow { section, limit, .. }) => {
                assert_eq!(section, ContextSectionName::RunHistory);
                assert_eq!(limit, 10);
            }
            other => panic!("expected Overflow, got {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn a_cap_below_the_bare_section_overflows_that_section() {
        let mut history = body("<history>", 3, 10);
        let mut untrusted = body("<u>", 0, 0);
        match run(&pool(10_000), &mut history, 3, &mut untrusted, 1000) {
            Err(ContextError::Overflow { section, limit, .. }) => {
                assert_eq!(section, ContextSectionName::RunHistory);
                assert_eq!(limit, 3);
            }
            other => panic!("expected Overflow, got {:?}", other.map(|_| ())),
        }
    }
}
