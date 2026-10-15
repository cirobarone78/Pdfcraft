//! Verification tooling: audit a document for faux redactions.
//!
//! [`audit_redactions`] finds *faux redactions* — opaque filled rectangles drawn over text
//! that is still extractable underneath — and redaction annotations that were marked but
//! never applied. A properly applied redaction removes the text, so it never flags.
//!
//! A rectangle counts as a cover only when it was painted strictly *after* the text it
//! overlaps, and only when it is dark enough against the text's color to make the text
//! unreadable (WCAG contrast below 3.0). Page backgrounds, table shading and highlight
//! marks are therefore not flagged. A white (or nearly white) opaque rectangle painted after
//! dark text hides it just as well, the way forms are "whited out"; that is reported as its
//! own kind, [`FindingKind::HiddenUnderWhite`], so callers can decide (#2385).
//!
//! Covers come from the page content — `re` rectangles and single closed `m`/`l` subpaths whose
//! four corners form an axis-aligned rectangle in user space (#1510), filled opaquely, and a
//! thick opaque stroke along one axis (a "marker" line) — and from the annotations: an opaque
//! `/Highlight` dark enough to hide the text under it (#2385).
//!
//! Detection limits (documented, not hidden): other filled paths, slanted or bent strokes,
//! shadings and patterns are not considered; a cover drawn with a translucent ExtGState
//! (`/ca`, `/CA < 1`) or a highlight with `/CA < 1` is not flagged; annotation rectangles are
//! compared in unrotated user space, so pages with `/Rotate` may misalign; covers inside form
//! XObjects are found, but only text drawn directly in the page content is checked under them,
//! so text inside a form XObject is not audited.
//!
//! The audit fails closed: any content that cannot be read (an undecodable content stream,
//! malformed annotations, form XObjects nested deeper than 32 or in a cycle) is an error, never
//! an empty finding list.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use pdfcraft_cos::{Dict, Document};
use pdfcraft_edit::TextLine;

mod cover;

#[cfg(test)]
mod tests;

/// What the audit found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FindingKind {
    /// An opaque dark cover (a filled rectangle, a thick stroke, a highlight annotation) hides
    /// text that is still extractable.
    CoveredText,
    /// A `/Redact` annotation was placed but never applied: the text is untouched.
    UnappliedMark,
    /// A white (or nearly white) opaque rectangle painted after dark text hides it: the text is
    /// invisible on the page and still extractable (a "whited out" form).
    HiddenUnderWhite,
}

impl FindingKind {
    /// The stable machine-readable ID for this finding class.
    ///
    /// These strings are the crate's public vocabulary: they appear in CLI output and are
    /// the IDs a future machine-readable format would serialize. Do not rename them;
    /// add a new variant instead.
    pub fn id(self) -> &'static str {
        match self {
            FindingKind::CoveredText => "covered-text",
            FindingKind::UnappliedMark => "unapplied-mark",
            FindingKind::HiddenUnderWhite => "hidden-under-white",
        }
    }
}

/// One audit finding.
#[derive(Clone, Debug, PartialEq)]
pub struct AuditFinding {
    /// 0-based page index.
    pub page: usize,
    pub kind: FindingKind,
    /// The covering rectangle (or the mark's rectangle), in page user space, normalized so
    /// `rect[0] <= rect[2]` and `rect[1] <= rect[3]`.
    pub rect: [f64; 4],
    /// The extractable text the cover hides, lines joined with single spaces and trimmed.
    /// Possibly empty when nothing decodable remains under the cover.
    pub covered_text: String,
}

