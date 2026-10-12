//! Paste Special: paste the clipboard in a chosen format (formatted, RTF, HTML, unformatted).
//!
//! The clipboard can hold several representations of the same content. WordCraft's own copy keeps
//! a rich fragment; the system clipboard gives text, and callers (agents, the CLI, a front end
//! with a richer clipboard) may pass `html` or `rtf` too. Text that is itself RTF or HTML source
//! can also be pasted as that format.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};
use wordcraft_doc::edit::Fragment;
use wordcraft_doc::{Block, Document, Paragraph};

use super::{delete_selection, sel_result};
use crate::{CmdError, CmdResult, ColumnBlock, CommandSpec, Selection, Session, p};

/// The most recent paste, retained independently of subsequent clipboard changes.
#[derive(Clone, Debug)]
pub struct PasteState {
    pub mode: &'static str,
    pub rich: bool,
    source: Arc<PasteSource>,
    text: Option<String>,
    pub(crate) before_doc: Document,
    pub(crate) before_sel: Selection,
    pub(crate) before_pending: Option<wordcraft_doc::props::CharProps>,
    pub(crate) before_column: Option<ColumnBlock>,
    before_move: Option<super::moves::PendingMove>,
    pub(crate) rev: u64,
    pub(crate) document_id: u64,
    pub(crate) final_sel: Selection,
}

#[derive(Clone, Debug)]
enum PasteSource {
    Internal(Fragment),
    Imported(Box<Document>),
    Text(String),
}

/// Reject oversized/deep internal copies before cloning or flattening them.
fn check_fragment(f: &Fragment) -> Result<(), CmdError> {
    fn block(b: &Block, depth: usize, bytes: &mut usize, nodes: &mut usize) -> Result<(), CmdError> {
        *nodes = nodes.saturating_add(1);
        if depth > 16 || *nodes > 100_000 {
            return Err(CmdError::Params("clipboard structure exceeds the paste limit".into()));
        }
        match b {
            Block::Para(p) => {
                *bytes = bytes
                    .saturating_add(p.text.len())
                    .saturating_add(p.runs.len().saturating_mul(256))
                    .saturating_add(p.objects.len().saturating_mul(256));
                if *bytes > MAX_PASTE_BYTES {
                    return Err(CmdError::Params("clipboard content exceeds the paste limit".into()));
                }
            }
            Block::Table(t) => {
                for cell in t.rows.iter().flat_map(|r| &r.cells) {
                    for b in &cell.blocks {
                        block(b, depth + 1, bytes, nodes)?;
                    }
                }
            }
        }
        Ok(())
    }
    let (mut bytes, mut nodes) = (0, 0);
    for b in &f.blocks {
        block(b, 0, &mut bytes, &mut nodes)?;
    }
    for part in f.parts.values() {
        for b in &part.blocks {
            block(b, 0, &mut bytes, &mut nodes)?;
        }
    }
    Ok(())
}

fn normal_source(s: &Session, v: &Value) -> Result<(PasteSource, bool), CmdError> {
    for k in ["text", "html", "rtf"] {
        if p::str(v, k).is_some_and(|t| t.len() > MAX_PASTE_BYTES) {
            return Err(CmdError::Params(format!("`{k}` is larger than {} MB", MAX_PASTE_BYTES >> 20)));
        }
    }
    if let Some(f) = &s.clipboard {
        let check = check_fragment(f);
        if p::str(v, "text").is_none() && p::str(v, "html").is_none() && p::str(v, "rtf").is_none() {
            check?;
            return Ok((PasteSource::Internal(f.clone()), false));
        }
        let own = match p::str(v, "text") {
            Some(t) if check.is_ok() => f.plain_text() == t.replace("\r\n", "\n"),
            _ => false,
        };
        if own {
            return Ok((PasteSource::Internal(f.clone()), false));
        }
    }
    let mut fallback = false;
    if let Some(html) = p::str(v, "html") {
        let doc = wordcraft_formats::html::import(html);
        if !doc.plain_text(wordcraft_doc::StoryRef::Body).is_empty() {
            return Ok((PasteSource::Imported(Box::new(doc)), false));
        }
        fallback = true;
    }
    if let Some(rtf) = p::str(v, "rtf") {
        if let Ok(doc) = wordcraft_formats::rtf::import(rtf.as_bytes())
            && !doc.plain_text(wordcraft_doc::StoryRef::Body).is_empty()
        {
            return Ok((PasteSource::Imported(Box::new(doc)), fallback));
        }
        fallback = true;
    }
    if let Some(t) = p::str(v, "text") {
        return Ok((PasteSource::Text(t.replace("\r\n", "\n")), fallback));
    }
    Err(CmdError::Failed(if fallback { "the rich clipboard content could not be read" } else { "the clipboard is empty" }.into()))
}

