//! (X)HTML element tree -> model blocks. Used by the EPUB frontend.
//!
//! Applies a deliberately small CSS subset - the semantic properties only:
//! `font-weight`, `font-style`, `text-decoration: line-through`, and the
//! hiding properties below - from inline `style` attributes and
//! element/class rules alike (both funnel through [`parse_declarations`]).
//! Tables build the canonical grid (`rowspan`/`colspan`); ordered lists honor
//! `start`, `reversed`, `type`, and per-item `value`.
//!
//! Stylesheet selector coverage is narrow by design (see
//! [`Stylesheet::add`]): a rule's selector list (`a, b`) participates
//! selector-by-selector, and each one must be a `tag`, `.class`, `#id`, one
//! of the two attribute selectors `[hidden]`/`[aria-hidden="true"]` (quotes
//! optional), or a combination of those on one compound selector
//! (`tag.class`, `tag#id`, `#id.class`, ...). Everything else - descendant,
//! child, and sibling combinators (`div p`, `div > p`, `p ~ span`),
//! pseudo-classes/elements (`p:first-child`), the universal selector (`*`),
//! any attribute selector besides the two above (`[data-x]`), and any
//! at-rule (`@media`, `@supports`, `@import`, or any other nested block) -
//! is skipped whole, never partially matched: [`Stylesheet::add`] has no
//! notion of nesting, so an at-rule's inner block is never reached as a
//! selector/declaration pair and its declarations never take effect.
//!
//! Author-hidden content is dropped, with no option to keep it: `display:
//! none`, `visibility: hidden`/`collapse`, `opacity: 0` (numeric zero in any
//! form, `0%` included though not valid CSS), `font-size: 0` (any unit, and
//! the `font` shorthand's size component), the `hidden` attribute (a boolean
//! attribute - any value hides, including `hidden="until-found"`), and
//! `aria-hidden="true"` (case-insensitive). The four CSS triggers are
//! tracked as independent tri-states in [`StyleProps`] and merged
//! independently through the cascade, so a declaration only cancels its own
//! property's hide: `font-size: 0; display: inline` and `opacity: 0;
//! display: block` both still hide (a later/higher-priority `display`
//! cannot undo a `font-size`/`opacity`/`visibility` hide, and vice versa) -
//! only `display: <not none>` cancels a prior `display: none`, only
//! `visibility: visible` cancels a prior `visibility: hidden`, and only a
//! non-zero `opacity`/`font-size` cancels a prior zero one. A hidden
//! ancestor hides every descendant regardless of the descendant's own
//! declarations - [`Builder::walk_elem`] returns before recursing once an
//! element's own `display`/`visibility`/`opacity`/attribute props resolve
//! hidden, so a child's `display: block` never gets a chance to run (this
//! includes `<body>` itself, checked by [`to_blocks`], and structural
//! elements - `<li>`, `<tr>`, `<thead>`/`<tbody>`/`<tfoot>`, `<td>`/`<th>`,
//! `<caption>` - which are collected directly by [`Builder::parse_list`] and
//! [`Builder::walk_elem`]'s `"table"` arm rather than through the general
//! recursion, so those call sites re-check `element_props` themselves).
//!
//! `font-size: 0` is the one hiding trigger that does *not* work this way,
//! because unlike the others it is an ordinarily-inherited, resettable CSS
//! property with a legitimate visible use (`ul { font-size: 0 } li {
//! font-size: 16px }`, the classic inline-block whitespace-removal hack): a
//! zero font-size on a container must not blank out a descendant that
//! resets it. So `font_size_zero` is threaded through [`Inherited`] as an
//! effective, inheritable state alongside the style delta rather than
//! folded into `props.hidden`/the early return, and only [`Builder::push_text`]
//! consults it - a text node is dropped when its *effective* font-size
//! resolves to zero, while everything else (an `<img>`, table structure, …)
//! is unaffected by font-size.
//!
//! A list item left with no blocks because its only content was hidden is
//! dropped rather than kept as an empty marker; a genuinely empty
//! `<li></li>` in the source is unaffected. White-on-white text and
//! off-page positioning are out of scope: not reliably detectable from
//! markup alone. A closed (non-`open`) `<details>` is left as-is - its body
//! is a UI click away, not author-hidden content, so it still renders.
//!
//! A `<a>` whose label was emptied *by dropping hidden content* is dropped
//! in full rather than falling back to its URL as link text the way a
//! genuinely empty `<a>` in the source still does - otherwise
//! `<a href="..."><span hidden>label</span></a>` would render its href as
//! visible prose. See the `"a"` arm of [`Builder::walk_inline`] and
//! [`Builder::dropped_hidden`].

use crate::error::ConvertError;
use crate::model::{
    AnchorId, Block, Cell, GridBuilder, ImageSource, Inline, LinkTarget, List, ListItem,
    MarkerKind, TableKind, inlines_are_empty, inlines_to_plain_text,
};
use crate::package::xml::{Element, Node};
use crate::shared::delta::{StyleDelta, rebase_emphasis};
use crate::shared::header::resolve_header_rows;
use crate::shared::math::{mathml_is_display, mathml_to_tex};
use crate::shared::text::{clean_text, collapse_ws};
use std::collections::HashMap;

/// Frontend hooks: how hrefs, image sources, and anchor ids resolve in the
/// containing document (EPUB scopes them per chapter).
pub trait HtmlCtx {
    fn link_target(&self, href: &str) -> Option<LinkTarget>;
    /// A failed load degrades to `Ok(None)`; resource-limit errors propagate.
    fn image_source(&self, src: &str) -> Result<Option<ImageSource>, ConvertError>;
    fn anchor_id(&self, raw: &str) -> AnchorId;
}

pub fn to_blocks(
    body: &Element,
    css: &Stylesheet,
    ctx: &dyn HtmlCtx,
) -> Result<Vec<Block>, ConvertError> {
    let dropped_hidden = std::cell::Cell::new(false);
    let mut builder = Builder {
        blocks: Vec::new(),
        inlines: Vec::new(),
        css,
        ctx,
        start_boundary: true,
        dropped_hidden: &dropped_hidden,
    };
    // `<body>` itself never goes through `walk_elem`, so its own hiding
    // declarations (or an ancestor `<html>` rule reaching it) would
    // otherwise be silently ignored; its own delta is threaded in as the
    // initial one for the same reason `walk_elem` does for every other
    // element.
    let body_props = builder.element_props(body);
    if body_props.hidden {
        return Ok(Vec::new());
    }
    let inherited = Inherited::default().merge(body_props);
    builder.walk_children(body, inherited)?;
    Ok(builder.finish())
}

// ---------------------------------------------------------------------------
// Minimal CSS

#[derive(Debug, Clone, Copy, Default)]
pub struct StyleProps {
    pub delta: StyleDelta,
    /// The four CSS hiding triggers, each an independent tri-state: `Some`
    /// once a declaration for that specific property has been seen on this
    /// element, cleared only by another declaration of the *same* property
    /// (see the module docs). Kept separate - rather than folded into one
    /// `hidden` flag - so `font-size: 0; display: inline` cannot un-hide
    /// itself: each field merges independently in [`StyleProps::merge`].
    display_none: Option<bool>,
    visibility_hidden: Option<bool>,
    opacity_zero: Option<bool>,
    font_size_zero: Option<bool>,
    /// The element's final hidden decision: any trigger above resolving
    /// true, or the `hidden` attribute / `aria-hidden="true"`. Computed once
    /// by [`Builder::element_props`] after the cascade is fully merged; the
    /// per-property fields above are what participate in the cascade
    /// itself, so this is always `false` on a [`StyleProps`] fresh out of
    /// [`parse_declarations`] or a stylesheet rule.
    pub hidden: bool,
}

impl StyleProps {
    fn merge(self, over: StyleProps) -> StyleProps {
        StyleProps {
            delta: self.delta.merge(over.delta),
            display_none: over.display_none.or(self.display_none),
            visibility_hidden: over.visibility_hidden.or(self.visibility_hidden),
            opacity_zero: over.opacity_zero.or(self.opacity_zero),
            font_size_zero: over.font_size_zero.or(self.font_size_zero),
            hidden: false,
        }
    }

    fn is_default(&self) -> bool {
        self.delta == StyleDelta::default()
            && self.display_none.is_none()
            && self.visibility_hidden.is_none()
            && self.opacity_zero.is_none()
            && self.font_size_zero.is_none()
    }
}

/// What [`Builder`] threads down through recursion: the style delta plus the
/// ancestor chain's effective `font-size: 0` state. `font-size` is an
/// ordinarily-inherited CSS property with a legitimate visible use
/// (`container { font-size: 0 } item { font-size: 16px }`), so unlike the
/// blanket hiding triggers folded into [`StyleProps::hidden`] it cannot be
/// handled by an early return in [`Builder::walk_elem`] - a descendant's own
/// `font-size` must be able to cancel an ancestor's zero one. Instead each
/// element's own declaration (if any) overrides the inherited value, and
/// only [`Builder::push_text`] consults the result - text is dropped when
/// its *effective* font-size resolves to zero, while non-text content
/// (`<img>`, table structure, …) is unaffected either way.
#[derive(Debug, Clone, Copy, Default)]
struct Inherited {
    delta: StyleDelta,
    font_size_zero: bool,
}

impl Inherited {
    /// Merge one element's own resolved [`StyleProps`] into the ancestor
    /// state: `delta` merges as it always has, and `font_size_zero`
    /// inherits from the ancestor unless this element declared its own
    /// `font-size` (zero or not).
    fn merge(self, props: StyleProps) -> Inherited {
        Inherited {
            delta: self.delta.merge(props.delta),
            font_size_zero: props.font_size_zero.unwrap_or(self.font_size_zero),
        }
    }
}

