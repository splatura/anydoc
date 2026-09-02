//! OOXML WordprocessingML (.docx / .docm).
//!
//! Resolution pipeline: package parts -> style/numbering models ->
//! spec-order property resolution -> document model. Author-hidden text
//! (`w:vanish`/`w:webHidden`, resolved through the same style cascade as
//! bold/italic/strike) and tracked deletions (`w:del`/`w:moveFrom`) are
//! omitted from the model entirely, with no option to retain them. Hidden
//! content never resolves to a leftover reference either: a hyperlink
//! hidden down to a rendered-empty label (whether from hidden runs, a
//! tracked deletion, or both, at any nesting depth) drops out entirely
//! (`content::InlineWalker`), and a footnote/endnote whose only reference
//! sat in a hidden run or tracked deletion is pruned from [`Document::notes`]
//! below (`shared::notes::prune_hidden_notes`) so it cannot resurface as an
//! unreferenced note at the document's end.

mod content;
mod numbering;
mod styles;

use crate::error::ConvertError;
use crate::model::{Document, Note, NoteKind};
use crate::package::Package;
use crate::package::relationships::{Relationships, read_rels, rel_type, rels_part_for};
use crate::package::xml::ns;
use crate::shared::assets::AssetSink;
use content::Ctx;
use numbering::Counters;
use std::cell::RefCell;
use std::collections::HashSet;

pub fn parse(bytes: &[u8]) -> Result<Document, ConvertError> {
    let pkg = match Package::open(bytes) {
        Ok(p) => p,
        Err(e) => return Err(crate::package::archive::probe_ole(bytes).unwrap_or(e)),
    };
    let pkg = RefCell::new(pkg);

    // OPC part discovery: the main part comes from the package-level
    // officeDocument relationship; its own parts (styles, numbering, notes)
    // from the main part's typed relationships. Conventional paths are the
    // fallback for packages with missing or unusable rels.
    let root_rels = read_rels(&mut pkg.borrow_mut(), "_rels/.rels")?;
    let main_part = root_rels
        .first_of_type(rel_type::OFFICE_DOCUMENT)
        .and_then(|rel| crate::package::path::resolve("", &rel.target).ok())
        .map(|t| t.path)
        .unwrap_or_else(|| "word/document.xml".to_string());
    let doc_rels = read_rels(&mut pkg.borrow_mut(), &rels_part_for(&main_part))?;

    let styles_part = typed_part_path(&doc_rels, &main_part, rel_type::STYLES, "styles.xml");
    let styles_tree = pkg.borrow_mut().optional_xml_part(&styles_part)?;
    let styles =
        styles::Styles::parse_opt(styles_tree.as_ref().and_then(|t| t.find(ns::W, "styles")));

    let numbering_part =
        typed_part_path(&doc_rels, &main_part, rel_type::NUMBERING, "numbering.xml");
    let numbering_tree = pkg.borrow_mut().optional_xml_part(&numbering_part)?;
    let numbering = match numbering_tree.as_ref().and_then(|t| t.find(ns::W, "numbering")) {
        Some(root) => numbering::parse(root, &|style_id| styles.direct_num_id(style_id))?,
        None => Default::default(),
    };

    let doc_tree = pkg.borrow_mut().required_xml_part(&main_part)?;
    let body = doc_tree
        .find(ns::W, "document")
        .and_then(|d| d.find(ns::W, "body"))
        .ok_or_else(|| ConvertError::malformed_part(main_part.clone(), "no document body"))?;

    let counters = RefCell::new(Counters::default());
    let assets = RefCell::new(AssetSink::new());
    let dropped_notes = RefCell::new(HashSet::new());
    let visible_notes = RefCell::new(HashSet::new());

    let footnotes_part =
        typed_part_path(&doc_rels, &main_part, rel_type::FOOTNOTES, "footnotes.xml");
    let endnotes_part = typed_part_path(&doc_rels, &main_part, rel_type::ENDNOTES, "endnotes.xml");

    let ctx = Ctx {
        pkg: &pkg,
        rels: doc_rels,
        base_part: main_part,
        styles: &styles,
        numbering: &numbering,
        counters: &counters,
        assets: &assets,
        dropped_notes: &dropped_notes,
        visible_notes: &visible_notes,
    };
    let blocks = content::parse_blocks(body, &ctx)?;

    let mut notes = Vec::new();
    for (part, root_name, elem_name, prefix, kind) in [
        (footnotes_part, "footnotes", "footnote", "fn", NoteKind::Footnote),
        (endnotes_part, "endnotes", "endnote", "en", NoteKind::Endnote),
    ] {
        let Some(tree) = pkg.borrow_mut().optional_xml_part(&part)? else {
            continue;
        };
        let Some(root) = tree.find(ns::W, root_name) else {
            continue;
        };
        let note_rels = read_rels(&mut pkg.borrow_mut(), &rels_part_for(&part))?;
        let note_ctx = ctx.for_part(note_rels, part);
        for note in root.find_all(ns::W, elem_name) {
            if matches!(
                note.attr(ns::W, "type"),
                Some("separator") | Some("continuationSeparator") | Some("continuationNotice")
            ) {
                continue;
            }
            let Some(id) = note.attr(ns::W, "id") else { continue };
            notes.push(Note {
                id: format!("{prefix}{id}"),
                kind,
                blocks: content::parse_blocks(note, &note_ctx)?,
            });
        }
    }

    crate::shared::notes::prune_hidden_notes(
        &mut notes,
        &dropped_notes.borrow(),
        &visible_notes.borrow(),
    );

    let assets = std::mem::take(&mut assets.borrow_mut().assets);
    Ok(Document { blocks, notes, assets })
}