/// Normal source, merge and text paste all retain the same original source.
pub fn normal(s: &mut Session, v: &Value, mode: &'static str) -> CmdResult {
    let (source, fallback) = normal_source(s, v)?;
    let result = save_source(s, source, p::str(v, "text").map(|t| t.replace("\r\n", "\n")), mode, v)?;
    if fallback {
        s.status = "Rich clipboard content could not be read; pasted an available fallback.".into();
    }
    Ok(result)
}

fn save_source(s: &mut Session, source: PasteSource, text: Option<String>, mode: &'static str, v: &Value) -> CmdResult {
    let image_only = match &source {
        PasteSource::Internal(f) => f.parts.is_empty() && images_only(f.blocks.iter(), &s.doc.styles),
        PasteSource::Imported(d) => d.parts.is_empty() && images_only(d.body.iter().map(AsRef::as_ref), &d.styles),
        PasteSource::Text(_) => false,
    };
    if image_only {
        s.dismiss_paste();
        if mode == "text" {
            let text = text.unwrap_or_else(|| match &source {
                PasteSource::Internal(f) => f.plain_text(),
                PasteSource::Imported(d) => d.plain_text(wordcraft_doc::StoryRef::Body),
                PasteSource::Text(t) => t.clone(),
            });
            super::paste_text(s, &text.replace(wordcraft_doc::para::OBJ, "").replace("\r\n", "\n").replace('\n', "\r"))?;
        } else {
            let frag = match source {
                PasteSource::Internal(f) => f,
                PasteSource::Imported(d) => adopt(s, *d),
                PasteSource::Text(t) => Fragment::from_text(&t),
            };
            insert(s, frag)?;
        }
        return sel_result(s);
    }
    let rich = !matches!(source, PasteSource::Text(_));
    s.last_paste = Some(PasteState {
        mode: if rich { mode } else { "text" },
        rich,
        source: Arc::new(source),
        text: text.map(|t| t.replace("\r\n", "\n")),
        before_doc: s.doc.clone(),
        before_sel: s.sel.clone(),
        before_pending: s.pending.clone(),
        before_column: s.column.clone(),
        before_move: s.pending_move.clone(),
        rev: s.rev(),
        document_id: s.document_id(),
        final_sel: s.sel.clone(),
    });
    insert_saved(s, v)
}

fn images_only<'a>(blocks: impl Iterator<Item = &'a Block>, styles: &wordcraft_doc::styles::StyleSheet) -> bool {
    let mut image = false;
    for block in blocks {
        let Block::Para(p) = block else { return false };
        if p.text.chars().any(|c| c != '\u{fffc}' && !c.is_whitespace())
            || styles.resolve_para(&p.props).numbering.is_some_and(|n| n.num != 0)
            || p.objects.iter().any(|o| !matches!(o, wordcraft_doc::InlineObject::Image { ole: None, .. }))
        {
            return false;
        }
        image |= !p.objects.is_empty();
    }
    image
}