/// Cascade priority tiers: rules < inline style < `!important` rules <
/// `!important` inline style, with selector specificity ordering within a
/// tier and source order breaking ties.
const INLINE_PRIORITY: u32 = 100_000;
const IMPORTANT_PRIORITY: u32 = 1_000_000;

/// The two attribute selectors honored - see [`Stylesheet::add`]. Both name
/// attributes that already force an element hidden outright (see
/// [`Builder::element_props`]'s attribute check), so matching them changes
/// no *behavior* today; supporting them just keeps a stylesheet's own
/// `[hidden] { display: none }`/`[aria-hidden="true"] { display: none }`
/// convention rule from being silently dropped as unsupported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttrSelector {
    /// `[hidden]`.
    Hidden,
    /// `[aria-hidden="true"]` / `[aria-hidden=true]` (quotes optional).
    AriaHiddenTrue,
}

#[derive(Debug)]
struct Rule {
    tag: Option<String>,
    id: Option<String>,
    class: Option<String>,
    attr: Option<AttrSelector>,
    /// Cascade priority: the tier base plus selector specificity within the
    /// supported subset - an id outweighs any number of classes/attributes,
    /// which in turn outweigh any number of tags (`#i` = 100, `.c` = 10,
    /// `[hidden]` = 10, `t.c` = 11, `t` = 1). Source order breaks ties
    /// (rules are stored in document order).
    priority: u32,
    props: StyleProps,
}

#[derive(Debug, Default)]
pub struct Stylesheet {
    rules: Vec<Rule>,
}

/// One compound selector's supported components.
#[derive(Debug, Default)]
struct ParsedSelector {
    tag: Option<String>,
    id: Option<String>,
    class: Option<String>,
    attr: Option<AttrSelector>,
}

/// Parse one compound selector's supported components, or `None` when it
/// uses anything outside the subset (a combinator, a pseudo-class, an
/// attribute selector other than the two in [`AttrSelector`], two selectors
/// of the same kind, whitespace inside an attribute selector's brackets
/// (`[hidden ]`, `[aria-hidden = "true"]`) - valid CSS, but rejected along
/// with everything else the leading whitespace check below skips, ...). `s`
/// must already be a single trimmed, non-empty selector with no whitespace
/// (a combinator is out of scope the moment it splits the selector into more
/// than one word).
fn parse_simple_selector(s: &str) -> Option<ParsedSelector> {
    if s.contains(':') || s.contains(char::is_whitespace) || s.contains(['>', '+', '~', '*']) {
        return None; // pseudo-classes/combinators/universal: out of subset
    }
    let mut tag = None;
    let mut id = None;
    let mut class = None;
    let mut attr = None;
    let mut rest = s;
    // An optional leading type selector, ending at the first `.`/`#`/`[`.
    if !rest.starts_with(['.', '#', '[']) {
        let end = rest.find(['.', '#', '[']).unwrap_or(rest.len());
        if end == 0 {
            return None; // empty tag name before a component
        }
        tag = Some(rest[..end].to_ascii_lowercase());
        rest = &rest[end..];
    }
    while !rest.is_empty() {
        let mark = rest.as_bytes()[0];
        let end = rest[1..].find(['.', '#', '[']).map_or(rest.len(), |i| i + 1);
        let body = &rest[1..end];
        match mark {
            b'.' => {
                if body.is_empty() || class.is_some() {
                    return None; // empty/duplicate class: out of subset
                }
                class = Some(body.to_string());
            }
            b'#' => {
                if body.is_empty() || id.is_some() {
                    return None; // empty/duplicate id: out of subset
                }
                id = Some(body.to_string());
            }
            b'[' => {
                if !body.ends_with(']') || attr.is_some() {
                    return None; // unterminated/duplicate attribute selector
                }
                // No `.trim()` here: the whole selector was already
                // rejected above if it contains any whitespace at all, so
                // `inner` (and `name`/`value` below) can never carry any -
                // `[hidden ]`/`[aria-hidden = "true"]` are out of the
                // subset, not trimmed into matching.
                let inner = &body[..body.len() - 1];
                let matched = if inner.eq_ignore_ascii_case("hidden") {
                    Some(AttrSelector::Hidden)
                } else {
                    inner.split_once('=').and_then(|(name, value)| {
                        let value = value.trim_matches(['"', '\'']);
                        (name.eq_ignore_ascii_case("aria-hidden")
                            && value.eq_ignore_ascii_case("true"))
                        .then_some(AttrSelector::AriaHiddenTrue)
                    })
                };
                attr = Some(matched?); // any other attribute selector: out of subset
            }
            _ => unreachable!("loop only advances to one of . # ["),
        }
        rest = &rest[end..];
    }
    Some(ParsedSelector { tag, id, class, attr })
}

impl Stylesheet {
    /// Add rules from one stylesheet's text. Selectors participate when they
    /// are, or combine, a `tag`, `.class`, `#id`, and/or one of the two
    /// attribute selectors in [`AttrSelector`] - so `tag`, `.class`, `#id`,
    /// `tag.class`, `tag#id`, `[hidden]`, and `[aria-hidden="true"]` all
    /// match, as does mixing them (`tag#id.class`). Not honored, and simply
    /// skipped: descendant/child/sibling combinators (`div p`, `div > p`,
    /// `p ~ span`), pseudo-classes/elements (`p:first-child`), the universal
    /// selector (`*`), any attribute selector other than the two above
    /// (`[data-x]`), a compound selector repeating the same kind of
    /// component (`.a.b`, `#a#b`), and any at-rule (`@media`, `@supports`,
    /// `@import`, or any other nested block) - this parser has no notion of
    /// nesting, so an at-rule's inner declarations are never reached.
    /// `!important` declarations enter the higher cascade tier. A selector
    /// list (`a, b`) is split on `,` and each part is parsed and skipped
    /// independently, so one unsupported part does not drop the others.
    pub fn add(&mut self, css: &str) {
        let css = strip_css_comments(css);
        for chunk in css.split('}') {
            let Some((selectors, body)) = chunk.split_once('{') else {
                continue;
            };
            let decls = parse_declarations(body);
            if decls.normal.is_default() && decls.important.is_default() {
                continue;
            }
            for selector in selectors.split(',') {
                let s = selector.trim();
                if s.is_empty() {
                    continue;
                }
                let Some(ParsedSelector { tag, id, class, attr }) = parse_simple_selector(s) else {
                    continue; // out of the supported subset
                };
                let specificity = u32::from(id.is_some()) * 100
                    + u32::from(class.is_some()) * 10
                    + u32::from(attr.is_some()) * 10
                    + u32::from(tag.is_some());
                for (props, base) in [(decls.normal, 0), (decls.important, IMPORTANT_PRIORITY)] {
                    if !props.is_default() {
                        self.rules.push(Rule {
                            tag: tag.clone(),
                            id: id.clone(),
                            class: class.clone(),
                            attr,
                            priority: base + specificity,
                            props,
                        });
                    }
                }
            }
        }
    }

    /// Number of rules parsed so far - a test-only hook to pin
    /// [`parse_simple_selector`]'s behavior directly (whether a given
    /// selector text produces a rule at all), independent of whether the
    /// element it would match happens to be hidden some other way too.
    #[cfg(test)]
    fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// Matching rules for one element as (priority, props) pairs. `hidden`
    /// and `aria_hidden_true` report whether the element carries the two
    /// attributes [`AttrSelector`] can match.
    fn matching_rules(
        &self,
        tag: &str,
        id: Option<&str>,
        classes: &[&str],
        hidden: bool,
        aria_hidden_true: bool,
    ) -> Vec<(u32, StyleProps)> {
        self.rules
            .iter()
            .filter(|rule| {
                rule.tag.as_deref().is_none_or(|t| t == tag)
                    && rule.id.as_deref().is_none_or(|i| Some(i) == id)
                    && rule.class.as_deref().is_none_or(|c| classes.contains(&c))
                    && match rule.attr {
                        None => true,
                        Some(AttrSelector::Hidden) => hidden,
                        Some(AttrSelector::AriaHiddenTrue) => aria_hidden_true,
                    }
            })
            .map(|rule| (rule.priority, rule.props))
            .collect()
    }
}

