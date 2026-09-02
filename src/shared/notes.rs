//! Post-assembly note pruning shared by the DOCX and DOC frontends.
//!
//! Both frontends collect every note a document defines up front, then rely
//! on the inline walk to say which ones text actually points at (the
//! renderer appends anything left over as an unreferenced note - see
//! `render/markdown/mod.rs`). That default is wrong for a note whose *only*
//! reference sat inside author-hidden content: Word never shows such a
//! footnote/endnote mark, so its body must not survive either, the same way
//! the mark's hidden text does not.

use crate::model::Note;
use std::collections::HashSet;

/// Drop every note whose only reference(s) were inside hidden content the
/// hidden-content policy already discarded while walking runs.
///
/// `dropped` holds the ids of footnote/endnote references seen inside hidden
/// runs; `visible` holds the ids of references seen outside one, anywhere in
/// the document (including inside other notes). A note is pruned only when
/// its id is in `dropped` and never in `visible`: a note referenced both
/// ways, or not referenced at all, is left untouched - the latter matches
/// the existing behaviour of rendering unreferenced notes at the end.
pub fn prune_hidden_notes(
    notes: &mut Vec<Note>,
    dropped: &HashSet<String>,
    visible: &HashSet<String>,
) {
    notes.retain(|note| visible.contains(&note.id) || !dropped.contains(&note.id));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Block, NoteKind};

    fn note(id: &str) -> Note {
        Note {
            id: id.to_string(),
            kind: NoteKind::Footnote,
            blocks: vec![Block::Paragraph(Vec::new())],
        }
    }

    fn set(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_note_only_referenced_from_hidden_content_is_dropped() {
        let mut notes = vec![note("fn1"), note("fn2")];
        prune_hidden_notes(&mut notes, &set(&["fn1"]), &HashSet::new());
        let ids: Vec<&str> = notes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, ["fn2"]);
    }

    #[test]
    fn a_note_with_a_surviving_visible_reference_is_kept() {
        // Referenced once hidden and once visibly: the visible one still
        // needs the body.
        let mut notes = vec![note("fn1")];
        prune_hidden_notes(&mut notes, &set(&["fn1"]), &set(&["fn1"]));
        assert_eq!(notes.len(), 1);
    }

    #[test]
    fn a_note_never_referenced_at_all_is_untouched() {
        // Not this pass's concern: an unreferenced note already renders at
        // the document's end regardless of hidden content.
        let mut notes = vec![note("fn1")];
        prune_hidden_notes(&mut notes, &HashSet::new(), &HashSet::new());
        assert_eq!(notes.len(), 1);
    }
}
