//! `contracts::EventKind` names each payload as the event schema tags it.

use std::collections::HashSet;

use contracts::EventKind;
use contracts::types::EventPayload;
use testkit::builders::{every_event_payload, parse};

#[test]
fn every_payload_has_the_kind_its_type_tag_names() {
    let mut kinds = HashSet::new();
    for value in every_event_payload() {
        let payload: EventPayload = parse(value.clone());
        let kind = EventKind::of(&payload);
        assert_eq!(Some(kind.as_str()), value["type"].as_str(), "{value}");
        kinds.insert(kind);
    }
    assert_eq!(kinds.len(), 15, "one payload of every type");
}