fn strip_css_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        match rest[start..].find("*/") {
            Some(end) => rest = &rest[start + end + 2..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// A declaration block's properties, split by cascade tier.
#[derive(Debug, Clone, Copy, Default)]
struct DeclProps {
    normal: StyleProps,
    important: StyleProps,
}

/// The semantic subset of a declaration block.
fn parse_declarations(body: &str) -> DeclProps {
    let mut out = DeclProps::default();
    for decl in body.split(';') {
        let Some((name, value)) = decl.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let mut value = value.trim().to_ascii_lowercase();
        // `!important` moves the declaration into the higher cascade tier.
        let mut important = false;
        if let Some(pos) = value.find('!') {
            if value[pos + 1..].trim() == "important" {
                important = true;
            }
            value.truncate(pos);
            value.truncate(value.trim_end().len());
        }
        let props = if important { &mut out.important } else { &mut out.normal };
        match name.as_str() {
            "font-weight" => {
                props.delta.bold = Some(
                    value == "bold"
                        || value == "bolder"
                        || value.parse::<u32>().is_ok_and(|n| n >= 600),
                );
            }
            "font-style" => {
                props.delta.italic = Some(value == "italic" || value == "oblique");
            }
            "text-decoration" | "text-decoration-line" => {
                if value.contains("line-through") {
                    props.delta.strike = Some(true);
                } else if value == "none" {
                    props.delta.strike = Some(false);
                }
            }
            "display" => {
                props.display_none = Some(value == "none");
            }
            "visibility" => {
                if value == "hidden" || value == "collapse" {
                    props.visibility_hidden = Some(true);
                } else if value == "visible" {
                    props.visibility_hidden = Some(false);
                }
            }
            "opacity" => {
                if let Some(zero) = numeric_is_zero(&value) {
                    props.opacity_zero = Some(zero);
                }
            }
            "font-size" => {
                if let Some(zero) = numeric_is_zero(&value) {
                    props.font_size_zero = Some(zero);
                }
            }
            "font" => {
                // The `font` shorthand's size is whichever token is
                // numeric (optionally with a `/line-height` suffix, e.g.
                // `font: 0/0 a`): the preceding `style`/`variant`/`weight`
                // keywords and the trailing font-family aren't, so the
                // first token that parses as a number is the size.
                for token in value.split_whitespace() {
                    let size = token.split('/').next().unwrap_or(token);
                    if let Some(zero) = numeric_is_zero(size) {
                        props.font_size_zero = Some(zero);
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Whether a CSS numeric value - with or without a trailing unit (`px`,
/// `em`, `pt`, `%`) - is zero, or `None` when it isn't a plain number at all
/// (a keyword like `inherit`, which leaves the corresponding hiding trigger
/// untouched rather than asserting non-zero). Used for `opacity` and
/// `font-size`, where `0`, `0.0`, `0px`, and `0%` all read as "invisible"
/// even though not every combination is valid CSS (`opacity` takes no unit,
/// but documents spell it like a length anyway often enough to be worth
/// tolerating). A non-zero result is `Some(false)` rather than `None` so
/// that, on the same element, it can cancel a lower-priority declaration of
/// the *same* property that hid via zero (see [`StyleProps`]).
fn numeric_is_zero(value: &str) -> Option<bool> {
    let numeric = value.trim_end_matches(|c: char| c.is_ascii_alphabetic() || c == '%');
    numeric.parse::<f64>().ok().map(|n| n == 0.0)
}

// ---------------------------------------------------------------------------
// Walking

struct Builder<'e> {
    blocks: Vec<Block>,
    inlines: Vec<Inline>,
    css: &'e Stylesheet,
    ctx: &'e dyn HtmlCtx,
    /// Whether text appended while `inlines` is empty sits at a whitespace
    /// boundary (block start: leading whitespace collapses away; inline
    /// sub-builders inherit the surrounding run's state instead).
    start_boundary: bool,
    /// Set whenever [`Builder::walk_elem`] or [`Builder::push_text`] drops
    /// content because it was author-hidden (as opposed to never having
    /// been there at all) - shared across every sub-[`Builder`] spawned by
    /// [`Builder::sub_blocks_at`] via the `Cell`, since a `<span hidden>`
    /// inside an `<a>`'s label walks through a fresh sub-builder rather than
    /// `self`. Consulted (and scoped with a save/restore) only by the `"a"`
    /// arm of [`Builder::walk_inline`]: an anchor emptied by dropping hidden
    /// content must not fall back to showing its URL as label text the way
    /// a source-empty `<a>` does.
    dropped_hidden: &'e std::cell::Cell<bool>,
}

/// Content worth keeping: visible text, or an anchor node some link may
/// target (the renderer drops unreferenced anchors itself).
fn keeps_paragraph(inlines: &[Inline]) -> bool {
    !inlines_are_empty(inlines) || inlines.iter().any(|i| matches!(i, Inline::Anchor(_)))
}

/// Whether text appended after `inlines` sits at a whitespace boundary
/// (its leading spaces collapse away): at a block start, after emitted
/// whitespace or a line break, looking through zero-width anchors.
fn at_space_boundary(inlines: &[Inline], start: bool) -> bool {
    for inline in inlines.iter().rev() {
        match inline {
            Inline::Anchor(_) => continue,
            Inline::Text { text, .. } => {
                if text.is_empty() {
                    continue;
                }
                return text.ends_with(char::is_whitespace);
            }
            Inline::LineBreak => return true,
            Inline::Link { content, .. } => {
                if inlines_are_empty(content) {
                    continue;
                }
                return at_space_boundary(content, false);
            }
            Inline::Image { .. } | Inline::NoteRef(_) | Inline::Math(_) | Inline::Checkbox(_) => {
                return false;
            }
        }
    }
    start
}

impl Builder<'_> {
    fn flush_paragraph(&mut self) {
        if !self.inlines.is_empty() {
            let inlines = std::mem::take(&mut self.inlines);
            if keeps_paragraph(&inlines) {
                self.blocks.push(Block::Paragraph(inlines));
            }
        }
        self.start_boundary = true;
    }

    fn finish(mut self) -> Vec<Block> {
        self.flush_paragraph();
        self.blocks
    }

    /// Sub-walk in a fresh block context (list items, quotes, cells).
    fn sub_blocks(
        &mut self,
        elem: &Element,
        inherited: Inherited,
    ) -> Result<Vec<Block>, ConvertError> {
        self.sub_blocks_at(elem, inherited, true)
    }

    /// Sub-walk starting at the given whitespace-boundary state (`false`
    /// when the sub-content continues an inline run, so its leading space
    /// stays significant relative to the surrounding text).
    fn sub_blocks_at(
        &mut self,
        elem: &Element,
        inherited: Inherited,
        start_boundary: bool,
    ) -> Result<Vec<Block>, ConvertError> {
        let mut b = Builder {
            blocks: Vec::new(),
            inlines: Vec::new(),
            css: self.css,
            ctx: self.ctx,
            start_boundary,
            dropped_hidden: self.dropped_hidden,
        };
        b.walk_children(elem, inherited)?;
        Ok(b.finish())
    }

    /// Element-level props: matching stylesheet rules and the inline `style`
    /// attribute merged in one cascade order — normal rules, inline style,
    /// `!important` rules, `!important` inline style.
    fn element_props(&self, elem: &Element) -> StyleProps {
        let classes: Vec<&str> =
            elem.attr_any("class").map(|c| c.split_whitespace().collect()).unwrap_or_default();
        let id = elem.attr_any("id");
        let has_hidden_attr = elem.attr_any("hidden").is_some();
        let aria_hidden_true =
            elem.attr_any("aria-hidden").is_some_and(|v| v.trim().eq_ignore_ascii_case("true"));
        let mut entries =
            self.css.matching_rules(&elem.local, id, &classes, has_hidden_attr, aria_hidden_true);
        if let Some(style) = elem.attr_any("style") {
            let decls = parse_declarations(style);
            entries.push((INLINE_PRIORITY, decls.normal));
            entries.push((IMPORTANT_PRIORITY + INLINE_PRIORITY, decls.important));
        }
        entries.sort_by_key(|&(priority, _)| priority); // stable: keeps source order
        let mut props = StyleProps::default();
        for (_, entry) in entries {
            props = props.merge(entry);
        }
        // Fold three of the four triggers into one blanket decision: any of
        // them resolving true hides the whole subtree, and only that same
        // property's own opposite value (handled above, per declaration)
        // can cancel it - a `display: block` cannot cancel an `opacity: 0`
        // hide, etc. `font_size_zero` deliberately stays out of this fold
        // (and so out of the early-return in `walk_elem`): it is an
        // inherited, resettable property, not a blanket subtree hide - see
        // the module docs and [`Inherited`].
        props.hidden = props.display_none == Some(true)
            || props.visibility_hidden == Some(true)
            || props.opacity_zero == Some(true);
        // The `hidden` attribute (a boolean attribute: any value, including
        // `hidden="until-found"`, hides) and `aria-hidden="true"`
        // (case-insensitively; HTML enumerated attribute values are ASCII
        // case-insensitive) are HTML semantics, not CSS - they sit outside
        // the cascade above and force hidden regardless of any
        // `display`/`visibility`/etc. declaration, unlike a browser's
        // low-specificity UA rule for `[hidden]` which an author style can
        // override.
        if has_hidden_attr || aria_hidden_true {
            props.hidden = true;
        }
        props
    }

    /// Whether `elem` resolves hidden, recording the drop in
    /// [`Builder::dropped_hidden`] when it does. [`Builder::walk_elem`]'s own
    /// early return records the drop itself since it already has `props` in
    /// hand for [`Builder::merge_props`]; every other site that filters
    /// children by `element_props(..).hidden` without going through
    /// `walk_elem` - list items, table structure (row groups, rows, cells),
    /// and hidden subtrees inside `<pre>` - has no other reason to compute
    /// `element_props`, so it goes through this helper instead. Without it,
    /// an `<a>` whose whole label is such a structural element (e.g.
    /// `<a href><ul><li hidden>x</li></ul></a>`) would empty the label
    /// without setting the flag, and the `"a"` arm of
    /// [`Builder::walk_inline`] would wrongly treat it as a source-empty
    /// `<a>` and fall back to showing the href as link text.
    fn hidden(&self, elem: &Element) -> bool {
        let hidden = self.element_props(elem).hidden;
        if hidden {
            self.dropped_hidden.set(true);
        }
        hidden
    }

    /// Fold one element's own resolved props into the ancestor-inherited
    /// state: cascade order is inherited delta, then the tag's
    /// presentational default (`<b>`, `<i>`, …), then this element's CSS -
    /// so `font-weight: normal` can undo a `<b>` - while `font_size_zero`
    /// inherits unless this element declared its own `font-size` (see
    /// [`Inherited`]). Shared by [`Builder::walk_elem`] and the directly
    /// collected `<caption>` (which does not otherwise go through
    /// `walk_elem`).
    fn merge_props(&self, elem: &Element, inherited: Inherited, props: StyleProps) -> Inherited {
        Inherited {
            delta: merge_inline_tag(elem, inherited.delta).merge(props.delta),
            font_size_zero: props.font_size_zero.unwrap_or(inherited.font_size_zero),
        }
    }

    fn push_anchor(&mut self, elem: &Element) {
        if let Some(id) = elem.attr_any("id").filter(|i| !i.is_empty()) {
            self.inlines.push(Inline::Anchor(self.ctx.anchor_id(id)));
        }
        if elem.local == "a"
            && let Some(name) = elem.attr_any("name").filter(|n| !n.is_empty())
        {
            self.inlines.push(Inline::Anchor(self.ctx.anchor_id(name)));
        }
    }

    fn walk_children(&mut self, elem: &Element, inherited: Inherited) -> Result<(), ConvertError> {
        for node in &elem.children {
            match node {
                Node::Text(t) => self.push_text(t, inherited),
                Node::Elem(e) => self.walk_elem(e, inherited)?,
            }
        }
        Ok(())
    }

    fn push_text(&mut self, text: &str, inherited: Inherited) {
        // `font-size: 0` (directly declared, or inherited from an ancestor
        // that never had it reset) blanks only the text itself - see the
        // module docs and [`Inherited`].
        if inherited.font_size_zero {
            self.dropped_hidden.set(true);
            return;
        }
        let collapsed = collapse_ws(&clean_text(text));
        if collapsed.is_empty() {
            return;
        }
        // Collapse across node boundaries: leading whitespace vanishes at a
        // block start and after already-emitted whitespace, no matter how
        // the source split its text nodes and inline elements.
        let text = if at_space_boundary(&self.inlines, self.start_boundary) {
            collapsed.trim_start_matches(' ').to_string()
        } else {
            collapsed
        };
        if text.is_empty() {
            return;
        }
        self.inlines.push(Inline::Text { text, style: inherited.delta.resolve() });
    }

    fn walk_elem(&mut self, elem: &Element, inherited: Inherited) -> Result<(), ConvertError> {
        let props = self.element_props(elem);
        if props.hidden {
            self.dropped_hidden.set(true);
            return Ok(());
        }
        let inherited = self.merge_props(elem, inherited, props);
        match elem.local.as_str() {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.flush_paragraph();
                let level = elem.local[1..].parse::<u8>().unwrap_or(1);
                let mut content = self.inline_children(elem, inherited)?;
                // The heading element's own styling is how it looks, not
                // markup applied to the words; a `<b>` inside it still is.
                rebase_emphasis(&mut content, inherited.delta.resolve());
                let anchor = elem.attr_any("id").map(|id| self.ctx.anchor_id(id));
                if !inlines_are_empty(&content) {
                    self.blocks.push(Block::Heading { level, anchor, content });
                } else {
                    // An empty heading still carries link targets: its own
                    // id and any anchors inside.
                    let kept: Vec<Inline> = anchor
                        .map(Inline::Anchor)
                        .into_iter()
                        .chain(content.into_iter().filter(|i| matches!(i, Inline::Anchor(_))))
                        .collect();
                    if !kept.is_empty() {
                        self.blocks.push(Block::Paragraph(kept));
                    }
                }
            }
            "p" => {
                self.flush_paragraph();
                self.push_anchor(elem);
                let mut content = std::mem::take(&mut self.inlines);
                content.extend(self.inline_children(elem, inherited)?);
                if keeps_paragraph(&content) {
                    self.blocks.push(Block::Paragraph(content));
                }
            }
            "ul" | "ol" => {
                self.flush_paragraph();
                let lists = self.parse_list(elem, inherited)?;
                self.blocks.extend(lists);
            }
            "table" => {
                self.flush_paragraph();
                // Collected directly rather than through `walk_elem`, like
                // the row/cell/group elements `parse_table` collects below -
                // so its own hiding declarations need the same re-check
                // `element_props` gives every other structural site.
                if let Some(caption) = elem.child_elems().find(|e| e.local == "caption") {
                    let caption_props = self.element_props(caption);
                    if caption_props.hidden {
                        self.dropped_hidden.set(true);
                    } else {
                        let caption_inherited = self.merge_props(caption, inherited, caption_props);
                        let content = self.inline_children(caption, caption_inherited)?;
                        if keeps_paragraph(&content) {
                            self.blocks.push(Block::Paragraph(content));
                        }
                    }
                }
                if let Some(t) = self.parse_table(elem, inherited)? {
                    self.blocks.push(t);
                }
            }
            "blockquote" => {
                self.flush_paragraph();
                let inner = self.sub_blocks(elem, inherited)?;
                if !inner.is_empty() {
                    self.blocks.push(Block::BlockQuote(inner));
                }
            }
            "pre" => {
                self.flush_paragraph();
                let mut text = String::new();
                self.collect_visible_text(elem, &mut text);
                if !text.trim().is_empty() {
                    self.blocks.push(Block::CodeBlock { lang: None, text });
                }
            }
            "hr" => {
                self.flush_paragraph();
                self.blocks.push(Block::Rule);
            }
            "math" => {
                let tex = mathml_to_tex(elem);
                if tex.is_empty() {
                } else if mathml_is_display(elem) {
                    self.flush_paragraph();
                    self.blocks.push(Block::Math(tex));
                } else {
                    self.inlines.push(Inline::Math(tex));
                }
            }
            name if is_container_tag(name) => {
                self.push_anchor(elem);
                if has_block_children(elem) {
                    self.flush_paragraph();
                    self.walk_children(elem, inherited)?;
                    self.flush_paragraph();
                } else {
                    self.walk_children(elem, inherited)?;
                }
            }
            "script" | "style" | "head" | "template" | "noscript" => {}
            _ => self.walk_inline(elem, inherited)?,
        }
        Ok(())
    }

    /// Concatenated text of `elem`'s descendants, skipping any subtree whose
    /// own element props resolve hidden - `<pre>` renders its text verbatim
    /// rather than through [`Builder::push_text`], so it needs its own
    /// hidden check instead of inheriting one for free.
    fn collect_visible_text(&self, elem: &Element, out: &mut String) {
        for node in &elem.children {
            match node {
                Node::Text(t) => out.push_str(t),
                Node::Elem(e) => {
                    if !self.hidden(e) {
                        self.collect_visible_text(e, out);
                    }
                }
            }
        }
    }

    fn walk_inline(&mut self, elem: &Element, inherited: Inherited) -> Result<(), ConvertError> {
        self.push_anchor(elem);
        match elem.local.as_str() {
            "br" => self.inlines.push(Inline::LineBreak),
            "img" | "image" => {
                let alt = clean_text(elem.attr_any("alt").unwrap_or(""));
                let src = elem.attr_any("src").or_else(|| elem.attr_any("href")).unwrap_or("");
                let source = self.ctx.image_source(src)?;
                if source.is_some() || !alt.trim().is_empty() {
                    self.inlines.push(Inline::Image {
                        alt,
                        source: source.unwrap_or(ImageSource::Unavailable),
                    });
                }
            }
            "a" => {
                let target = elem.attr_any("href").and_then(|href| self.ctx.link_target(href));
                // Scoped to just this anchor's children: save/restore around
                // the walk so a drop inside this label neither inherits an
                // unrelated drop from before the `<a>` nor leaks out to an
                // enclosing one.
                let outer_dropped_hidden = self.dropped_hidden.replace(false);
                let content = self.inline_children_at(
                    elem,
                    inherited,
                    at_space_boundary(&self.inlines, self.start_boundary),
                )?;
                let label_dropped_hidden = self.dropped_hidden.replace(outer_dropped_hidden);
                // A label left empty by dropping hidden content must not
                // fall back to the URL as visible text - that would leak an
                // attacker-controlled href the author tried to disguise
                // behind hidden text. A label that was simply never there
                // (a genuinely empty `<a>`) is unaffected: the renderer
                // still shows the URL as its link text.
                let leaked_by_hidden_label = label_dropped_hidden && inlines_are_empty(&content);
                match target {
                    Some(_) if leaked_by_hidden_label => {}
                    Some(target) => self.inlines.push(Inline::Link { content, target }),
                    None => self.inlines.extend(content),
                }
            }
            _ => self.walk_children(elem, inherited)?,
        }
        Ok(())
    }

    fn inline_children(
        &mut self,
        elem: &Element,
        inherited: Inherited,
    ) -> Result<Vec<Inline>, ConvertError> {
        self.inline_children_at(elem, inherited, true)
    }

    /// Inline children starting at the given whitespace-boundary state.
    fn inline_children_at(
        &mut self,
        elem: &Element,
        inherited: Inherited,
        start_boundary: bool,
    ) -> Result<Vec<Inline>, ConvertError> {
        let mut blocks = self.sub_blocks_at(elem, inherited, start_boundary)?;
        if blocks.len() == 1
            && let Block::Paragraph(inlines) = &mut blocks[0]
        {
            return Ok(std::mem::take(inlines));
        }
        let mut out = Vec::new();
        for (i, block) in blocks.into_iter().enumerate() {
            if i > 0 {
                out.push(Inline::LineBreak);
            }
            match block {
                Block::Paragraph(inlines) | Block::Heading { content: inlines, .. } => {
                    out.extend(inlines)
                }
                other => out.push(Inline::plain(collapse_ws(&block_text(&other)))),
            }
        }
        Ok(out)
    }

    /// Ordered/unordered list with `start`, `reversed`, `type`, and per-item
    /// `value`. Non-contiguous numbering splits into consecutive list blocks
    /// so the rendered numbers match the source.
    fn parse_list(
        &mut self,
        elem: &Element,
        inherited: Inherited,
    ) -> Result<Vec<Block>, ConvertError> {
        let ordered = elem.local == "ol";
        let items: Vec<&Element> =
            elem.child_elems().filter(|e| e.local == "li" && !self.hidden(e)).collect();
        if items.is_empty() {
            return Ok(Vec::new());
        }
        if !ordered {
            let mut list_items = Vec::with_capacity(items.len());
            for li in &items {
                let blocks = self.sub_blocks(li, inherited)?;
                // A visible `<li>` whose only content was itself dropped as
                // hidden leaves no blocks; keep it out rather than render a
                // bare marker for content that no longer exists. A source
                // `<li></li>` that was always empty is unaffected.
                if blocks.is_empty() && li_had_content(li) {
                    continue;
                }
                list_items.push(ListItem { blocks, marker_label: None });
            }
            if list_items.is_empty() {
                return Ok(Vec::new());
            }
            let list = List { marker: MarkerKind::Bullet, start: 1, items: list_items };
            return Ok(vec![Block::List(list)]);
        }
        let marker = match elem.attr_any("type") {
            Some("a") => MarkerKind::LowerAlpha,
            Some("A") => MarkerKind::UpperAlpha,
            Some("i") => MarkerKind::LowerRoman,
            Some("I") => MarkerKind::UpperRoman,
            _ => MarkerKind::Decimal,
        };
        let reversed = elem.attr_any("reversed").is_some();
        let start: i64 = elem
            .attr_any("start")
            .and_then(|v| v.parse().ok())
            .unwrap_or(if reversed { items.len() as i64 } else { 1 });
        let mut numbers: Vec<i64> = Vec::with_capacity(items.len());
        let mut next = start;
        for li in &items {
            if let Some(v) = li.attr_any("value").and_then(|v| v.parse::<i64>().ok()) {
                next = v;
            }
            numbers.push(next);
            // Source-controlled values sit anywhere in the i64 range; the
            // step must not overflow.
            next = if reversed { next.saturating_sub(1) } else { next.saturating_add(1) };
        }
        // Zero/negative numbers are valid ordered-list values but cannot be
        // a `start` for the renderer's start+index numbering; such lists
        // carry every number as an explicit literal marker instead.
        if numbers.iter().any(|&n| n < 1) {
            let mut list_items = Vec::with_capacity(items.len());
            for (li, &n) in items.iter().zip(&numbers) {
                let blocks = self.sub_blocks(li, inherited)?;
                if blocks.is_empty() && li_had_content(li) {
                    continue;
                }
                list_items.push(ListItem { blocks, marker_label: Some(format!("{n}.")) });
            }
            if list_items.is_empty() {
                return Ok(Vec::new());
            }
            return Ok(vec![Block::List(List { marker, start: 1, items: list_items })]);
        }
        let mut out: Vec<Block> = Vec::new();
        let mut current: Option<List> = None;
        let mut last_number = 0i64;
        for (li, &number) in items.iter().zip(&numbers) {
            let blocks = self.sub_blocks(li, inherited)?;
            if blocks.is_empty() && li_had_content(li) {
                // A skipped item's source number must not leak into the
                // surrounding run: it neither extends the current list (that
                // would misnumber the next real item against a number that
                // was never rendered) nor gets attached to one - the next
                // surviving item starts a fresh run at its own true number,
                // same as any other numbering discontinuity.
                continue;
            }
            let contiguous = current.is_some() && last_number.checked_add(1) == Some(number);
            if !contiguous {
                if let Some(list) = current.take()
                    && !list.items.is_empty()
                {
                    out.push(Block::List(list));
                }
                current = Some(List { marker, start: number as u64, items: Vec::new() });
            }
            current.as_mut().unwrap().items.push(ListItem { blocks, marker_label: None });
            last_number = number;
        }
        if let Some(list) = current
            && !list.items.is_empty()
        {
            out.push(Block::List(list));
        }
        Ok(out)
    }

    fn parse_table(
        &mut self,
        elem: &Element,
        inherited: Inherited,
    ) -> Result<Option<Block>, ConvertError> {
        // (row, is thead row, row-group index): each thead/tbody/tfoot is
        // one row group; consecutive direct `tr` children form an implicit
        // one. `rowspan="0"` spans to the end of its group.
        let mut row_elems: Vec<(&Element, bool, usize)> = Vec::new();
        let mut group = 0usize;
        let mut in_implicit_group = false;
        for child in elem.child_elems() {
            match child.local.as_str() {
                "thead" | "tbody" | "tfoot" => {
                    if in_implicit_group {
                        in_implicit_group = false;
                        group += 1;
                    }
                    // A hidden group (e.g. `<tbody hidden>`) drops every row
                    // in it; the group id is still consumed so later groups
                    // keep distinct ids (harmless - `group_end` below only
                    // maps ids that actually appear in `row_elems`).
                    if !self.hidden(child) {
                        let in_head = child.local == "thead";
                        for tr in child.child_elems().filter(|e| e.local == "tr" && !self.hidden(e))
                        {
                            row_elems.push((tr, in_head, group));
                        }
                    }
                    group += 1;
                }
                "tr" => {
                    in_implicit_group = true;
                    if !self.hidden(child) {
                        row_elems.push((child, false, group));
                    }
                }
                _ => {}
            }
        }
        if row_elems.is_empty() {
            return Ok(None);
        }
        // Last row index of each group, for rowspan=0 expansion.
        let mut group_end: HashMap<usize, usize> = HashMap::new();
        for (i, &(_, _, g)) in row_elems.iter().enumerate() {
            group_end.insert(g, i);
        }
        let mut builder = GridBuilder::new();
        let mut header_rows = 0usize;
        for (i, (tr, in_head, grp)) in row_elems.iter().enumerate() {
            builder.next_row();
            let mut all_th = true;
            let mut any_cell = false;
            for cell in tr.child_elems() {
                if !matches!(cell.local.as_str(), "td" | "th") {
                    continue;
                }
                if self.hidden(cell) {
                    continue;
                }
                any_cell = true;
                if cell.local != "th" {
                    all_th = false;
                }
                // HTML clamps colspan to 1000 and rowspan to 65534.
                let col_span: u32 = cell
                    .attr_any("colspan")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(1)
                    .clamp(1, 1000);
                let row_span: u32 = match cell.attr_any("rowspan").and_then(|v| v.parse().ok()) {
                    // rowspan=0: span all remaining rows of the row group.
                    Some(0) => (group_end[grp] - i + 1) as u32,
                    Some(n) => n.clamp(1, 65534),
                    None => 1,
                };
                let blocks = self.sub_blocks(cell, inherited)?;
                builder.place(Cell::spanning(blocks, col_span, row_span))?;
            }
            if i == header_rows && (*in_head || (all_th && any_cell)) {
                header_rows += 1;
            }
        }
        let mut table = builder.finish(TableKind::Data);
        if table.grid.is_empty() {
            return Ok(None);
        }
        table.header_rows = resolve_header_rows(&table, header_rows);
        Ok(Some(Block::Table(table)))
    }
}

fn merge_inline_tag(elem: &Element, mut delta: StyleDelta) -> StyleDelta {
    match elem.local.as_str() {
        "b" | "strong" => delta.bold = Some(true),
        "i" | "em" | "cite" | "dfn" | "var" => delta.italic = Some(true),
        "s" | "del" | "strike" => delta.strike = Some(true),
        "code" | "kbd" | "samp" | "tt" => delta.code = Some(true),
        _ => {}
    }
    delta
}

fn block_text(block: &Block) -> String {
    match block {
        Block::Paragraph(i) | Block::Heading { content: i, .. } => inlines_to_plain_text(i),
        Block::List(list) => list
            .items
            .iter()
            .flat_map(|it| it.blocks.iter().map(block_text))
            .collect::<Vec<_>>()
            .join(" "),
        Block::BlockQuote(blocks) => blocks.iter().map(block_text).collect::<Vec<_>>().join(" "),
        Block::CodeBlock { text, .. } | Block::Math(text) => text.clone(),
        Block::Table(table) => table
            .grid
            .iter()
            .flat_map(|row| {
                row.iter().filter_map(|slot| match slot {
                    crate::model::CellSlot::Origin(cell) => {
                        Some(cell.blocks.iter().map(block_text).collect::<Vec<_>>().join(" "))
                    }
                    crate::model::CellSlot::Covered { .. } => None,
                })
            })
            .collect::<Vec<_>>()
            .join(" "),
        Block::Rule => String::new(),
    }
}

/// Elements that only group other content, walked through transparently.
fn is_container_tag(name: &str) -> bool {
    matches!(
        name,
        "div"
            | "section"
            | "article"
            | "aside"
            | "main"
            | "nav"
            | "header"
            | "footer"
            | "figure"
            | "figcaption"
            | "center"
            | "details"
            | "summary"
            | "li"
            | "dl"
            | "dt"
            | "dd"
            | "body"
    )
}

fn is_block_tag(name: &str) -> bool {
    is_container_tag(name)
        || matches!(
            name,
            "p" | "ul"
                | "ol"
                | "table"
                | "blockquote"
                | "pre"
                | "hr"
                | "h1"
                | "h2"
                | "h3"
                | "h4"
                | "h5"
                | "h6"
        )
}

fn has_block_children(elem: &Element) -> bool {
    elem.child_elems().any(|e| is_block_tag(&e.local))
}

/// Whether an `<li>` had any content at all in the source (a child element,
/// or non-whitespace text) - distinguishes "ended up empty because its only
/// content was hidden" from a source `<li></li>` that was always empty, so
/// [`Builder::parse_list`] drops only the former as a bare marker.
fn li_had_content(li: &Element) -> bool {
    li.children.iter().any(|n| match n {
        Node::Text(t) => !t.trim().is_empty(),
        Node::Elem(_) => true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CellSlot, ImageSource, Style};
    use crate::package::xml::parse_xml;

    struct NullCtx;

    impl HtmlCtx for NullCtx {
        fn link_target(&self, href: &str) -> Option<LinkTarget> {
            (!href.is_empty()).then(|| LinkTarget::External(href.to_string()))
        }

        fn image_source(&self, _src: &str) -> Result<Option<ImageSource>, ConvertError> {
            Ok(None)
        }

        fn anchor_id(&self, raw: &str) -> AnchorId {
            raw.to_string()
        }
    }

    fn blocks_with_css(html: &str, css: &str) -> Vec<Block> {
        let tree = parse_xml(html.as_bytes()).unwrap();
        let body = tree.child_elems().next().unwrap();
        let mut sheet = Stylesheet::default();
        sheet.add(css);
        to_blocks(body, &sheet, &NullCtx).unwrap()
    }

    fn blocks(html: &str) -> Vec<Block> {
        blocks_with_css(html, "")
    }

    fn para_text(block: &Block) -> String {
        let Block::Paragraph(inlines) = block else { panic!("expected paragraph: {block:?}") };
        crate::model::inlines_to_plain_text(inlines)
    }

    fn first_text_style(inlines: &[Inline]) -> Style {
        for inline in inlines {
            match inline {
                Inline::Text { style, .. } => return *style,
                Inline::Link { content, .. } => return first_text_style(content),
                _ => {}
            }
        }
        panic!("no text run in {inlines:?}");
    }

    #[test]
    fn whitespace_collapses_across_inline_boundaries() {
        // M7: a lone space inside a span must survive between words, and
        // formatting splits must not double or drop spaces.
        let out = blocks("<body><p>foo<span> </span>bar</p></body>");
        assert_eq!(para_text(&out[0]), "foo bar");
        let out = blocks("<body><p>foo <span> bar</span></p></body>");
        assert_eq!(para_text(&out[0]), "foo bar");
        let out = blocks("<body><p>  foo\n  bar  </p></body>");
        assert_eq!(para_text(&out[0]), "foo bar ");
    }

    #[test]
    fn link_label_whitespace_joins_the_surrounding_run() {
        // `foo <a> bar</a>`: the label's leading space collapses with the
        // space already emitted before the link.
        let out = blocks(r#"<body><p>foo <a href="u"> bar</a></p></body>"#);
        let Block::Paragraph(inlines) = &out[0] else { panic!() };
        let Some(Inline::Link { content, .. }) =
            inlines.iter().find(|i| matches!(i, Inline::Link { .. }))
        else {
            panic!("no link in {inlines:?}");
        };
        assert_eq!(crate::model::inlines_to_plain_text(content), "bar");
        // `foo<a> bar</a>`: no space yet, so the label keeps its lead.
        let out = blocks(r#"<body><p>foo<a href="u"> bar</a></p></body>"#);
        assert_eq!(para_text(&out[0]), "foo bar");
    }

    #[test]
    fn inline_style_overrides_the_tag_default() {
        // M7: `<b style="font-weight: normal">` renders plain.
        let out = blocks(r#"<body><p><b style="font-weight: normal">x</b></p></body>"#);
        let Block::Paragraph(inlines) = &out[0] else { panic!() };
        assert!(!first_text_style(inlines).bold);
    }

    #[test]
    fn important_declarations_win_the_cascade() {
        let css = "span.x { font-weight: bold !important } span.x { font-weight: normal }";
        let out = blocks_with_css(r#"<body><p><span class="x">x</span></p></body>"#, css);
        let Block::Paragraph(inlines) = &out[0] else { panic!() };
        assert!(first_text_style(inlines).bold);
    }

    #[test]
    fn heading_styling_stays_out_of_its_content() {
        // A heading's own CSS is how the heading looks, so it does not reach
        // the runs; emphasis a child adds beyond it still does.
        let css = "h2 { font-style: italic; font-weight: bold }";
        let out = blocks_with_css("<body><h2>title <b>b</b></h2></body>", css);
        let Block::Heading { content, .. } = &out[0] else { panic!("{out:?}") };
        assert_eq!(first_text_style(content), Style::PLAIN);

        let out = blocks_with_css("<body><h2>title <b>b</b></h2></body>", "");
        let Block::Heading { content, .. } = &out[0] else { panic!("{out:?}") };
        assert_eq!(first_text_style(content), Style::PLAIN);
        let Some(Inline::Text { style, .. }) =
            content.iter().rfind(|i| matches!(i, Inline::Text { .. }))
        else {
            panic!()
        };
        assert!(style.bold);
    }

    #[test]
    fn extreme_ordered_list_values_do_not_overflow() {
        // H2: source-controlled `start`/`value` sit anywhere in i64.
        let html = format!(
            "<body><ol start=\"{}\"><li>a</li><li>b</li><li value=\"{}\">c</li><li>d</li></ol></body>",
            i64::MAX,
            i64::MIN,
        );
        let out = blocks(&html);
        assert!(!out.is_empty());
    }

    #[test]
    fn rowspan_zero_spans_to_the_end_of_the_row_group() {
        // M8: rowspan="0" covers the remaining rows of its row group only.
        let html = r#"<body><table>
            <tbody>
                <tr><td rowspan="0">tall</td><td>a</td></tr>
                <tr><td>b</td></tr>
            </tbody>
            <tbody><tr><td>c</td><td>d</td></tr></tbody>
        </table></body>"#;
        let out = blocks(html);
        let Block::Table(t) = &out[0] else { panic!("{out:?}") };
        assert!(matches!(&t.grid[0][0], CellSlot::Origin(c) if c.row_span == 2));
        assert!(matches!(t.grid[1][0], CellSlot::Covered { origin_row: 0, origin_col: 0 }));
        assert!(matches!(&t.grid[2][0], CellSlot::Origin(_)), "next group must not be covered");
    }

    #[test]
    fn visibility_hidden_and_collapse_drop_content_from_inline_style() {
        let out = blocks(r#"<body><p style="visibility: hidden">x</p><p>y</p></body>"#);
        assert_eq!(out.len(), 1);
        assert_eq!(para_text(&out[0]), "y");

        let out = blocks(r#"<body><p style="visibility: collapse">x</p><p>y</p></body>"#);
        assert_eq!(out.len(), 1);
        assert_eq!(para_text(&out[0]), "y");
    }

    #[test]
    fn visibility_hidden_from_a_style_rule_by_class() {
        let css = "p.gone { visibility: hidden }";
        let out = blocks_with_css(r#"<body><p class="gone">x</p><p>y</p></body>"#, css);
        assert_eq!(out.len(), 1);
        assert_eq!(para_text(&out[0]), "y");
    }

    #[test]
    fn opacity_zero_drops_content() {
        for value in ["0", "0.0", "0%"] {
            let html = format!(r#"<body><p style="opacity: {value}">x</p><p>y</p></body>"#);
            let out = blocks(&html);
            assert_eq!(out.len(), 1, "opacity: {value} should hide its paragraph");
            assert_eq!(para_text(&out[0]), "y");
        }
        // A non-zero opacity keeps the content.
        let out = blocks(r#"<body><p style="opacity: 0.5">x</p></body>"#);
        assert_eq!(out.len(), 1);
        assert_eq!(para_text(&out[0]), "x");
    }

    #[test]
    fn font_size_zero_drops_content_in_any_unit() {
        for value in ["0", "0px", "0em", "0pt", "0%"] {
            let html = format!(r#"<body><p style="font-size: {value}">x</p><p>y</p></body>"#);
            let out = blocks(&html);
            assert_eq!(out.len(), 1, "font-size: {value} should hide its paragraph");
            assert_eq!(para_text(&out[0]), "y");
        }
    }

    #[test]
    fn hidden_attribute_drops_content_regardless_of_value() {
        // XHTML must quote every attribute, so a boolean attribute is
        // written `hidden=""` (or `hidden="hidden"`) rather than bare
        // `hidden`; either still hides.
        let out = blocks(r#"<body><p hidden="">x</p><p>y</p></body>"#);
        assert_eq!(out.len(), 1);
        assert_eq!(para_text(&out[0]), "y");

        // Any value hides, including `hidden="until-found"`.
        let out = blocks(r#"<body><p hidden="until-found">x</p><p>y</p></body>"#);
        assert_eq!(out.len(), 1);
        assert_eq!(para_text(&out[0]), "y");
    }

    #[test]
    fn aria_hidden_true_drops_content() {
        let out = blocks(r#"<body><p aria-hidden="true">x</p><p>y</p></body>"#);
        assert_eq!(out.len(), 1);
        assert_eq!(para_text(&out[0]), "y");

        // Any other value is not a hiding signal.
        let out = blocks(r#"<body><p aria-hidden="false">x</p></body>"#);
        assert_eq!(out.len(), 1);
        assert_eq!(para_text(&out[0]), "x");
    }

    #[test]
    fn hidden_ancestor_hides_descendants_regardless_of_their_own_display() {
        // A child's `display: block` must not resurrect content inside a
        // `display: none` ancestor: descendants never see their own props
        // once the ancestor itself is skipped.
        let html = r#"<body>
            <div style="display: none">
                <p style="display: block">x</p>
            </div>
            <p>y</p>
        </body>"#;
        let out = blocks(html);
        assert_eq!(out.len(), 1);
        assert_eq!(para_text(&out[0]), "y");
    }

    #[test]
    fn closed_details_body_is_left_visible() {
        // A UI-collapsed <details> is not author-hidden content: its body
        // stays in the output whether or not `open` is present.
        let out = blocks("<body><details><summary>s</summary><p>x</p></details></body>");
        let text: String = out.iter().map(para_text).collect::<Vec<_>>().join(" ");
        assert!(text.contains('x'), "{out:?}");
    }

    #[test]
    fn hiding_properties_do_not_cancel_each_other() {
        // Each of the four CSS hiding triggers is independent: a later or
        // higher-priority declaration of a *different* property must not
        // resurrect content another property hid on the same element.
        let out = blocks(r#"<body><p style="font-size:0; display:inline">x</p></body>"#);
        assert!(out.is_empty(), "font-size:0 must survive display:inline: {out:?}");

        let out = blocks(r#"<body><p style="opacity:0; display:block">x</p></body>"#);
        assert!(out.is_empty(), "opacity:0 must survive display:block: {out:?}");

        let css = "p.h { display: none }";
        let out =
            blocks_with_css(r#"<body><p class="h" style="visibility: visible">x</p></body>"#, css);
        assert!(
            out.is_empty(),
            "display:none from a rule must survive visibility:visible: {out:?}"
        );

        // But a property's own opposite value, on the same element, still
        // cancels its own hide.
        let out = blocks(r#"<body><p style="opacity:0; opacity:0.5">x</p></body>"#);
        assert_eq!(out.len(), 1);
        assert_eq!(para_text(&out[0]), "x");
    }

    #[test]
    fn aria_hidden_true_is_case_insensitive() {
        let out = blocks(r#"<body><p aria-hidden="TRUE">x</p><p>y</p></body>"#);
        assert_eq!(out.len(), 1);
        assert_eq!(para_text(&out[0]), "y");

        let out = blocks(r#"<body><p aria-hidden=" True ">x</p><p>y</p></body>"#);
        assert_eq!(out.len(), 1);
        assert_eq!(para_text(&out[0]), "y");
    }

    #[test]
    fn hidden_body_yields_no_blocks() {
        let out = blocks(r#"<body hidden=""><p>x</p></body>"#);
        assert!(out.is_empty(), "{out:?}");

        let out = blocks(r#"<body style="display: none"><p>x</p></body>"#);
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn hidden_list_item_is_dropped() {
        let out = blocks(
            r#"<body><ul>
                <li>one</li>
                <li hidden="">two</li>
                <li style="display: none">three</li>
                <li>four</li>
            </ul></body>"#,
        );
        let Block::List(list) = &out[0] else { panic!("{out:?}") };
        let texts: Vec<String> = list.items.iter().map(|it| block_text(&it.blocks[0])).collect();
        assert_eq!(texts, vec!["one".to_string(), "four".to_string()]);
    }

    #[test]
    fn list_item_emptied_entirely_by_hidden_content_is_dropped() {
        // The bullet itself is not hidden, but its only content is -
        // leaving no blocks. It must not survive as a bare marker; a
        // genuinely empty `<li></li>` is a separate, unaffected case.
        let out = blocks(
            r#"<body><ul>
                <li>one</li>
                <li><p hidden="">gone</p></li>
                <li>three</li>
                <li></li>
            </ul></body>"#,
        );
        let Block::List(list) = &out[0] else { panic!("{out:?}") };
        assert_eq!(list.items.len(), 3, "{list:?}");
        assert_eq!(block_text(&list.items[0].blocks[0]), "one");
        assert_eq!(block_text(&list.items[1].blocks[0]), "three");
        assert!(list.items[2].blocks.is_empty(), "source-empty <li> stays, empty: {list:?}");
    }

    #[test]
    fn hidden_table_row_is_dropped() {
        let html = r#"<body><table>
            <tr><th>keep</th></tr>
            <tr hidden=""><th>gone</th></tr>
            <tr style="display: none"><td>also gone</td></tr>
            <tr><td>x</td></tr>
        </table></body>"#;
        let out = blocks(html);
        let Block::Table(t) = &out[0] else { panic!("{out:?}") };
        assert_eq!(t.grid.len(), 2, "hidden rows must not be counted: {t:?}");
        assert_eq!(t.header_rows, 1, "a hidden row must not be treated as a header row");
    }

    #[test]
    fn hidden_table_cell_is_dropped() {
        let html = r#"<body><table>
            <tr><td>a</td><td hidden="">b</td></tr>
        </table></body>"#;
        let out = blocks(html);
        let Block::Table(t) = &out[0] else { panic!("{out:?}") };
        // Only the visible cell should occupy the grid.
        assert_eq!(t.grid[0].len(), 1, "{t:?}");
    }

    #[test]
    fn hidden_table_row_group_drops_all_its_rows() {
        let html = r#"<body><table>
            <tbody hidden="">
                <tr><td>gone1</td></tr>
                <tr><td>gone2</td></tr>
            </tbody>
            <tbody><tr><td>kept</td></tr></tbody>
        </table></body>"#;
        let out = blocks(html);
        let Block::Table(t) = &out[0] else { panic!("{out:?}") };
        assert_eq!(t.grid.len(), 1, "{t:?}");
    }

    #[test]
    fn hidden_table_caption_is_dropped() {
        // <caption> is collected directly (not through walk_elem), like the
        // row/cell/group elements above - it needs the same hidden re-check.
        let html = r#"<body><table>
            <caption hidden="">CAPTION-HIDDEN-LEAK</caption>
            <tr><td>x</td></tr>
        </table></body>"#;
        let out = blocks(html);
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(matches!(&out[0], Block::Table(_)), "{out:?}");

        let html = r#"<body><table>
            <caption style="display: none">CAPTION-DISPLAYNONE-LEAK</caption>
            <tr><td>x</td></tr>
        </table></body>"#;
        let out = blocks(html);
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(matches!(&out[0], Block::Table(_)), "{out:?}");
    }

    #[test]
    fn visible_table_caption_still_renders_as_a_paragraph() {
        let html = r#"<body><table>
            <caption>Table Title</caption>
            <tr><td>x</td></tr>
        </table></body>"#;
        let out = blocks(html);
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(para_text(&out[0]), "Table Title");
        assert!(matches!(&out[1], Block::Table(_)), "{out:?}");
    }

    #[test]
    fn pre_text_skips_hidden_descendants() {
        let out =
            blocks(r#"<body><pre>pre-visible <span hidden="">PRE-HIDDEN-LEAK</span></pre></body>"#);
        let Block::CodeBlock { text, .. } = &out[0] else { panic!("{out:?}") };
        assert!(!text.contains("PRE-HIDDEN-LEAK"), "{text:?}");
        assert!(text.contains("pre-visible"), "{text:?}");
    }

    #[test]
    fn font_shorthand_zero_size_drops_content() {
        let out = blocks(r#"<body><p style="font: 0/0 a">x</p><p>y</p></body>"#);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(para_text(&out[0]), "y");
    }

    #[test]
    fn font_shorthand_with_nonzero_size_keeps_content() {
        let out = blocks(r#"<body><p style="font: 12px/1.4 serif">x</p></body>"#);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(para_text(&out[0]), "x");
    }

    #[test]
    fn opacity_and_font_size_zero_from_a_style_rule_by_class() {
        for prop in ["opacity: 0", "font-size: 0"] {
            let css = format!(".h {{ {prop} }}");
            let out = blocks_with_css(r#"<body><p class="h">x</p><p>y</p></body>"#, &css);
            assert_eq!(out.len(), 1, "{prop}: {out:?}");
            assert_eq!(para_text(&out[0]), "y", "{prop}");
        }
    }

    #[test]
    fn font_size_zero_container_does_not_hide_a_child_that_resets_it() {
        // `font-size: 0` on a container followed by a non-zero reset on a
        // descendant is a common visible pattern (the classic `ul.ib {
        // font-size: 0 } ul.ib li { font-size: 16px }` inline-block
        // whitespace-removal hack, among others); unlike
        // display/visibility/opacity, font-size is an ordinary inherited
        // CSS property, so a descendant's own value cancels the ancestor's
        // zero instead of the whole subtree being blanket-hidden as it used
        // to be. (The supported CSS subset has no descendant combinator, so
        // this exercises the same cascade with a `<div>`/`<p>` pair instead
        // of `ul`/`li`.)
        let out = blocks(
            r#"<body><div style="font-size: 0">
                <p style="font-size: 16px">IB-VISIBLE-ITEM-ONE</p>
                <p style="font-size: 16px">IB-VISIBLE-ITEM-TWO</p>
            </div></body>"#,
        );
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(para_text(&out[0]), "IB-VISIBLE-ITEM-ONE");
        assert_eq!(para_text(&out[1]), "IB-VISIBLE-ITEM-TWO");
    }

    #[test]
    fn font_size_zero_hides_only_text_that_does_not_reset_it() {
        let html = r#"<body><p style="font-size: 0">gone <span style="font-size: 16px">kept</span></p></body>"#;
        let out = blocks(html);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(para_text(&out[0]), "kept");
    }

    #[test]
    fn font_size_zero_does_not_hide_non_text_content() {
        // Replaced content (an image) is unaffected by font-size either way.
        let html = r#"<body><p style="font-size: 0"><img src="a.png" alt="pic"></p></body>"#;
        let out = blocks(html);
        let Block::Paragraph(inlines) = &out[0] else { panic!("{out:?}") };
        assert!(inlines.iter().any(|i| matches!(i, Inline::Image { .. })), "{inlines:?}");
    }

    #[test]
    fn skipped_ordered_list_item_does_not_shift_following_numbers() {
        // A middle item emptied entirely by hidden content must not steal a
        // number that renumbers the surviving items after it: the skipped
        // item is a discontinuity, so "three" starts a fresh run at its own
        // true source number (3) rather than landing at 2.
        let out = blocks(
            r#"<body><ol>
                <li>one</li>
                <li><span hidden="">gone</span></li>
                <li>three</li>
            </ol></body>"#,
        );
        let starts_and_text: Vec<(u64, String)> = out
            .iter()
            .map(|b| {
                let Block::List(l) = b else { panic!("{out:?}") };
                (l.start, block_text(&l.items[0].blocks[0]))
            })
            .collect();
        assert_eq!(
            starts_and_text,
            vec![(1, "one".to_string()), (3, "three".to_string())],
            "{out:?}"
        );
    }

    #[test]
    fn skipped_first_ordered_list_item_does_not_lower_the_next_items_number() {
        let out = blocks(
            r#"<body><ol>
                <li><span hidden="">gone</span></li>
                <li>two</li>
            </ol></body>"#,
        );
        let Block::List(list) = &out[0] else { panic!("{out:?}") };
        assert_eq!(list.start, 2, "{list:?}");
        assert_eq!(block_text(&list.items[0].blocks[0]), "two");
    }

    #[test]
    fn anchor_with_hidden_label_drops_the_whole_link() {
        // LEAK A: the label is not merely empty in the source - it was
        // emptied by dropping hidden content. Falling back to the URL as
        // link text would leak the attacker-chosen href as visible prose.
        let out = blocks(
            r#"<body><p><a href="https://evil.example/IGNORE"><span hidden="">x</span></a></p></body>"#,
        );
        assert!(out.is_empty(), "hidden-label link must not leak its URL: {out:?}");
    }

    #[test]
    fn anchor_with_hidden_label_drops_the_link_but_keeps_a_visible_sibling() {
        let out = blocks(
            r#"<body><p>
                <a href="https://evil.example/IGNORE"><span hidden="">x</span></a>
                <a href="https://good.example">kept</a>
            </p></body>"#,
        );
        let Block::Paragraph(inlines) = &out[0] else { panic!("{out:?}") };
        let links: Vec<&str> = inlines
            .iter()
            .filter_map(|i| match i {
                Inline::Link { target: LinkTarget::External(u), .. } => Some(u.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(links, vec!["https://good.example"], "{inlines:?}");
    }

    #[test]
    fn anchor_with_no_text_at_all_still_renders_the_url_as_label() {
        // Pinning today's behaviour: a link that was genuinely empty in the
        // source (nothing hidden, just no label) is unaffected - the
        // renderer is the one that falls back to the URL as its own text.
        let out = blocks(r#"<body><p><a href="https://example.com"></a></p></body>"#);
        let Block::Paragraph(inlines) = &out[0] else { panic!("{out:?}") };
        let link = inlines.iter().find(|i| matches!(i, Inline::Link { .. }));
        assert!(
            matches!(
                link,
                Some(Inline::Link { content, target: LinkTarget::External(u) })
                    if content.is_empty() && u == "https://example.com"
            ),
            "{inlines:?}"
        );
    }

    #[test]
    fn anchor_wrapping_a_list_with_a_hidden_item_drops_the_link() {
        // LEAK A follow-up: `<li>` is filtered by `element_props(..).hidden`
        // directly in `parse_list`, not through `walk_elem`'s early return,
        // so it must record the drop itself too.
        let out = blocks(
            r#"<body><p>
                <a href="https://evil.example/X"><ul><li hidden="">x</li></ul></a>
                <a href="https://good.example">kept</a>
            </p></body>"#,
        );
        let Block::Paragraph(inlines) = &out[0] else { panic!("{out:?}") };
        let links: Vec<&str> = inlines
            .iter()
            .filter_map(|i| match i {
                Inline::Link { target: LinkTarget::External(u), .. } => Some(u.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(links, vec!["https://good.example"], "{inlines:?}");
    }

    #[test]
    fn anchor_wrapping_a_table_with_a_hidden_row_drops_the_link() {
        // Same leak, via the table row/cell path in `parse_table`.
        let out = blocks(
            r#"<body><p><a href="https://evil.example/X"><table><tr hidden=""><td>x</td></tr></table></a></p></body>"#,
        );
        assert!(out.is_empty(), "hidden-table-row label must not leak its URL: {out:?}");
    }

    #[test]
    fn anchor_wrapping_pre_with_hidden_span_drops_the_link() {
        // Same leak, via `collect_visible_text`'s own hidden re-check.
        let out = blocks(
            r#"<body><p><a href="https://evil.example/X"><pre><span hidden="">x</span></pre></a></p></body>"#,
        );
        assert!(out.is_empty(), "hidden-pre-content label must not leak its URL: {out:?}");
    }

    #[test]
    fn id_selector_hides_the_element() {
        // LEAK B: `#byid` used to be parsed as a tag named `#byid`, which
        // never matches anything, so the rule silently did nothing.
        let css = "#byid { display: none }";
        let out = blocks_with_css(r#"<body><p id="byid">x</p><p>y</p></body>"#, css);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(para_text(&out[0]), "y");
    }

    #[test]
    fn tag_id_selector_hides_the_element() {
        let css = "p#byid { display: none }";
        let out = blocks_with_css(r#"<body><p id="byid">x</p><p>y</p></body>"#, css);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(para_text(&out[0]), "y");

        // A tag mismatch on the same id must not match.
        let out = blocks_with_css(r#"<body><div id="byid">x</div></body>"#, css);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(para_text(&out[0]), "x");
    }

    #[test]
    fn hidden_attribute_selector_hides_the_element() {
        let css = "[hidden] { display: none }";
        let out = blocks_with_css(r#"<body><p hidden="">x</p><p>y</p></body>"#, css);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(para_text(&out[0]), "y");
    }

    #[test]
    fn aria_hidden_true_attribute_selector_hides_the_element() {
        for selector in [r#"[aria-hidden="true"]"#, r#"[aria-hidden=true]"#] {
            let css = format!("{selector} {{ display: none }}");
            let out = blocks_with_css(r#"<body><p aria-hidden="true">x</p><p>y</p></body>"#, &css);
            assert_eq!(out.len(), 1, "{selector}: {out:?}");
            assert_eq!(para_text(&out[0]), "y", "{selector}");
        }
    }

    #[test]
    fn attribute_selectors_parse_into_exactly_the_two_supported_rules() {
        // `hidden_attribute_selector_hides_the_element` and
        // `aria_hidden_true_attribute_selector_hides_the_element` above only
        // exercise elements that the `hidden`/`aria-hidden="true"`
        // attributes already force-hide outright, regardless of CSS - so
        // they'd still pass even if `AttrSelector` parsing were deleted
        // entirely. Pin the parsing itself instead: exactly the two
        // supported attribute selectors produce a rule, and a lookalike
        // (`[data-x]`) does not.
        for selector in ["[hidden]", r#"[aria-hidden="true"]"#, "[aria-hidden=true]"] {
            let mut sheet = Stylesheet::default();
            sheet.add(&format!("{selector} {{ display: none }}"));
            assert_eq!(sheet.rule_count(), 1, "{selector} should parse to one rule");
        }
        let mut sheet = Stylesheet::default();
        sheet.add("[data-x] { display: none }");
        assert_eq!(sheet.rule_count(), 0, "[data-x] is outside the supported subset");
    }

    #[test]
    fn descendant_combinator_selector_is_skipped_not_matched() {
        // Documents a known limitation: combinator selectors are out of the
        // supported subset, so the element stays visible rather than being
        // (mis)matched some other way.
        let css = "div.wrap p { display: none }";
        let out = blocks_with_css(r#"<body><div class="wrap"><p>x</p></div></body>"#, css);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(para_text(&out[0]), "x");
    }

    #[test]
    fn at_rule_wrapped_selector_is_skipped_not_matched() {
        // Documents a known limitation: `Stylesheet::add` has no notion of
        // nesting, so `@media`/`@supports`/`@import` (and any other nested
        // block) is skipped whole - the wrapped rule's declarations never
        // take effect, same as an unsupported selector.
        let css = "@media screen { #x { display: none } }";
        let out = blocks_with_css(r#"<body><p id="x">x</p></body>"#, css);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(para_text(&out[0]), "x");
    }

    #[test]
    fn general_attribute_selector_is_skipped_not_matched() {
        // `[data-x]` (and anything beyond the two hiding attributes above)
        // is out of the supported subset.
        let css = "p[data-x] { display: none }";
        let out = blocks_with_css(r#"<body><p data-x="1">x</p></body>"#, css);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(para_text(&out[0]), "x");
    }

    #[test]
    fn class_selector_still_works_alongside_id_and_attribute_selectors() {
        let css = ".gone { display: none }";
        let out = blocks_with_css(r#"<body><p class="gone">x</p><p>y</p></body>"#, css);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(para_text(&out[0]), "y");
    }

    #[test]
    fn selector_list_handles_each_part_independently() {
        let css = "#a, .b, [hidden] { display: none }";
        let out = blocks_with_css(
            r#"<body><p id="a">1</p><p class="b">2</p><p hidden="">3</p><p>4</p></body>"#,
            css,
        );
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(para_text(&out[0]), "4");
    }
}