fn insert_saved(s: &mut Session, _: &Value) -> CmdResult {
    let state = s.last_paste.clone().ok_or_else(|| CmdError::Failed("there is no recent paste".into()))?;
    if state.mode == "text" {
        let text = state.text.clone().unwrap_or_else(|| match state.source.as_ref() {
            PasteSource::Internal(f) => f.plain_text(),
            PasteSource::Imported(d) => d.plain_text(wordcraft_doc::StoryRef::Body),
            PasteSource::Text(t) => t.clone(),
        });
        s.pending_move = None;
        super::paste_text(s, &text.replace(wordcraft_doc::para::OBJ, "").replace('\n', "\r"))?;
    } else {
        let styles = match state.source.as_ref() {
            PasteSource::Imported(d) => &d.styles,
            _ => &state.before_doc.styles,
        };
        let mut frag = match state.source.as_ref() {
            PasteSource::Internal(f) => f.clone(),
            PasteSource::Imported(d) => adopt(s, d.as_ref().clone()),
            PasteSource::Text(t) => Fragment::from_text(t),
        };
        let at = s.sel.ordered().0;
        let para = s.doc.para_at(&at).map(|p| p.props.clone()).unwrap_or_default();
        let dest = s.doc.styles.resolve_char(para.style.as_deref(), &s.typing_props());
        let mut format = |p: &mut Paragraph| {
            for c in p.runs.iter_mut().map(|r| &mut r.props).chain(std::iter::once(&mut p.mark)) {
                let source = styles.resolve_char(p.props.style.as_deref(), c);
                let target = if state.mode == "merge" { &dest } else { &source };
                c.font = Some(target.font.clone());
                c.font_cs = Some(target.font_cs.clone());
                c.font_ea = Some(target.font_ea.clone().unwrap_or_else(|| target.font.clone()));
                c.size = Some(target.size);
                c.size_cs = Some(target.size_cs);
                c.color = Some(target.color);
                c.bold = Some(source.bold);
                c.italic = Some(source.italic);
                c.bold_cs = Some(source.bold_cs);
                c.italic_cs = Some(source.italic_cs);
                // Explicit defaults prevent a same-named destination style leaking into the copy.
                c.underline = Some(source.underline);
                c.underline_color = source.underline_color;
                c.strike = Some(source.strike);
                c.double_strike = Some(source.double_strike);
                c.vert_align = Some(source.vert_align);
                c.caps = Some(source.caps);
                c.small_caps = Some(source.small_caps);
                c.hidden = Some(source.hidden);
                c.spacing = Some(source.spacing);
                c.scale = Some(source.scale);
                c.position = Some(source.position);
                c.outline = Some(source.outline);
                c.shadow = Some(source.shadow);
                c.emboss = Some(source.emboss);
                c.engrave = Some(source.engrave);
                c.no_proof = Some(source.no_proof);
                c.rtl = Some(source.rtl);
                c.cs = Some(source.cs);
                c.lang = source.lang;
                c.shading = source.shading;
                c.border = Some(source.border.unwrap_or_default());
                c.text_effects = source.text_effects;
                c.highlight = wordcraft_doc::props::Highlight::ALL.into_iter().find(|h| h.rgb() == source.highlight);
            }
            if state.mode == "merge" {
                let numbering = p.props.numbering;
                p.props = para.clone();
                if numbering.is_some() {
                    p.props.numbering = numbering;
                }
            } else {
                let source = styles.resolve_para(&p.props);
                p.props.align = Some(source.align);
                p.props.indent_left = Some(source.indent_left);
                p.props.indent_right = Some(source.indent_right);
                p.props.indent_first = Some(source.indent_first);
                let [left, right, first] = source.indent_chars;
                p.props.indent_left_chars = Some(left);
                p.props.indent_right_chars = Some(right);
                p.props.indent_first_chars = Some(first);
                p.props.space_before = Some(source.space_before);
                p.props.space_after = Some(source.space_after);
                p.props.line_spacing = Some(source.line_spacing);
                p.props.contextual_spacing = Some(source.contextual_spacing);
                p.props.mirror_indents = Some(source.mirror_indents);
                p.props.keep_next = Some(source.keep_next);
                p.props.keep_lines = Some(source.keep_lines);
                p.props.page_break_before = Some(source.page_break_before);
                p.props.widow_control = Some(source.widow_control);
                p.props.outline_level = Some(source.outline_level.unwrap_or(9));
                p.props.numbering = Some(source.numbering.unwrap_or(wordcraft_doc::props::NumRef { num: 0, level: 0 }));
                p.props.tabs = Some(source.tabs);
                p.props.shading = source.shading;
                p.props.borders = Some(source.borders.unwrap_or_default());
                p.props.bidi = Some(source.bidi);
                p.props.drop_cap = Some(source.drop_cap);
                p.props.suppress_hyphens = Some(source.suppress_hyphens);
                p.props.suppress_line_numbers = Some(source.suppress_line_numbers);
                p.props.kinsoku = Some(source.kinsoku);
                p.props.word_wrap = Some(source.word_wrap);
                p.props.overflow_punct = Some(source.overflow_punct);
                p.props.top_line_punct = Some(source.top_line_punct);
                p.props.auto_space_de = Some(source.auto_space_de);
                p.props.auto_space_dn = Some(source.auto_space_dn);
                p.props.snap_to_grid = Some(source.snap_to_grid);
            }
            p.touch();
        };
        for b in &mut frag.blocks {
            each_para(b, 0, &mut format);
        }
        for part in frag.parts.values_mut() {
            for b in &mut part.blocks {
                each_para(Arc::make_mut(b), 0, &mut format);
            }
        }
        // A whole empty destination paragraph can keep the source paragraph style/list.
        // Inline pastes retain the surrounding paragraph, as insert_fragment does.
        super::controls::prepare_edit(s)?;
        let at = delete_selection(s)?;
        if let Some(Block::Para(first)) = frag.blocks.first()
            && s.doc.para_at(&at).is_some_and(|p| p.is_empty())
        {
            s.doc.para_mut(at.story, &at.path)?.props = first.props.clone();
        }
        insert(s, frag)?;
        s.pending = None;
    }
    let mut result = sel_result(s)?;
    result["mode"] = json!(state.mode);
    result["rich"] = json!(state.rich);
    Ok(result)
}

