// ABOUTME: Private transport for Helix events consumed by the application
// ABOUTME: Forwards Helix hooks through a channel without defining application events

use helix_core::{Assoc, ChangeSet, Operation, Rope};
use helix_view::DocumentId;
use nucleotide_events::document::{ChangeType, DocumentLineChange};
use nucleotide_logging::{debug, info, instrument, trace, warn};
use std::sync::OnceLock;
use tokio::sync::mpsc;

/// Internal events forwarded from Helix hooks to the application.
#[derive(Debug, Clone)]
pub enum HelixEvent {
    DocumentChanged {
        doc_id: DocumentId,
        change_summary: ChangeType,
        line_change: DocumentLineChange,
    },
    DiagnosticsChanged {
        doc_id: DocumentId,
    },
    DocumentOpened {
        doc_id: DocumentId,
    },
    DocumentClosed {
        doc_id: DocumentId,
        was_modified: bool,
    },
    LanguageServerInitialized {
        server_id: helix_lsp::LanguageServerId,
    },
    LanguageServerExited {
        server_id: helix_lsp::LanguageServerId,
    },
}

/// Global event bridge sender - initialized once when application starts
static EVENT_BRIDGE_SENDER: OnceLock<mpsc::UnboundedSender<HelixEvent>> = OnceLock::new();

/// Initialize the event bridge system with a sender
#[instrument(skip(sender))]
pub fn initialize_bridge(sender: mpsc::UnboundedSender<HelixEvent>) {
    if EVENT_BRIDGE_SENDER.set(sender).is_err() {
        warn!("Event bridge was already initialized");
    } else {
        info!("Event bridge initialized successfully");
    }
}

/// Send a Helix event from an event hook.
pub fn send_helix_event(event: HelixEvent) {
    if let Some(sender) = EVENT_BRIDGE_SENDER.get() {
        debug!(event.type = ?std::mem::discriminant(&event), "Sending Helix event");
        if let HelixEvent::DiagnosticsChanged { doc_id } = &event {
            trace!(doc_id = ?doc_id, "DIAG: Bridging DiagnosticsChanged to GPUI");
        }
        if let Err(e) = sender.send(event) {
            warn!(
                error = %e,
                "Failed to send Helix event"
            );
        }
    } else {
        warn!(
            event = ?event,
            "Event bridge not initialized, dropping event"
        );
    }
}

/// Analyze a ChangeSet to determine the type of change that occurred
fn analyze_change_type(changes: &ChangeSet) -> ChangeType {
    let operations = changes.changes();

    if operations.is_empty() {
        return ChangeType::Bulk; // No operations, but a change occurred
    }

    let mut has_insert = false;
    let mut has_delete = false;
    let mut operation_count = 0;

    for operation in operations {
        match operation {
            Operation::Insert(_) => {
                has_insert = true;
                operation_count += 1;
            }
            Operation::Delete(_) => {
                has_delete = true;
                operation_count += 1;
            }
            // `Retain` is positional padding, not a content edit, so it must not be counted.
            // `ChangeSet` appends a trailing `retain(len - last)` so the change set spans the
            // whole document, which means typing one character with the cursor mid-document
            // produces `[Retain(n), Insert(c), Retain(m)]`. Counting that padding pushed a
            // single keystroke to three operations, past the `> 2` bulk threshold, so every
            // keystroke was classified `Bulk` and `maybe_trigger_auto_completion` silently
            // skipped the trigger: the completion popup never appeared at all.
            Operation::Retain(_) => {}
        }
    }

    match (has_insert, has_delete, operation_count > 2) {
        (true, true, _) => ChangeType::Replace, // Both insert and delete = replace
        (true, false, false) => ChangeType::Insert, // Only insert
        (false, true, false) => ChangeType::Delete, // Only delete
        _ => ChangeType::Bulk,                  // Complex multi-operation change
    }
}

fn affected_line_range(text: &Rope, start: usize, end: usize) -> std::ops::Range<usize> {
    let text_len = text.len_chars();
    let line_count = text.len_lines().max(1);
    let start_line = text.char_to_line(start.min(text_len));
    let end_line = text.char_to_line(end.min(text_len));
    start_line..end_line.saturating_add(1).min(line_count)
}