#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error("page {0}: {1}")]
    Text(usize, String),
    #[error("content walk exceeded the operator bound")]
    TooManyOps,
    #[error("form XObjects nest deeper than the audit follows")]
    TooDeep,
    #[error("page has a malformed /Annots entry")]
    BadAnnots,
    #[error(transparent)]
    Cos(#[from] pdfcraft_cos::CosError),
    #[error(transparent)]
    Edit(#[from] pdfcraft_edit::EditError),
}

/// Fraction of a text line's ink (the area of its words), or of its box, that must lie under a
/// cover to count as hidden (see [`covers`]).
const COVER_FRACTION: f64 = 0.4;
/// WCAG contrast ratio below which the text is unreadable against the cover.
const COVER_CONTRAST: f64 = 3.0;
/// Relative luminance from which an opaque cover counts as white (white is 1.0; `0.9 g`, the
/// kind of shading a table has, is 0.79): dark text under it is gone, not shaded.
const WHITE_LUMINANCE: f64 = 0.9;

/// Audit every page for faux redactions and unapplied redaction marks. Fails closed: any
/// content that cannot be decoded is returned as an error, never as an empty finding list.
pub fn audit_redactions(doc: &Document) -> Result<Vec<AuditFinding>, AuditError> {
    let mut out = Vec::new();
    for (n, page) in pdfcraft_model::pages(doc).iter().enumerate() {
        let lines = pdfcraft_edit::text_lines(doc, n).map_err(|e| AuditError::Text(n, e.to_string()))?;
        // 1. Opaque rectangles (filled, or painted by a thick stroke) over extractable text.
        for r in cover::filled_rects(doc, &page.dict)? {
            for kind in [FindingKind::CoveredText, FindingKind::HiddenUnderWhite] {
                let covered = joined(lines.iter().filter(|line| cover_kind(&r, line) == Some(kind.clone())));
                if !covered.is_empty() {
                    out.push(AuditFinding { page: n, kind, rect: r.rect, covered_text: covered });
                }
            }
        }
        // 2. Redact annotations that were never applied, and highlights dark enough to hide the
        // text under them (annotations are drawn over the page's content).
        for mark in annots(doc, &page.dict)? {
            let (kind, hides) = match mark.kind {
                Mark::Redact => (FindingKind::UnappliedMark, None),
                Mark::Highlight(rgb) => (FindingKind::CoveredText, Some(rgb)),
            };
            let covered =
                joined(lines.iter().filter(|line| covers(&mark.rect, line) && hides.is_none_or(|rgb| contrast(line.color, rgb) < COVER_CONTRAST)));
            if !covered.is_empty() {
                out.push(AuditFinding { page: n, kind, rect: mark.rect, covered_text: covered });
            }
        }
    }
    Ok(out)
}

/// A filled rectangle is a faux-redaction cover for a text line iff it was painted strictly
/// after the line, overlaps it substantially, and is dark enough against the line's color to
/// make the text unreadable. Light covers (table shading, highlights) over dark text fail the
/// contrast check and are not flagged.
fn is_cover(rect: &cover::FilledRect, line: &TextLine) -> bool {
    rect.pos > line.paint_pos() && covers(&rect.rect, line) && contrast(line.color, rect.rgb) < COVER_CONTRAST
}

/// The text of `lines`, trimmed and joined by spaces (empty when nothing is left).
fn joined<'a>(lines: impl Iterator<Item = &'a TextLine>) -> String {
    let mut covered = String::new();
    for line in lines {
        if !covered.is_empty() {
            covered.push(' ');
        }
        covered.push_str(line.text.trim());
    }
    covered.trim().to_string()
}

/// How a painted rectangle hides a text line, if it does: dark enough to be a cover
/// ([`is_cover`]), or white or nearly so over dark text, which is gone under it. Light covers
/// in between (table shading, highlights) are not flagged.
fn cover_kind(rect: &cover::FilledRect, line: &TextLine) -> Option<FindingKind> {
    if is_cover(rect, line) {
        Some(FindingKind::CoveredText)
    } else if rect.pos > line.paint_pos() && covers(&rect.rect, line) && luminance(rect.rgb) >= WHITE_LUMINANCE {
        Some(FindingKind::HiddenUnderWhite)
    } else {
        None
    }
}

/// Does `cover` hide a substantial part of what `line` draws? Two ways, either is enough:
///
/// - 40% of the area of the line's words (its `ink`) lies under the cover. Measured on the words,
///   not the box: an applied redaction leaves a gap that draws nothing (a `TJ` adjustment where
///   the text was), and the box still spans it, so a redaction box at the end of a line used to
///   cover most of the box and flag the words left beside it.
/// - 40% of the line's box lies under the cover (the measure used before `ink`) and intersects the ink
///   of a word still drawn. This keeps a cover over a short value after a long label
///   flagged, while a cover over the gap alone, with no word under it, stops counting.
///
/// A line without word boxes is measured on its box alone.
fn covers(cover: &[f64; 4], line: &TextLine) -> bool {
    let hidden = |b: &[f64; 4]| area(&[cover[0].max(b[0]), cover[1].max(b[1]), cover[2].min(b[2]), cover[3].min(b[3])]);
    let fraction = |boxes: &[[f64; 4]]| {
        let total: f64 = boxes.iter().map(area).sum();
        if total > 0.0 { boxes.iter().map(hidden).sum::<f64>() / total } else { 0.0 }
    };
    let inked = fraction(&line.ink) >= COVER_FRACTION;
    let boxed = fraction(std::slice::from_ref(&line.rect)) >= COVER_FRACTION;
    let word_intersects = || line.ink.iter().any(|w| hidden(w) > 0.0);
    inked || (boxed && (line.ink.is_empty() || word_intersects()))
}