pub fn options(s: &mut Session, v: &Value) -> CmdResult {
    let mode = match p::str(v, "mode") {
        Some("source") => "source",
        Some("merge") => "merge",
        Some("text") => "text",
        _ => return Err(CmdError::Params("`mode` must be source, merge or text".into())),
    };
    if !s.paste_options_valid() {
        return Err(CmdError::Failed("there is no recent paste".into()));
    }
    let mut state = s.last_paste.clone().ok_or_else(|| CmdError::Failed("there is no recent paste".into()))?;
    state.mode = if state.rich { mode } else { "text" };
    s.doc = state.before_doc.clone();
    s.sel = state.before_sel.clone();
    s.pending = state.before_pending.clone();
    s.column = state.before_column.clone();
    s.pending_move = state.before_move.clone();
    s.last_paste = Some(state);
    super::column::dispatch(s, "edit.paste", true, insert_saved, v)
}

/// Largest clipboard payload Paste Special accepts, in bytes.
pub const MAX_PASTE_BYTES: usize = 16 << 20;
/// Nesting depth we follow when adjusting pasted tables.
const MAX_DEPTH: usize = 64;

/// The formats, in the order the dialog lists them: (id, label).
pub const FORMATS: [(&str, &str); 4] =
    [("formatted", "Formatted Text"), ("rtf", "Rich Text Format (RTF)"), ("html", "HTML Format"), ("text", "Unformatted Text")];

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec::new("edit.pasteSpecial", "Paste Special", "Home › Clipboard › Paste", paste_special)
            .key("Mod+Alt+V")
            .params(r#"{"as"?: "formatted"|"rtf"|"html"|"text", "text"?: string, "html"?: string, "rtf"?: string}"#),
    ]
}

/// Clipboard text from the params (system clipboard), else WordCraft's own copy.
fn clip_text(s: &Session, v: &Value) -> Option<String> {
    match p::str(v, "text") {
        Some(t) => Some(t.replace("\r\n", "\n")),
        None => s.clipboard.as_ref().filter(|f| check_fragment(f).is_ok()).map(Fragment::plain_text),
    }
}

fn looks_like_rtf(t: &str) -> bool {
    t.trim_start().starts_with("{\\rtf")
}

fn looks_like_html(t: &str) -> bool {
    let t = t.trim_start();
    t.starts_with('<') && (t.contains("</") || t.contains("/>"))
}