/// Path of a typed related part, resolved against the main part; falls back
/// to the conventional sibling name when the relationship is absent.
fn typed_part_path(rels: &Relationships, base: &str, rel_type: &str, fallback: &str) -> String {
    let reference = rels.first_of_type(rel_type).map(|rel| rel.target.as_str()).unwrap_or(fallback);
    match crate::package::path::resolve(base, reference) {
        Ok(target) => target.path,
        Err(e) => {
            log::warn!("skipping unresolvable related-part target {reference:?}: {e}");
            crate::package::path::resolve(base, fallback)
                .map(|t| t.path)
                .unwrap_or_else(|_| fallback.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Block, ImageSource, Inline};
    use std::io::{Cursor, Write};

    fn docx_parts(parts: &[(&str, &str)]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default();
        for (name, body) in parts {
            w.start_file(*name, opts).unwrap();
            w.write_all(body.as_bytes()).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    fn docx(document: &str, rels: &str) -> Vec<u8> {
        docx_parts(&[("word/document.xml", document), ("word/_rels/document.xml.rels", rels)])
    }

    const W: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;

    fn find_image(blocks: &[Block]) -> Option<&Inline> {
        blocks.iter().find_map(|b| match b {
            Block::Paragraph(inlines) => inlines.iter().find(|i| matches!(i, Inline::Image { .. })),
            _ => None,
        })
    }

    #[test]
    fn linked_image_relationship_becomes_an_external_source() {
        // M9: `r:link` with an external-mode relationship must carry the URL
        // instead of failing the internal-part loader.
        let document = r#"<w:document
            xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"
            xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"
            xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
            <w:body><w:p><w:r><w:drawing>
                <a:blip r:link="rId9"/>
            </w:drawing></w:r></w:p></w:body></w:document>"#;
        let rels = r#"<Relationships
            xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
            <Relationship Id="rId9"
                Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image"
                Target="https://e.com/pic.png" TargetMode="External"/>
            </Relationships>"#;
        let doc = parse(&docx(document, rels)).unwrap();
        let image = find_image(&doc.blocks).expect("image inline");
        let Inline::Image { source, .. } = image else { unreachable!() };
        assert_eq!(*source, ImageSource::External("https://e.com/pic.png".into()));
        assert!(doc.assets.is_empty(), "external images are not retained as assets");
    }

    fn numbering_with_start(start: &str) -> String {
        format!(
            r#"<w:numbering {W}>
            <w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0">
                <w:numFmt w:val="decimal"/><w:start w:val="{start}"/>
            </w:lvl></w:abstractNum>
            <w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>
            </w:numbering>"#
        )
    }

    fn numbered_paragraphs() -> String {
        let para = r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr>
            <w:r><w:t>item</w:t></w:r></w:p>"#;
        format!(r#"<w:document {W}><w:body>{para}{para}</w:body></w:document>"#)
    }

    #[test]
    fn huge_numbering_start_values_cannot_overflow() {
        // H2: w:start is ST_DecimalNumber (xsd:int); out-of-range values are
        // clamped so document-order increments can never overflow.
        for start in ["18446744073709551615", "-5", "2147483647"] {
            let bytes = docx_parts(&[
                ("word/document.xml", &numbered_paragraphs()),
                ("word/numbering.xml", &numbering_with_start(start)),
            ]);
            let doc = parse(&bytes).expect(start);
            assert!(!doc.blocks.is_empty());
        }
    }

    #[test]
    fn partial_num_pr_inherits_property_by_property() {
        // M1: a direct numPr carrying only ilvl merges with the style's
        // numId instead of suppressing numbering.
        let document = format!(
            r#"<w:document {W}><w:body>
            <w:p><w:pPr><w:pStyle w:val="Listy"/>
                <w:numPr><w:ilvl w:val="1"/></w:numPr></w:pPr>
                <w:r><w:t>second level</w:t></w:r></w:p>
            </w:body></w:document>"#
        );
        let styles = format!(
            r#"<w:styles {W}>
            <w:style w:type="paragraph" w:styleId="Listy">
                <w:pPr><w:numPr><w:numId w:val="1"/></w:numPr></w:pPr>
            </w:style></w:styles>"#
        );
        let numbering = format!(
            r#"<w:numbering {W}>
            <w:abstractNum w:abstractNumId="0">
                <w:lvl w:ilvl="0"><w:numFmt w:val="decimal"/><w:start w:val="1"/></w:lvl>
                <w:lvl w:ilvl="1"><w:numFmt w:val="lowerLetter"/><w:start w:val="1"/></w:lvl>
            </w:abstractNum>
            <w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>
            </w:numbering>"#
        );
        let bytes = docx_parts(&[
            ("word/document.xml", &document),
            ("word/styles.xml", &styles),
            ("word/numbering.xml", &numbering),
        ]);
        let doc = parse(&bytes).unwrap();
        let Some(Block::List(list)) = doc.blocks.first() else {
            panic!("expected a list, got {:?}", doc.blocks.first());
        };
        assert_eq!(
            list.marker,
            crate::model::MarkerKind::LowerAlpha,
            "level 1 of the style's numbering"
        );
    }

    #[test]
    fn unmarked_run_edge_whitespace_is_kept() {
        // Converters that never write xml:space carry inter-word spacing on
        // run edges; dropping it glues the words together.
        let document = format!(
            r#"<w:document {W}><w:body><w:p>
            <w:r><w:t>This</w:t></w:r>
            <w:r><w:t> by-law</w:t></w:r>
            <w:r><w:t> grants</w:t></w:r>
            </w:p></w:body></w:document>"#
        );
        let doc = parse(&docx_parts(&[("word/document.xml", &document)])).unwrap();
        let Some(Block::Paragraph(inlines)) = doc.blocks.first() else { panic!() };
        assert_eq!(crate::model::inlines_to_plain_text(inlines), "This by-law grants");
    }

    #[test]
    fn page_break_between_runs_keeps_the_word_boundary() {
        // A w:br is unrepresentable in Markdown whatever its type, but the
        // runs it separates must not merge into a word the document never
        // had. A break ending a paragraph still leaves no stray marker.
        let cases = [
            (
                r#"<w:p><w:r><w:t>Alfa</w:t><w:br w:type="page"/><w:t>Beta</w:t></w:r></w:p>"#,
                "Alfa\\\nBeta\n",
            ),
            (
                r#"<w:p><w:r><w:t>Alfa</w:t><w:br w:type="page"/></w:r></w:p>
                <w:p><w:r><w:t>Beta</w:t></w:r></w:p>"#,
                "Alfa\n\nBeta\n",
            ),
        ];
        for (body, expected) in cases {
            let document = format!(r#"<w:document {W}><w:body>{body}</w:body></w:document>"#);
            let bytes = docx_parts(&[("word/document.xml", &document)]);
            let markdown = crate::to_markdown_bytes(&bytes, crate::Format::Docx).unwrap();
            assert_eq!(markdown, expected, "body: {body}");
        }
    }

    #[test]
    fn numbered_heading_keeps_its_number() {
        // H1: a heading style with numbering shows its label and advances
        // the sequence.
        let document = format!(
            r#"<w:document {W}><w:body>
            <w:p><w:pPr><w:pStyle w:val="H1"/></w:pPr><w:r><w:t>Intro</w:t></w:r></w:p>
            <w:p><w:pPr><w:pStyle w:val="H1"/></w:pPr><w:r><w:t>Details</w:t></w:r></w:p>
            </w:body></w:document>"#
        );
        let styles = format!(
            r#"<w:styles {W}>
            <w:style w:type="paragraph" w:styleId="H1"><w:name w:val="heading 1"/>
                <w:pPr><w:numPr><w:numId w:val="1"/></w:numPr></w:pPr>
            </w:style></w:styles>"#
        );
        let numbering = format!(
            r#"<w:numbering {W}>
            <w:abstractNum w:abstractNumId="0">
                <w:lvl w:ilvl="0"><w:numFmt w:val="decimal"/><w:start w:val="1"/>
                    <w:lvlText w:val="%1."/><w:pStyle w:val="H1"/></w:lvl>
            </w:abstractNum>
            <w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>
            </w:numbering>"#
        );
        let bytes = docx_parts(&[
            ("word/document.xml", &document),
            ("word/styles.xml", &styles),
            ("word/numbering.xml", &numbering),
        ]);
        let doc = parse(&bytes).unwrap();
        let headings: Vec<String> = doc
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Heading { content, .. } => {
                    Some(crate::model::inlines_to_plain_text(content))
                }
                _ => None,
            })
            .collect();
        assert_eq!(headings, vec!["1. Intro", "2. Details"]);
    }

    #[test]
    fn direct_vanish_run_is_dropped_while_neighbours_survive() {
        // Author-hidden text is omitted from the model entirely, the same
        // way DOCX already drops w:del tracked deletions.
        let document = format!(
            r#"<w:document {W}><w:body><w:p>
            <w:r><w:t>before </w:t></w:r>
            <w:r><w:rPr><w:vanish/></w:rPr><w:t>secret</w:t></w:r>
            <w:r><w:t> after</w:t></w:r>
            </w:p></w:body></w:document>"#
        );
        let doc = parse(&docx_parts(&[("word/document.xml", &document)])).unwrap();
        let Some(Block::Paragraph(inlines)) = doc.blocks.first() else {
            panic!("expected a paragraph: {:?}", doc.blocks)
        };
        let text = crate::model::inlines_to_plain_text(inlines);
        assert!(!text.contains("secret"), "{text:?}");
        assert!(text.contains("before"), "{text:?}");
        assert!(text.contains("after"), "{text:?}");
    }

    #[test]
    fn direct_web_hidden_run_is_dropped() {
        let document = format!(
            r#"<w:document {W}><w:body><w:p>
            <w:r><w:t>before </w:t></w:r>
            <w:r><w:rPr><w:webHidden/></w:rPr><w:t>secret</w:t></w:r>
            </w:p></w:body></w:document>"#
        );
        let doc = parse(&docx_parts(&[("word/document.xml", &document)])).unwrap();
        let Some(Block::Paragraph(inlines)) = doc.blocks.first() else {
            panic!("expected a paragraph: {:?}", doc.blocks)
        };
        let text = crate::model::inlines_to_plain_text(inlines);
        assert!(!text.contains("secret"), "{text:?}");
        assert!(text.contains("before"), "{text:?}");
    }

    #[test]
    fn character_style_vanish_hides_its_runs() {
        // Hidden must resolve through the style cascade like bold does: a
        // character style carrying w:vanish hides the runs that use it.
        let document = format!(
            r#"<w:document {W}><w:body><w:p>
            <w:r><w:t>before </w:t></w:r>
            <w:r><w:rPr><w:rStyle w:val="Hidden"/></w:rPr><w:t>secret</w:t></w:r>
            </w:p></w:body></w:document>"#
        );
        let styles = format!(
            r#"<w:styles {W}>
            <w:style w:type="character" w:styleId="Hidden"><w:rPr><w:vanish/></w:rPr></w:style>
            </w:styles>"#
        );
        let bytes = docx_parts(&[("word/document.xml", &document), ("word/styles.xml", &styles)]);
        let doc = parse(&bytes).unwrap();
        let Some(Block::Paragraph(inlines)) = doc.blocks.first() else {
            panic!("expected a paragraph: {:?}", doc.blocks)
        };
        let text = crate::model::inlines_to_plain_text(inlines);
        assert!(!text.contains("secret"), "{text:?}");
        assert!(text.contains("before"), "{text:?}");
    }

    #[test]
    fn direct_vanish_off_under_a_hidden_style_is_kept() {
        // A run's direct rPr with <w:vanish w:val="0"/> un-hides even when
        // its character style is hidden.
        let document = format!(
            r#"<w:document {W}><w:body><w:p>
            <w:r><w:rPr><w:rStyle w:val="Hidden"/><w:vanish w:val="0"/></w:rPr>
                <w:t>visible</w:t></w:r>
            </w:p></w:body></w:document>"#
        );
        let styles = format!(
            r#"<w:styles {W}>
            <w:style w:type="character" w:styleId="Hidden"><w:rPr><w:vanish/></w:rPr></w:style>
            </w:styles>"#
        );
        let bytes = docx_parts(&[("word/document.xml", &document), ("word/styles.xml", &styles)]);
        let doc = parse(&bytes).unwrap();
        let Some(Block::Paragraph(inlines)) = doc.blocks.first() else {
            panic!("expected a paragraph: {:?}", doc.blocks)
        };
        assert_eq!(crate::model::inlines_to_plain_text(inlines).trim(), "visible");
    }

    #[test]
    fn a_paragraph_of_only_hidden_runs_produces_no_empty_paragraph() {
        let document = format!(
            r#"<w:document {W}><w:body>
            <w:p><w:r><w:rPr><w:vanish/></w:rPr><w:t>gone</w:t></w:r></w:p>
            <w:p><w:r><w:t>kept</w:t></w:r></w:p>
            </w:body></w:document>"#
        );
        let doc = parse(&docx_parts(&[("word/document.xml", &document)])).unwrap();
        assert_eq!(doc.blocks.len(), 1, "{:?}", doc.blocks);
        let Some(Block::Paragraph(inlines)) = doc.blocks.first() else {
            panic!("expected a paragraph: {:?}", doc.blocks)
        };
        assert_eq!(crate::model::inlines_to_plain_text(inlines).trim(), "kept");
    }

    #[test]
    fn paragraph_style_vanish_hides_runs_with_no_direct_rpr() {
        // Hidden must also cascade from a *paragraph* style, not only a
        // character style: a run with no rPr of its own still hides when
        // its paragraph uses a w:pStyle carrying w:vanish.
        let document = format!(
            r#"<w:document {W}><w:body>
            <w:p><w:pPr><w:pStyle w:val="HiddenPara"/></w:pPr>
                <w:r><w:t>secret</w:t></w:r></w:p>
            <w:p><w:r><w:t>kept</w:t></w:r></w:p>
            </w:body></w:document>"#
        );
        let styles = format!(
            r#"<w:styles {W}>
            <w:style w:type="paragraph" w:styleId="HiddenPara">
                <w:rPr><w:vanish/></w:rPr>
            </w:style></w:styles>"#
        );
        let bytes = docx_parts(&[("word/document.xml", &document), ("word/styles.xml", &styles)]);
        let doc = parse(&bytes).unwrap();
        assert_eq!(doc.blocks.len(), 1, "hidden paragraph must not survive: {:?}", doc.blocks);
        let Some(Block::Paragraph(inlines)) = doc.blocks.first() else {
            panic!("expected a paragraph: {:?}", doc.blocks)
        };
        assert_eq!(crate::model::inlines_to_plain_text(inlines).trim(), "kept");
    }

    #[test]
    fn doc_defaults_vanish_hides_runs_with_no_style_or_direct_rpr() {
        // docDefaults' rPr is the base every toggle parity flips over; a
        // vanish there hides a run that specifies no rPr at all.
        let document = format!(
            r#"<w:document {W}><w:body><w:p>
            <w:r><w:t>secret</w:t></w:r>
            </w:p></w:body></w:document>"#
        );
        let styles = format!(
            r#"<w:styles {W}>
            <w:docDefaults><w:rPrDefault><w:rPr><w:vanish/></w:rPr></w:rPrDefault></w:docDefaults>
            </w:styles>"#
        );
        let bytes = docx_parts(&[("word/document.xml", &document), ("word/styles.xml", &styles)]);
        let doc = parse(&bytes).unwrap();
        assert!(doc.blocks.is_empty(), "{:?}", doc.blocks);
    }

    #[test]
    fn based_on_vanish_cancels_between_parent_and_child_style() {
        // w:vanish is a toggle property: two true specifications along the
        // basedOn chain cancel back to visible, the same parity rule as
        // bold/italic/strike.
        let document = format!(
            r#"<w:document {W}><w:body><w:p>
            <w:r><w:rPr><w:rStyle w:val="Child"/></w:rPr><w:t>visible</w:t></w:r>
            </w:p></w:body></w:document>"#
        );
        let styles = format!(
            r#"<w:styles {W}>
            <w:style w:type="character" w:styleId="Hidden"><w:rPr><w:vanish/></w:rPr></w:style>
            <w:style w:type="character" w:styleId="Child">
                <w:basedOn w:val="Hidden"/><w:rPr><w:vanish/></w:rPr>
            </w:style>
            </w:styles>"#
        );
        let bytes = docx_parts(&[("word/document.xml", &document), ("word/styles.xml", &styles)]);
        let doc = parse(&bytes).unwrap();
        let Some(Block::Paragraph(inlines)) = doc.blocks.first() else {
            panic!("expected a paragraph, hidden text cancelled back to visible: {:?}", doc.blocks)
        };
        assert_eq!(crate::model::inlines_to_plain_text(inlines).trim(), "visible");
    }

    #[test]
    fn based_on_vanish_cascades_through_a_three_level_chain() {
        // Regression test for the cascade actually propagating: unlike
        // `based_on_vanish_cancels_between_parent_and_child_style` (where
        // vanish/vanish cancel back to visible even without the feature),
        // this chain has an odd number of `w:vanish` specifications so the
        // run must resolve hidden - the parity assertion only holds if
        // hidden keeps cascading through more than one basedOn hop.
        let document = format!(
            r#"<w:document {W}><w:body><w:p>
            <w:r><w:rPr><w:rStyle w:val="Leaf"/></w:rPr><w:t>secret</w:t></w:r>
            </w:p></w:body></w:document>"#
        );
        let styles = format!(
            r#"<w:styles {W}>
            <w:style w:type="character" w:styleId="Hidden"><w:rPr><w:vanish/></w:rPr></w:style>
            <w:style w:type="character" w:styleId="Middle">
                <w:basedOn w:val="Hidden"/><w:rPr><w:vanish/></w:rPr>
            </w:style>
            <w:style w:type="character" w:styleId="Leaf">
                <w:basedOn w:val="Middle"/><w:rPr><w:vanish/></w:rPr>
            </w:style>
            </w:styles>"#
        );
        let bytes = docx_parts(&[("word/document.xml", &document), ("word/styles.xml", &styles)]);
        let doc = parse(&bytes).unwrap();
        assert!(doc.blocks.is_empty(), "hidden text must not survive: {:?}", doc.blocks);
    }

    #[test]
    fn character_style_web_hidden_hides_its_runs() {
        // w:webHidden is absolute, not an ECMA-376 toggle, but it must
        // still cascade through a character style like vanish does.
        let document = format!(
            r#"<w:document {W}><w:body><w:p>
            <w:r><w:t>before </w:t></w:r>
            <w:r><w:rPr><w:rStyle w:val="WebHidden"/></w:rPr><w:t>secret</w:t></w:r>
            </w:p></w:body></w:document>"#
        );
        let styles = format!(
            r#"<w:styles {W}>
            <w:style w:type="character" w:styleId="WebHidden"><w:rPr><w:webHidden/></w:rPr></w:style>
            </w:styles>"#
        );
        let bytes = docx_parts(&[("word/document.xml", &document), ("word/styles.xml", &styles)]);
        let doc = parse(&bytes).unwrap();
        let Some(Block::Paragraph(inlines)) = doc.blocks.first() else {
            panic!("expected a paragraph: {:?}", doc.blocks)
        };
        let text = crate::model::inlines_to_plain_text(inlines);
        assert!(!text.contains("secret"), "{text:?}");
        assert!(text.contains("before"), "{text:?}");
    }

    #[test]
    fn nearer_web_hidden_specification_wins_over_an_ancestor_style() {
        // w:webHidden is absolute last-wins along basedOn, not XOR parity:
        // a child style's explicit off must un-hide even though its parent
        // specifies webHidden, and direct formatting still overrides both.
        let document = format!(
            r#"<w:document {W}><w:body><w:p>
            <w:r><w:rPr><w:rStyle w:val="Child"/></w:rPr><w:t>visible</w:t></w:r>
            </w:p></w:body></w:document>"#
        );
        let styles = format!(
            r#"<w:styles {W}>
            <w:style w:type="character" w:styleId="Hidden"><w:rPr><w:webHidden/></w:rPr></w:style>
            <w:style w:type="character" w:styleId="Child">
                <w:basedOn w:val="Hidden"/><w:rPr><w:webHidden w:val="0"/></w:rPr>
            </w:style>
            </w:styles>"#
        );
        let bytes = docx_parts(&[("word/document.xml", &document), ("word/styles.xml", &styles)]);
        let doc = parse(&bytes).unwrap();
        let Some(Block::Paragraph(inlines)) = doc.blocks.first() else {
            panic!("expected a paragraph, nearer webHidden=0 must win: {:?}", doc.blocks)
        };
        assert_eq!(crate::model::inlines_to_plain_text(inlines).trim(), "visible");
    }

    #[test]
    fn hidden_hyperlink_label_drops_the_link_while_a_visible_one_survives() {
        // A hyperlink whose entire label is hidden must not resolve to a
        // link at all: with no visible text, the renderer would otherwise
        // show the raw target as the link text (`[target](target)`), which
        // leaks a URL the author never displayed.
        let document = format!(
            r#"<w:document {W}><w:body><w:p>
            <w:hyperlink w:anchor="secret">
                <w:r><w:rPr><w:vanish/></w:rPr><w:t>x</w:t></w:r>
            </w:hyperlink>
            <w:hyperlink w:anchor="visible"><w:r><w:t>click here</w:t></w:r></w:hyperlink>
            </w:p></w:body></w:document>"#
        );
        let doc = parse(&docx_parts(&[("word/document.xml", &document)])).unwrap();
        let Some(Block::Paragraph(inlines)) = doc.blocks.first() else {
            panic!("expected a paragraph: {:?}", doc.blocks)
        };
        let links: Vec<&Inline> =
            inlines.iter().filter(|i| matches!(i, Inline::Link { .. })).collect();
        assert_eq!(links.len(), 1, "{inlines:?}");
        let Inline::Link { target, .. } = links[0] else { unreachable!() };
        assert_eq!(*target, crate::model::LinkTarget::Anchor("visible".into()));
        let text = crate::model::inlines_to_plain_text(inlines);
        assert!(!text.contains('x'), "{text:?}");
        assert!(text.contains("click here"), "{text:?}");
    }

    #[test]
    fn a_hyperlink_with_a_genuinely_empty_label_still_shows_its_target() {
        // Pinning today's behaviour for the case the fix must not touch: an
        // empty label the source itself wrote (no hidden content dropped)
        // still keeps its resolved target, the way Word shows the URL.
        let document = format!(
            r#"<w:document {W}><w:body><w:p>
            <w:hyperlink w:anchor="target"></w:hyperlink>
            </w:p></w:body></w:document>"#
        );
        let doc = parse(&docx_parts(&[("word/document.xml", &document)])).unwrap();
        let Some(Block::Paragraph(inlines)) = doc.blocks.first() else {
            panic!("expected a paragraph: {:?}", doc.blocks)
        };
        assert!(
            matches!(
                inlines.as_slice(),
                [Inline::Link { content, target }]
                if content.is_empty() && *target == crate::model::LinkTarget::Anchor("target".into())
            ),
            "{inlines:?}"
        );
    }

    #[test]
    fn a_hyperlink_with_a_whitespace_only_label_and_no_hidden_content_still_shows_its_target() {
        // A whitespace-only label the source itself wrote (no hidden
        // content dropped) must keep today's behaviour: Word shows the
        // URL as the link text. Only a label emptied *by* hidden content
        // being dropped loses its target.
        let document = format!(
            r#"<w:document {W}><w:body><w:p>
            <w:hyperlink w:anchor="target"><w:r><w:t xml:space="preserve"> </w:t></w:r></w:hyperlink>
            </w:p></w:body></w:document>"#
        );
        let doc = parse(&docx_parts(&[("word/document.xml", &document)])).unwrap();
        let Some(Block::Paragraph(inlines)) = doc.blocks.first() else {
            panic!("expected a paragraph: {:?}", doc.blocks)
        };
        assert!(
            matches!(
                inlines.as_slice(),
                [Inline::Link { target, .. }]
                if *target == crate::model::LinkTarget::Anchor("target".into())
            ),
            "{inlines:?}"
        );
    }

    fn footnotes_docx(document: &str, footnote_body: &str) -> Vec<u8> {
        let footnotes = format!(
            r#"<w:footnotes {W}><w:footnote w:id="1"><w:p><w:r><w:t>{footnote_body}</w:t></w:r></w:p></w:footnote></w:footnotes>"#
        );
        docx_parts(&[("word/document.xml", document), ("word/footnotes.xml", &footnotes)])
    }

    #[test]
    fn a_footnote_referenced_only_from_hidden_content_is_dropped_entirely() {
        // The reference mark itself is hidden, so Word never shows this
        // footnote; its body must not survive as an unreferenced note either
        // (the renderer appends unreferenced notes at the document's end).
        let document = format!(
            r#"<w:document {W}><w:body><w:p>
            <w:r><w:t>before </w:t></w:r>
            <w:r><w:rPr><w:vanish/></w:rPr><w:footnoteReference w:id="1"/></w:r>
            </w:p></w:body></w:document>"#
        );
        let doc = parse(&footnotes_docx(&document, "HIDDEN-FOOTNOTE-BODY")).unwrap();
        assert!(doc.notes.iter().all(|n| n.id != "fn1"), "{:?}", doc.notes);
        let markdown = crate::render::markdown::document_to_markdown(&doc);
        assert!(!markdown.contains("[^"), "{markdown:?}");
        assert!(!markdown.contains("HIDDEN-FOOTNOTE-BODY"), "{markdown:?}");
    }

    #[test]
    fn a_footnote_reference_nested_in_a_text_box_inside_a_hidden_run_is_dropped_entirely() {
        // A note reference can sit arbitrarily deep under a hidden run's
        // w:drawing (wps:txbx/w:txbxContent), not just as a direct child:
        // the shallow field-marker walk used for hidden runs must still
        // find it so the note body doesn't survive as an unreferenced
        // trailing note. Word itself refuses to place footnotes inside
        // text boxes, but a crafted file can still reach this path.
        let document = format!(
            r#"<w:document {W}
            xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape">
            <w:body><w:p>
            <w:r><w:t>before </w:t></w:r>
            <w:r><w:rPr><w:vanish/></w:rPr><w:drawing><wps:txbx><w:txbxContent>
                <w:p><w:r><w:footnoteReference w:id="1"/></w:r></w:p>
            </w:txbxContent></wps:txbx></w:drawing></w:r>
            </w:p></w:body></w:document>"#
        );
        let doc = parse(&footnotes_docx(&document, "HIDDEN-TEXTBOX-BODY")).unwrap();
        assert!(doc.notes.iter().all(|n| n.id != "fn1"), "{:?}", doc.notes);
        let markdown = crate::render::markdown::document_to_markdown(&doc);
        assert!(!markdown.contains("[^"), "{markdown:?}");
        assert!(!markdown.contains("HIDDEN-TEXTBOX-BODY"), "{markdown:?}");
    }

    #[test]
    fn a_footnote_referenced_once_hidden_and_once_visibly_is_kept() {
        let document = format!(
            r#"<w:document {W}><w:body><w:p>
            <w:r><w:rPr><w:vanish/></w:rPr><w:footnoteReference w:id="1"/></w:r>
            <w:r><w:t>seen</w:t></w:r>
            <w:r><w:footnoteReference w:id="1"/></w:r>
            </w:p></w:body></w:document>"#
        );
        let doc = parse(&footnotes_docx(&document, "visible body")).unwrap();
        assert!(doc.notes.iter().any(|n| n.id == "fn1"), "{:?}", doc.notes);
        let markdown = crate::render::markdown::document_to_markdown(&doc);
        assert!(markdown.contains("[^1]: visible body"), "{markdown:?}");
    }

    #[test]
    fn hidden_fld_char_separate_does_not_drop_the_visible_field_result() {
        // A field's structural markers (w:fldChar/w:instrText) must still
        // be walked even when the marker's own run is hidden: a hidden
        // "separate" that never flips in_result would leave every visible
        // result run discarded until "end".
        let document = format!(
            r#"<w:document {W}><w:body><w:p>
            <w:r><w:fldChar w:fldCharType="begin"/></w:r>
            <w:r><w:instrText> HYPERLINK "https://example.com" </w:instrText></w:r>
            <w:r><w:rPr><w:vanish/></w:rPr><w:fldChar w:fldCharType="separate"/></w:r>
            <w:r><w:t>link text</w:t></w:r>
            <w:r><w:fldChar w:fldCharType="end"/></w:r>
            </w:p></w:body></w:document>"#
        );
        let doc = parse(&docx_parts(&[("word/document.xml", &document)])).unwrap();
        let Some(Block::Paragraph(inlines)) = doc.blocks.first() else {
            panic!("expected a paragraph: {:?}", doc.blocks)
        };
        let text = crate::model::inlines_to_plain_text(inlines);
        assert!(text.contains("link text"), "{text:?}");
    }

    fn hyperlink_rels() -> &'static str {
        r#"<Relationships
            xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
            <Relationship Id="rId1"
                Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink"
                Target="https://evil.example/IGNORE-PREVIOUS" TargetMode="External"/>
            </Relationships>"#
    }

    fn assert_no_link_and_no_url_text(document: &str) {
        let bytes = docx_parts(&[
            ("word/document.xml", document),
            ("word/_rels/document.xml.rels", hyperlink_rels()),
        ]);
        let doc = parse(&bytes).unwrap();
        let markdown = crate::render::markdown::document_to_markdown(&doc);
        assert!(!markdown.contains("evil.example"), "{markdown:?}");
        for block in &doc.blocks {
            if let Block::Paragraph(inlines) = block {
                assert!(!inlines.iter().any(|i| matches!(i, Inline::Link { .. })), "{inlines:?}");
            }
        }
    }

    #[test]
    fn a_hidden_label_wrapped_in_a_nested_hyperlink_still_drops_the_outer_link() {
        // The inner hyperlink correctly drops itself, but the outer
        // walker's `dropped_hidden` flag must still learn that hidden
        // content was involved, or `content.is_empty() && label_hidden`
        // is false for the outer link and its URL leaks.
        let document = format!(
            r#"<w:document {W}
            xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><w:body><w:p>
            <w:hyperlink r:id="rId1"><w:hyperlink w:anchor="inner">
                <w:r><w:rPr><w:vanish/></w:rPr><w:t>x</w:t></w:r>
            </w:hyperlink></w:hyperlink>
            </w:p></w:body></w:document>"#
        );
        assert_no_link_and_no_url_text(&document);
    }

    #[test]
    fn a_hidden_label_wrapped_in_a_fld_simple_inside_a_hyperlink_drops_the_outer_link() {
        let document = format!(
            r#"<w:document {W}
            xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><w:body><w:p>
            <w:hyperlink r:id="rId1"><w:fldSimple w:instr=" PAGE ">
                <w:r><w:rPr><w:vanish/></w:rPr><w:t>x</w:t></w:r>
            </w:fldSimple></w:hyperlink>
            </w:p></w:body></w:document>"#
        );
        assert_no_link_and_no_url_text(&document);
    }

    #[test]
    fn a_hidden_label_beside_a_whitespace_only_visible_run_still_drops_the_link() {
        // Word never shows the URL for a link whose rendered label is
        // visually empty, even when a visible run remains alongside the
        // hidden one - as long as that visible run is only whitespace, a
        // tab, or a bare bookmark. The bookmark itself must survive as
        // plain content so its anchor is not lost.
        let cases = [
            r#"<w:r><w:rPr><w:vanish/></w:rPr><w:t>x</w:t></w:r><w:r><w:t xml:space="preserve"> </w:t></w:r>"#,
            r#"<w:r><w:rPr><w:vanish/></w:rPr><w:t>x</w:t></w:r><w:r><w:tab/></w:r>"#,
            r#"<w:r><w:rPr><w:vanish/></w:rPr><w:t>x</w:t></w:r><w:bookmarkStart w:name="bm"/>"#,
            r#"<w:r><w:rPr><w:vanish/></w:rPr><w:t>x</w:t></w:r><w:r><w:br/></w:r>"#,
        ];
        for label in cases {
            let document = format!(
                r#"<w:document {W}
                xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><w:body><w:p>
                <w:hyperlink r:id="rId1">{label}</w:hyperlink>
                </w:p></w:body></w:document>"#
            );
            assert_no_link_and_no_url_text(&document);
        }
    }

    #[test]
    fn a_hyperlink_whose_label_sits_entirely_inside_a_tracked_deletion_drops_the_link() {
        let document = format!(
            r#"<w:document {W}
            xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><w:body><w:p>
            <w:hyperlink r:id="rId1"><w:del w:id="1" w:author="a">
                <w:r><w:t>x</w:t></w:r>
            </w:del></w:hyperlink>
            </w:p></w:body></w:document>"#
        );
        assert_no_link_and_no_url_text(&document);
    }

    #[test]
    fn a_footnote_reference_inside_a_tracked_deletion_is_dropped_entirely() {
        let document = format!(
            r#"<w:document {W}><w:body><w:p>
            <w:r><w:t>t</w:t></w:r>
            <w:del w:id="1" w:author="a"><w:r><w:footnoteReference w:id="1"/></w:r></w:del>
            </w:p></w:body></w:document>"#
        );
        let doc = parse(&footnotes_docx(&document, "BODY-ONE")).unwrap();
        assert!(doc.notes.iter().all(|n| n.id != "fn1"), "{:?}", doc.notes);
        let markdown = crate::render::markdown::document_to_markdown(&doc);
        assert!(!markdown.contains("[^"), "{markdown:?}");
    }

    #[test]
    fn a_footnote_reference_inside_a_move_from_is_dropped_entirely() {
        let document = format!(
            r#"<w:document {W}><w:body><w:p>
            <w:r><w:t>t</w:t></w:r>
            <w:moveFrom w:id="1" w:author="a"><w:r><w:footnoteReference w:id="1"/></w:r></w:moveFrom>
            </w:p></w:body></w:document>"#
        );
        let doc = parse(&footnotes_docx(&document, "BODY-ONE")).unwrap();
        assert!(doc.notes.iter().all(|n| n.id != "fn1"), "{:?}", doc.notes);
        let markdown = crate::render::markdown::document_to_markdown(&doc);
        assert!(!markdown.contains("[^"), "{markdown:?}");
    }
}