fn document_line_change(
    old_text: &Rope,
    new_text: &Rope,
    changes: &ChangeSet,
) -> DocumentLineChange {
    let mut changed = changes.changes_iter();
    let Some((first_start, first_end, _)) = changed.next() else {
        return DocumentLineChange {
            old_lines: 0..old_text.len_lines().max(1),
            new_lines: 0..new_text.len_lines().max(1),
        };
    };

    let mut old_start = first_start;
    let mut old_end = first_end;
    for (start, end, _) in changed {
        old_start = old_start.min(start);
        old_end = old_end.max(end);
    }

    let new_start = changes.map_pos(old_start, Assoc::Before);
    let new_end = changes.map_pos(old_end, Assoc::After);
    DocumentLineChange {
        old_lines: affected_line_range(old_text, old_start, old_end),
        new_lines: affected_line_range(new_text, new_start, new_end),
    }
}

/// Register Helix event hooks that bridge to GPUI events
#[instrument]
pub fn register_event_hooks() {
    use helix_event::register_hook;
    use helix_view::doc_mut;
    use helix_view::events::{
        DiagnosticsDidChange, DocumentDidChange, DocumentDidClose, DocumentDidOpen,
        DocumentFocusLost, LanguageServerExited, LanguageServerInitialized, SelectionDidChange,
    };

    info!("Registering Helix event hooks for event bridge");

    // Document change events
    register_hook!(move |event: &mut DocumentDidChange<'_>| {
        if let Some(snippet) = &mut event.doc.active_snippet {
            let invalid = snippet.map(event.changes);
            if invalid {
                event.doc.active_snippet = None;
            }
        }

        let doc_id = event.doc.id();
        let change_summary = analyze_change_type(event.changes);
        let line_change = document_line_change(event.old_text, event.doc.text(), event.changes);
        debug!(
            doc_id = ?doc_id,
            change_type = ?change_summary,
            "Document changed event"
        );
        send_helix_event(HelixEvent::DocumentChanged {
            doc_id,
            change_summary,
            line_change,
        });
        Ok(())
    });

    // Selection change events
    register_hook!(move |event: &mut SelectionDidChange<'_>| {
        if let Some(snippet) = &event.doc.active_snippet
            && !snippet.is_valid(event.doc.selection(event.view))
        {
            event.doc.active_snippet = None;
        }

        Ok(())
    });

    register_hook!(move |event: &mut DocumentFocusLost<'_>| {
        let editor = &mut event.editor;
        doc_mut!(editor, &event.doc).active_snippet = None;
        Ok(())
    });

    // Diagnostics change events
    register_hook!(move |event: &mut DiagnosticsDidChange<'_>| {
        let doc_id = event.doc;
        debug!(
            doc_id = ?doc_id,
            "DIAG: Helix DiagnosticsDidChange observed"
        );
        send_helix_event(HelixEvent::DiagnosticsChanged { doc_id });
        Ok(())
    });

    // Document open events
    register_hook!(move |event: &mut DocumentDidOpen<'_>| {
        let doc_id = event.doc;
        info!(
            doc_id = ?doc_id,
            "Document opened event"
        );
        send_helix_event(HelixEvent::DocumentOpened { doc_id });
        Ok(())
    });

    // Document close events
    register_hook!(move |event: &mut DocumentDidClose<'_>| {
        let doc_id = event.doc.id();
        let was_modified = event.doc.is_modified();
        info!(
            doc_id = ?doc_id,
            was_modified = was_modified,
            "Document closed event"
        );
        send_helix_event(HelixEvent::DocumentClosed {
            doc_id,
            was_modified,
        });
        Ok(())
    });

    // Language server initialized events
    register_hook!(move |event: &mut LanguageServerInitialized<'_>| {
        let server_id = event.server_id;
        info!(
            server_id = ?server_id,
            "Language server initialized event"
        );
        send_helix_event(HelixEvent::LanguageServerInitialized { server_id });
        Ok(())
    });

    // Language server exited events
    register_hook!(move |event: &mut LanguageServerExited<'_>| {
        let server_id = event.server_id;
        info!(
            server_id = ?server_id,
            "Language server exited event"
        );
        send_helix_event(HelixEvent::LanguageServerExited { server_id });
        Ok(())
    });

    info!("Successfully registered all Helix event hooks for event bridge");
}

/// Receiver type for Helix events.
pub type HelixEventReceiver = mpsc::UnboundedReceiver<HelixEvent>;