/// The source for a format, if the clipboard has it.
fn source(s: &Session, v: &Value, fmt: &str) -> Option<String> {
    let non_empty = |t: String| if t.is_empty() { None } else { Some(t) };
    match fmt {
        "formatted" => {
            let f = s.clipboard.as_ref()?;
            check_fragment(f).ok()?;
            // System clipboard text that isn't our own copy means someone else copied since.
            match p::str(v, "text") {
                Some(t) if f.plain_text() != t.replace("\r\n", "\n") => None,
                _ => Some(String::new()),
            }
        }
        "rtf" => p::str(v, "rtf").map(str::to_string).or_else(|| clip_text(s, v).filter(|t| looks_like_rtf(t))).and_then(non_empty),
        "html" => p::str(v, "html").map(str::to_string).or_else(|| clip_text(s, v).filter(|t| looks_like_html(t))).and_then(non_empty),
        "text" => clip_text(s, v).and_then(non_empty),
        _ => None,
    }
}

/// Formats the clipboard can be pasted as, best first.
pub fn available(s: &Session, v: &Value) -> Vec<&'static str> {
    FORMATS.iter().map(|(id, _)| *id).filter(|id| source(s, v, id).is_some()).collect()
}

fn formats_json(ids: &[&str]) -> Value {
    json!(ids.iter().map(|id| json!({"id": id, "label": FORMATS.iter().find(|(f, _)| f == id).map(|(_, l)| *l).unwrap_or(*id)})).collect::<Vec<_>>())
}

fn paste_special(s: &mut Session, v: &Value) -> CmdResult {
    for k in ["text", "html", "rtf"] {
        if p::str(v, k).is_some_and(|t| t.len() > MAX_PASTE_BYTES) {
            return Err(CmdError::Params(format!("`{k}` is larger than {} MB", MAX_PASTE_BYTES >> 20)));
        }
    }
    let ids = available(s, v);
    let Some(fmt) = p::str(v, "as") else {
        // No format chosen: list them and ask a front end (if any) to show the dialog.
        let mut req = json!({"open": "pasteSpecial", "formats": ids});
        for k in ["text", "html", "rtf"] {
            if let Some(t) = p::str(v, k) {
                req[k] = json!(t);
            }
        }
        s.ui_requests.push(req);
        return Ok(json!({"formats": formats_json(&ids)}));
    };
    let fmt = match fmt.to_ascii_lowercase().as_str() {
        "formatted" | "keep" | "wordcraft" => "formatted",
        "rtf" => "rtf",
        "html" => "html",
        "text" | "unformatted" | "plain" => "text",
        other => return Err(CmdError::Params(format!("unknown format `{other}` (formatted, rtf, html or text)"))),
    };
    let Some(src) = source(s, v, fmt) else {
        return Err(CmdError::Failed(if ids.is_empty() {
            "the clipboard is empty".into()
        } else {
            format!("the clipboard has no {fmt} content (available: {})", ids.join(", "))
        }));
    };
    let mut r = match fmt {
        "text" => normal(s, v, "text")?,
        "formatted" => normal(s, &json!({}), "source")?,
        "html" => save_source(
            s,
            PasteSource::Imported(Box::new(wordcraft_formats::html::import(&src))),
            p::str(v, "text").map(str::to_string),
            "source",
            v,
        )?,
        _ => save_source(
            s,
            PasteSource::Imported(Box::new(wordcraft_formats::rtf::import(src.as_bytes()).map_err(|e| CmdError::Failed(format!("bad RTF: {e}")))?)),
            p::str(v, "text").map(str::to_string),
            "source",
            v,
        )?,
    };
    r["pastedAs"] = json!(fmt);
    Ok(r)
}