/// The area of a rectangle, 0 when it is empty or inverted.
fn area(r: &[f64; 4]) -> f64 {
    (r[2] - r[0]).max(0.0) * (r[3] - r[1]).max(0.0)
}

/// WCAG 2.x relative luminance of an sRGB triple.
fn luminance(rgb: [f64; 3]) -> f64 {
    let lin = rgb.map(|c| {
        let c = c.clamp(0.0, 1.0);
        if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    });
    0.2126 * lin[0] + 0.7152 * lin[1] + 0.0722 * lin[2]
}

/// WCAG contrast ratio between two sRGB colors: 1.0 (identical) to 21.0 (black on white).
fn contrast(a: [f64; 3], b: [f64; 3]) -> f64 {
    let (l1, l2) = (luminance(a), luminance(b));
    let (hi, lo) = if l1 > l2 { (l1, l2) } else { (l2, l1) };
    (hi + 0.05) / (lo + 0.05)
}

/// An annotation the audit looks at.
enum Mark {
    Redact,
    /// An opaque highlight, with its color.
    Highlight([f64; 3]),
}

struct Annot {
    /// `/Rect`, normalized, in unrotated user space.
    rect: [f64; 4],
    kind: Mark,
}

/// The `/Redact` annotations on a page, and the `/Highlight` ones shown opaque with a color
/// (unrotated user space). A missing `/Annots` means no marks; a malformed one fails the audit
/// rather than silently passing.
fn annots(doc: &Document, page: &Dict) -> Result<Vec<Annot>, AuditError> {
    let Some(a) = page.get(b"Annots") else { return Ok(Vec::new()) };
    let resolved = doc.resolve(a);
    let items: Vec<pdfcraft_cos::Object> = match &*resolved {
        pdfcraft_cos::Object::Array(items) => items.clone(),
        // A missing /Annots means no marks; a malformed one fails the audit.
        _ => return Err(AuditError::BadAnnots),
    };
    let mut out = Vec::new();
    for a in &items {
        let dict = match &*doc.resolve(a) {
            pdfcraft_cos::Object::Dict(d) => d.clone(),
            _ => continue,
        };
        let nums = |key: &[u8]| -> Vec<f64> {
            dict.get(key)
                .map(|r| doc.resolve(r))
                .and_then(|r| r.as_array().map(|arr| arr.iter().filter_map(|o| doc.resolve(o).as_f64()).collect()))
                .unwrap_or_default()
        };
        // Hidden or NoView: not drawn, so hiding nothing.
        let flags = dict.get(b"F").and_then(|f| doc.resolve(f).as_int()).unwrap_or(0);
        if flags & (2 | 32) != 0 {
            continue;
        }
        let kind = match dict.name(b"Subtype") {
            Some(b"Redact") => Mark::Redact,
            Some(b"Highlight") => {
                let alpha = dict.get(b"CA").and_then(|a| doc.resolve(a).as_f64()).unwrap_or(1.0);
                let rgb = match nums(b"C").as_slice() {
                    [g] => [*g, *g, *g],
                    [r, g, b] => [*r, *g, *b],
                    [c, m, y, k] => [(1.0 - c) * (1.0 - k), (1.0 - m) * (1.0 - k), (1.0 - y) * (1.0 - k)],
                    _ => continue, // no color: drawn transparent
                };
                if alpha.is_nan() || alpha < 1.0 || rgb.iter().any(|v| !v.is_finite()) {
                    continue;
                }
                Mark::Highlight(rgb.map(|v| v.clamp(0.0, 1.0)))
            }
            _ => continue,
        };
        let v = nums(b"Rect");
        if v.len() == 4 && v.iter().all(|x| x.is_finite()) {
            out.push(Annot { rect: [v[0].min(v[2]), v[1].min(v[3]), v[0].max(v[2]), v[1].max(v[3])], kind });
        }
    }
    Ok(out)
}