/// Create a channel pair for Helix events.
pub fn create_bridge_channel() -> (mpsc::UnboundedSender<HelixEvent>, HelixEventReceiver) {
    mpsc::unbounded_channel()
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_core::{Tendril, Transaction};

    /// Build a `ChangeSet` the way production does: a `Transaction` constructed over a real
    /// `Rope` and then applied to it.
    ///
    /// Going through `Transaction` is the whole point of this helper. `ChangeSet::from_changes`
    /// appends the trailing `Retain` padding that spans the rest of the document, so a
    /// hand-written operation list would hide the bug these tests guard against.
    fn change_set(
        text: &str,
        changes: impl Iterator<Item = (usize, usize, Option<Tendril>)>,
    ) -> ChangeSet {
        let rope = Rope::from(text);
        let transaction = Transaction::change(&rope, changes);
        let mut new_text = rope.clone();
        assert!(transaction.apply(&mut new_text), "transaction should apply");
        transaction.changes().clone()
    }

    #[test]
    fn document_line_change_tracks_inserted_lines() {
        let old_text = Rope::from("one\ntwo\nthree\n");
        let transaction =
            Transaction::change(&old_text, [(4, 4, Some("inserted\n".into()))].into_iter());
        let mut new_text = old_text.clone();
        assert!(transaction.apply(&mut new_text));

        assert_eq!(
            document_line_change(&old_text, &new_text, transaction.changes()),
            DocumentLineChange {
                old_lines: 1..2,
                new_lines: 1..3,
            }
        );
    }

    #[test]
    fn document_line_change_tracks_deleted_lines() {
        let old_text = Rope::from("one\ntwo\nthree\n");
        let transaction =
            Transaction::change(&old_text, [(4, 8, None::<helix_core::Tendril>)].into_iter());
        let mut new_text = old_text.clone();
        assert!(transaction.apply(&mut new_text));

        assert_eq!(
            document_line_change(&old_text, &new_text, transaction.changes()),
            DocumentLineChange {
                old_lines: 1..3,
                new_lines: 1..2,
            }
        );
    }

    /// Regression guard for the dead auto-trigger popup.
    ///
    /// The cursor sits mid-document, so `ChangeSet::from_changes` emits
    /// `[Retain(14), Insert("x"), Retain(12)]`: the trailing retain covers the remaining 12
    /// characters so the change set spans all 26. Counting that padding made the operation count
    /// 3, past the `> 2` bulk threshold, so every keystroke was classified `Bulk` and
    /// `maybe_trigger_auto_completion` dropped the trigger.
    ///
    /// Ignoring both retains leaves `has_insert = true`, `has_delete = false`,
    /// `operation_count = 1`, so the `(true, false, false)` arm applies: `Insert`.
    #[test]
    fn analyze_change_type_counts_mid_document_keystroke_as_insert() {
        const TEXT: &str = "alpha\nbravo\ncharlie\ndelta\n";
        assert_eq!(Rope::from(TEXT).len_chars(), 26, "document length in characters");

        let changes = change_set(TEXT, [(14, 14, Some(Tendril::from("x")))].into_iter());

        // Pin the reproduction: the leading *and* trailing retains must be present and
        // non-empty. On a document ending at the cursor there is no trailing padding, and the
        // test would pass for the wrong reason.
        let expected: &[Operation] = &[
            Operation::Retain(14),
            Operation::Insert(Tendril::from("x")),
            Operation::Retain(12),
        ];
        assert_eq!(changes.changes(), expected);

        assert!(matches!(analyze_change_type(&changes), ChangeType::Insert));
    }

    /// Typing at the very end appends nothing after the insertion (`retain(0)` is a no-op), so
    /// the operation list is `[Retain(12), Insert("x")]` and only the leading padding is
    /// ignored. `has_insert = true`, `has_delete = false`, `operation_count = 1`, so
    /// `(true, false, false)` gives `Insert`.
    ///
    /// This case was already classified correctly before the fix; it is here to pin the
    /// boundary against the padding behaviour that broke the mid-document case.
    #[test]
    fn analyze_change_type_counts_keystroke_at_end_of_document_as_insert() {
        const TEXT: &str = "alpha\nbravo\n";
        assert_eq!(Rope::from(TEXT).len_chars(), 12, "document length in characters");

        let changes = change_set(TEXT, [(12, 12, Some(Tendril::from("x")))].into_iter());

        let operations = changes.changes();
        assert_eq!(operations.len(), 2, "no padding after an insert at the end");
        assert!(matches!(operations[0], Operation::Retain(12)));
        assert!(matches!(operations[1], Operation::Insert(_)));

        assert!(matches!(analyze_change_type(&changes), ChangeType::Insert));
    }

    /// A deletion expands to `[Retain(2), Delete(3), Retain(15)]`. Ignoring the padding leaves
    /// `has_insert = false`, `has_delete = true`, `operation_count = 1`, so the
    /// `(false, true, false)` arm applies: `Delete`.
    #[test]
    fn analyze_change_type_counts_single_deletion_as_delete() {
        const TEXT: &str = "alpha\nbravo\ncharlie\n";
        assert_eq!(Rope::from(TEXT).len_chars(), 20, "document length in characters");

        let changes = change_set(TEXT, [(2, 5, None::<Tendril>)].into_iter());

        let expected: &[Operation] = &[
            Operation::Retain(2),
            Operation::Delete(3),
            Operation::Retain(15),
        ];
        assert_eq!(changes.changes(), expected);

        assert!(matches!(analyze_change_type(&changes), ChangeType::Delete));
    }

    /// Replacing a span emits both an `Insert` and a `Delete` for the same range, plus padding
    /// between and after the two edits. `has_insert` and `has_delete` are both true, so the
    /// leading `(true, true, _)` arm applies regardless of the count: `Replace`.
    #[test]
    fn analyze_change_type_counts_insert_with_delete_as_replace() {
        const TEXT: &str = "alpha\nbravo\ncharlie\ndelta\n";
        assert_eq!(Rope::from(TEXT).len_chars(), 26, "document length in characters");

        let changes = change_set(
            TEXT,
            [(2, 5, Some(Tendril::from("X"))), (14, 18, None::<Tendril>)].into_iter(),
        );

        let expected: &[Operation] = &[
            Operation::Retain(2),
            Operation::Insert(Tendril::from("X")),
            Operation::Delete(3),
            Operation::Retain(9),
            Operation::Delete(4),
            Operation::Retain(8),
        ];
        assert_eq!(changes.changes(), expected);

        assert!(matches!(analyze_change_type(&changes), ChangeType::Replace));
    }

    /// Three cursors editing at once, as a multi-cursor insert produces, give three `Insert`
    /// operations separated by `Retain` padding: `[Retain(2), Insert("a"), Retain(6),
    /// Insert("b"), Retain(6), Insert("c"), Retain(12)]`. This is a genuine multi-operation
    /// edit, so counting only the content edits still yields `operation_count = 3`, which is
    /// `> 2` and falls through to `Bulk`. The old counter reached the same verdict by accident,
    /// having counted all seven operations; this pins the genuine three-edit case on purpose.
    #[test]
    fn analyze_change_type_counts_multiple_insertions_as_bulk() {
        const TEXT: &str = "alpha\nbravo\ncharlie\ndelta\n";
        assert_eq!(Rope::from(TEXT).len_chars(), 26, "document length in characters");

        let changes = change_set(
            TEXT,
            [
                (2, 2, Some(Tendril::from("a"))),
                (8, 8, Some(Tendril::from("b"))),
                (14, 14, Some(Tendril::from("c"))),
            ]
            .into_iter(),
        );

        let expected: &[Operation] = &[
            Operation::Retain(2),
            Operation::Insert(Tendril::from("a")),
            Operation::Retain(6),
            Operation::Insert(Tendril::from("b")),
            Operation::Retain(6),
            Operation::Insert(Tendril::from("c")),
            Operation::Retain(12),
        ];
        assert_eq!(changes.changes(), expected);

        assert!(matches!(analyze_change_type(&changes), ChangeType::Bulk));
    }

    /// An empty operation list reaches the early return before any counting happens. A change
    /// event still fired, so there is nothing to attribute it to: `Bulk`.
    #[test]
    fn analyze_change_type_counts_empty_change_set_as_bulk() {
        let rope = Rope::from("alpha\nbravo\n");
        let transaction = Transaction::new(&rope);
        assert!(
            transaction.changes().changes().is_empty(),
            "a fresh transaction has no operations"
        );

        assert!(matches!(analyze_change_type(transaction.changes()), ChangeType::Bulk));
    }
}