/// Replace the selection with `frag`, marking it as an insertion under Track Changes.
pub fn insert(s: &mut Session, mut frag: Fragment) -> Result<(), CmdError> {
    super::controls::prepare_edit(s)?;
    let at = delete_selection(s)?;
    let track = s.doc.settings.track_changes;
    let mut rid = None;
    if track {
        // What the last command cut, pasted again: the two ends of a move.
        let r = match super::moves::take_paste(s, &frag) {
            Some(r) => r,
            None => super::new_revision(s, wordcraft_doc::RevisionKind::Insert),
        };
        rid = Some(r);
        for b in &mut frag.blocks {
            each_para(b, 0, &mut |p| {
                for run in &mut p.runs {
                    run.props.ins = Some(r);
                    run.props.del = None;
                    run.props.fmt_change = None;
                }
                p.mark.ins = None;
                p.mark.del = None;
            });
        }
    }
    let end = s.doc.insert_fragment(&at, &frag)?;
    // The paragraph marks the paste added are inserted too, so rejecting it joins the
    // paragraphs again. The last paragraph ends with the original mark and keeps it.
    if let Some(r) = rid
        && at.story == end.story
        && at.path.parent() == end.path.parent()
    {
        for i in at.path.last()..end.path.last() {
            if let Ok(p) = s.doc.para_mut(at.story, &at.path.with_last(i)) {
                p.mark.ins = Some(r);
                p.mark.del = None;
                p.touch();
            }
        }
    }
    s.sel = Selection::caret(end);
    Ok(())
}

/// Every paragraph in a block, tables included (bounded depth).
fn each_para(b: &mut Block, depth: usize, f: &mut dyn FnMut(&mut Paragraph)) {
    if depth > MAX_DEPTH {
        return;
    }
    match b {
        Block::Para(p) => f(p),
        Block::Table(t) => {
            for row in &mut t.rows {
                for cell in &mut row.cells {
                    for cb in &mut cell.blocks {
                        each_para(Arc::make_mut(cb), depth + 1, f);
                    }
                }
            }
        }
    }
}

/// Turn an imported document into a fragment of the session's document: bring over the media,
/// lists and styles it refers to (renumbered so they can't collide with ours).
fn adopt(s: &mut Session, src: Document) -> Fragment {
    let mut media = BTreeMap::new();
    for (key, data) in &src.media {
        let ext = key.rsplit_once('.').map(|(_, e)| e).unwrap_or("png");
        media.insert(key.clone(), s.doc.add_media(data.to_vec(), ext));
    }
    let next_abs = s.doc.numbering.abstracts.iter().map(|a| a.id.saturating_add(1)).max().unwrap_or(0);
    let mut abs_map = BTreeMap::new();
    for (i, a) in src.numbering.abstracts.iter().enumerate() {
        let id = next_abs.saturating_add(i as u32);
        abs_map.insert(a.id, id);
        s.doc.numbering.abstracts.push(wordcraft_doc::numbering::AbstractNum { id, ..a.clone() });
    }
    let next_num = s.doc.numbering.nums.iter().map(|n| n.id.saturating_add(1)).max().unwrap_or(1).max(1);
    let mut num_map = BTreeMap::new();
    for (i, n) in src.numbering.nums.iter().enumerate() {
        let id = next_num.saturating_add(i as u32);
        num_map.insert(n.id, id);
        let abstract_id = abs_map.get(&n.abstract_id).copied().unwrap_or(n.abstract_id);
        s.doc.numbering.nums.push(wordcraft_doc::numbering::Num { id, abstract_id, ..n.clone() });
    }
    for st in &src.styles.styles {
        if s.doc.styles.get(&st.id).is_none() {
            s.doc.styles.upsert(st.clone());
        }
    }
    let mut blocks: Vec<Block> = src.body.iter().map(|b| (**b).clone()).collect();
    for b in &mut blocks {
        each_para(b, 0, &mut |p| {
            if let Some(n) = p.props.numbering.as_mut()
                && n.num != 0
            {
                n.num = num_map.get(&n.num).copied().unwrap_or(0);
            }
            for o in &mut p.objects {
                if let wordcraft_doc::InlineObject::Image { media: key, .. } = o
                    && let Some(k) = media.get(key.as_str())
                {
                    *key = k.clone();
                }
            }
        });
    }
    // Text box stories the imported shapes show; pasting gives each a new story of its own.
    let parts = src.parts.into_iter().filter(|(_, p)| p.kind == wordcraft_doc::PartKind::TextBox).collect();
    Fragment { blocks, parts }
}
