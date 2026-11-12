//! Email messages (`.eml`: RFC 5322 with MIME) as PDF: a header block (From, Sent, To, Cc,
//! Subject, Attachments), the message body (the HTML version when there is one, set as
//! paragraphs with bold, italic, links, lists and quoted replies; else the plain text), the
//! pictures the HTML embeds by Content-ID, and every attachment embedded in the PDF as a
//! document-level file attachment. Nothing is fetched: a picture that lives on the web stays its
//! alternative text, and a link is text with its address shown after it.
//!
//! HTML files ([`from_html`]) go through the same body pipeline: the `<title>` as the title, the
//! pictures the file carries inline (`data:` URIs) drawn, every other picture its text.

use std::collections::{HashMap, HashSet};

use html2text::render::{RichAnnotation, RichDecorator, TaggedLine, TextDecorator};
use mail_parser::{AddressList, DateTime, Message, MessageParser, PartRole};
use pdfcraft_cos::{Dict, Document, ObjRef, Object, PdfString, Stream};
use pdfcraft_fonts::{Std14, literal, win_ansi, win_ansi_encodable};

use crate::{CreateError, ESCAPED_NOTE, MAX_SIDE, Pages, decode_text, image_xobject_with_pixels, set_title};

/// An email's page margin in points: three quarters of an inch, as a printed email has.
const MARGIN: f64 = 54.0;
/// An HTML file's page margin in points: an inch, as plain text has.
const DOCUMENT_MARGIN: f64 = 72.0;
/// The most pictures an HTML file carries inline that are embedded, and the most bytes one of
/// them may hold (base64 text).
const MAX_DATA_PICTURES: usize = 1_000;
const MAX_DATA_PICTURE: usize = 32 << 20;
/// A `<title>` is cut to this many characters.
const MAX_TITLE: usize = 500;
/// The most HTML comments cut out before the title is read.
const MAX_COMMENTS: usize = 10_000;

/// What html2text marks structure with here, instead of its `# `, `> ` and `* `: the same
/// signs behind a space. html2text never lets a paragraph keep a leading space (preformatted
/// text aside), so the space is what user text can't forge: a paragraph that happens to start
/// with `# ` stays text. The marks keep html2text's prefix width (two cells; it sizes prefixes
/// by display width and slices them by bytes, so they must be ASCII), and the renderer indents
/// a nested item or quote by that width per level.
const HEADING_MARK: char = '#';
const QUOTE_MARK: char = '>';
const BULLET_MARK: char = '*';

/// html2text's rich decorator with the structure marks above.
#[derive(Clone)]
struct Marks(RichDecorator);

impl TextDecorator for Marks {
    type Annotation = RichAnnotation;
    fn decorate_link_start(&mut self, url: &str) -> (String, Self::Annotation) {
        self.0.decorate_link_start(url)
    }
    fn decorate_link_end(&mut self) -> String {
        self.0.decorate_link_end()
    }
    fn decorate_em_start(&self) -> (String, Self::Annotation) {
        self.0.decorate_em_start()
    }
    fn decorate_em_end(&self) -> String {
        self.0.decorate_em_end()
    }
    fn decorate_strong_start(&self) -> (String, Self::Annotation) {
        self.0.decorate_strong_start()
    }
    fn decorate_strong_end(&self) -> String {
        self.0.decorate_strong_end()
    }
    fn decorate_strikeout_start(&self) -> (String, Self::Annotation) {
        self.0.decorate_strikeout_start()
    }
    fn decorate_strikeout_end(&self) -> String {
        self.0.decorate_strikeout_end()
    }
    fn decorate_code_start(&self) -> (String, Self::Annotation) {
        self.0.decorate_code_start()
    }
    fn decorate_code_end(&self) -> String {
        self.0.decorate_code_end()
    }
    fn decorate_preformat_first(&self) -> Self::Annotation {
        self.0.decorate_preformat_first()
    }
    fn decorate_preformat_cont(&self) -> Self::Annotation {
        self.0.decorate_preformat_cont()
    }
    fn decorate_image(&mut self, src: &str, title: &str) -> (String, Self::Annotation) {
        self.0.decorate_image(src, title)
    }
    fn header_prefix(&self, _level: usize) -> String {
        format!(" {HEADING_MARK}")
    }
    fn quote_prefix(&self) -> String {
        format!(" {QUOTE_MARK}")
    }
    fn unordered_item_prefix(&self) -> String {
        format!(" {BULLET_MARK}")
    }
    fn ordered_item_prefix(&self, i: i64) -> String {
        self.0.ordered_item_prefix(i)
    }
    fn make_subblock_decorator(&self) -> Self {
        Marks(self.0.make_subblock_decorator())
    }
    fn finalise(&mut self, urls: Vec<String>) -> Vec<TaggedLine<Self::Annotation>> {
        self.0.finalise(urls)
    }
}
/// The width html2text is given, in characters: wide enough that it never wraps a paragraph
/// itself (this module wraps with Helvetica's real widths). A wrap bound, not an allocation.
const HTML_WIDTH: usize = 100_000;
/// At most this many attachments are embedded; the rest are left out of the PDF.
const MAX_ATTACHMENTS: usize = 1_000;
/// Pictures are drawn no taller than this share of the page's inner height.
const MAX_IMAGE_SHARE: f64 = 0.6;
/// A link's address is shown after its text, cut to this many characters.
const MAX_LINK: usize = 200;
/// A CSS pixel in points: pictures are shown at the size a mail program shows them, 96 to the inch.
const CSS_PX: f64 = 0.75;
/// The most nodes of an HTML document that are visited for picture sizes.
const MAX_NODES: usize = 200_000;

/// A PDF of the email message `bytes` (a `.eml`, or an Outlook `.msg`; file `name`, for errors and the title when the message
/// has no subject) on pages of `page` points at `font_size` points of Helvetica with
/// three-quarter-inch margins: the header block, the body, the pictures it embeds, and its attachments embedded as
/// document-level file attachments. Characters Helvetica can't draw are written as `[U+XXXX]`
/// (never refused: a message is evidence, and it is not the reader's to edit), and a note under
/// the headers says so. Bytes that are not a message (no header at all) are refused.
pub fn from_email(name: &str, bytes: &[u8], page: (f64, f64), font_size: f64) -> Result<Document, CreateError> {
    email_with_facts(name, bytes, page, font_size).map(|(doc, _)| doc)
}

/// What an email export needs besides the PDF: when the message was sent (Unix seconds, from
/// its `Date`), its subject, and the files the PDF embeds as attachments, in order. Pictures
/// the body draws are not among them, exactly as the PDF leaves them out.
#[derive(Debug, Clone)]
pub struct EmailFacts {
    pub sent: Option<i64>,
    pub subject: Option<String>,
    pub attachments: Vec<(String, Vec<u8>)>,
    /// Attachments past the limit one PDF embeds, which are not in `attachments`.
    pub left_out: usize,
}

/// [`from_email`], with the message's [`EmailFacts`].
pub fn email_with_facts(name: &str, bytes: &[u8], page: (f64, f64), font_size: f64) -> Result<(Document, EmailFacts), CreateError> {
    let (w, h) = page;
    if !(w.is_finite() && h.is_finite() && (3.0..=MAX_SIDE).contains(&w) && (3.0..=MAX_SIDE).contains(&h)) {
        return Err(CreateError::Invalid("invalid page size".into()));
    }
    // An Outlook message is read into the same form as a `.eml` first.
    let converted = if crate::msg::is_msg(bytes) { Some(crate::msg::msg_to_eml(name, bytes)?) } else { None };
    let bytes = converted.as_deref().unwrap_or(bytes);
    let message = MessageParser::default()
        .parse(bytes)
        .filter(|m| m.header_count() > 0)
        .ok_or_else(|| CreateError::Invalid(format!("{name}: not an email message")))?;
    let size = if font_size.is_finite() && font_size > 0.0 { font_size.clamp(4.0, 72.0) } else { 11.0 };

    let mut escaped = 0usize;
    let (mut body, referenced) = body_blocks(&message, &mut escaped);
    let mut doc = Document::new_empty();
    let mut images = inline_images(&mut doc, &message, &referenced);
    images.extend(data_images(&mut doc, &referenced));
    // A picture the body draws is not listed as an attachment; one it refers to but PdfCraft
    // can't draw (not an image it embeds) still travels as a file, so nothing is lost.
    let drawn: HashSet<String> = images.keys().cloned().collect();
    let (files, left_out) = attachments(&message, &drawn);
    let mut blocks = header_blocks(&message, &files, left_out, &mut escaped);
    if escaped > 0 {
        blocks.push(Block::Para { spans: vec![Span { face: Face::Oblique, text: ESCAPED_NOTE.into() }], quote: 0 });
        blocks.push(Block::Blank);
    }
    blocks.append(&mut body);

    let fonts = font_resources(&mut doc);
    let mut tree = Pages::of(&doc)?;
    for items in layout(&blocks, &images, (w, h), size, MARGIN) {
        write_page(&mut doc, &mut tree, &items, (w, h), size, &fonts);
    }
    tree.finish(&mut doc)?;
    let facts = EmailFacts {
        sent: message.date().map(|d| d.to_timestamp()),
        subject: message.subject().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string),
        attachments: files.iter().map(|f| (f.name.clone(), f.bytes.clone())).collect(),
        left_out,
    };
    embed_attachments(&mut doc, files, message.date().as_ref())?;
    let title = match message.subject().map(str::trim).filter(|s| !s.is_empty()) {
        Some(subject) => subject.to_string(),
        None => name.rsplit_once('.').map_or(name, |(stem, _)| stem).to_string(),
    };
    set_title(&mut doc, &title);
    set_author(&mut doc, &mailboxes(message.from()));
    Ok((doc, facts))
}

/// A PDF of the HTML file `bytes` (file `name`, for errors and the title when the file has no
/// `<title>`) on pages of `page` points at `font_size` points of Helvetica with one-inch margins:
/// the text as paragraphs with bold, italic, links, lists and quotes, and the pictures the file
/// carries inline (`data:` URIs) at the size it asks for. Nothing is fetched or read from disk:
/// a picture on the web or in another file is its alternative text. Characters Helvetica can't
/// draw are written as `[U+XXXX]` with a note. The bytes are UTF-8, or the Windows code page
/// [`decode_text`] picks. An empty file is refused.
pub fn from_html(name: &str, bytes: &[u8], page: (f64, f64), font_size: f64) -> Result<Document, CreateError> {
    let (w, h) = page;
    if !(w.is_finite() && h.is_finite() && (3.0..=MAX_SIDE).contains(&w) && (3.0..=MAX_SIDE).contains(&h)) {
        return Err(CreateError::Invalid("invalid page size".into()));
    }
    let html = decode_html(bytes);
    if html.trim().is_empty() {
        return Err(CreateError::Invalid(format!("{name}: the file is empty")));
    }
    let size = if font_size.is_finite() && font_size > 0.0 { font_size.clamp(4.0, 72.0) } else { 11.0 };
    let mut escaped = 0usize;
    let (mut blocks, referenced) = blocks_from_html(&html, &mut escaped).map_err(|e| CreateError::Invalid(format!("{name}: {e}")))?;
    if !blocks.iter().any(|b| !matches!(b, Block::Blank)) {
        blocks = vec![Block::Para { spans: vec![Span { face: Face::Oblique, text: "(the file has no text)".into() }], quote: 0 }];
    }
    if escaped > 0 {
        blocks.insert(0, Block::Blank);
        blocks.insert(0, Block::Para { spans: vec![Span { face: Face::Oblique, text: ESCAPED_NOTE.into() }], quote: 0 });
    }
    let mut doc = Document::new_empty();
    let images = data_images(&mut doc, &referenced);
    let fonts = font_resources(&mut doc);
    let mut tree = Pages::of(&doc)?;
    for items in layout(&blocks, &images, (w, h), size, DOCUMENT_MARGIN) {
        write_page(&mut doc, &mut tree, &items, (w, h), size, &fonts);
    }
    tree.finish(&mut doc)?;
    let title = match html_title(&html) {
        Some(title) => title,
        None => name.rsplit_once('.').map_or(name, |(stem, _)| stem).to_string(),
    };
    set_title(&mut doc, &title);
    Ok(doc)
}

/// The text of the first `<title>` outside a comment, whitespace collapsed, control characters
/// dropped, the common entities decoded, cut to [`MAX_TITLE`] characters. (html2text leaves
/// the head out, so the title is read from the text.)
pub(crate) fn html_title(html: &str) -> Option<String> {
    let html = without_comments(html);
    let lower = html.to_ascii_lowercase();
    let mut from = 0usize;
    let open = loop {
        let at = lower.get(from..)?.find("<title")?.checked_add(from)?;
        from = at.checked_add(6)?;
        if lower.get(from..)?.starts_with(|c: char| c.is_ascii_whitespace() || c == '>') {
            break at;
        }
    };
    let start = lower.get(open..)?.find('>')?.checked_add(open)?.checked_add(1)?;
    let rest = lower.get(start..)?;
    let end = rest.find("</title").unwrap_or(rest.len());
    let raw = html.get(start..start.checked_add(end)?)?;
    let text = decode_entities(raw).chars().filter(|c| !c.is_control()).collect::<String>();
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let text: String = text.chars().take(MAX_TITLE).collect();
    (!text.is_empty()).then_some(text)
}

/// `html` with its comments cut out (an unterminated one runs to the end), at most
/// [`MAX_COMMENTS`] of them.
fn without_comments(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    for _ in 0..MAX_COMMENTS {
        let Some(at) = rest.find("<!--") else { break };
        out.push_str(rest.get(..at).unwrap_or_default());
        let after = rest.get(at.saturating_add(4)..).unwrap_or_default();
        match after.find("-->") {
            Some(end) => rest = after.get(end.saturating_add(3)..).unwrap_or_default(),
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// The text of an HTML file: by its byte order mark, else by the charset a `<meta>` in the
/// first kilobyte declares (`charset=` or `content="…; charset=…"`), as a browser's prescan
/// reads it, else by [`decode_text`]'s guess. A UTF-16 label reads as UTF-8, as the HTML
/// standard says; a label nobody knows falls back to the guess.
fn decode_html(bytes: &[u8]) -> String {
    if encoding_rs::Encoding::for_bom(bytes).is_some() {
        return decode_text(bytes);
    }
    match meta_charset(bytes) {
        Some(encoding) => encoding.decode_without_bom_handling(bytes).0.into_owned(),
        None => decode_text(bytes),
    }
}

/// The encoding the first `<meta>` with a readable `charset` in the first kilobyte names.
fn meta_charset(bytes: &[u8]) -> Option<&'static encoding_rs::Encoding> {
    let head = String::from_utf8_lossy(bytes.get(..bytes.len().min(1024))?).to_ascii_lowercase();
    let mut from = 0usize;
    while let Some(at) = head.get(from..)?.find("<meta") {
        let tag_start = at.checked_add(from)?;
        let tag = head.get(tag_start..)?;
        let tag = tag.get(..tag.find('>').unwrap_or(tag.len()))?;
        from = tag_start.checked_add(tag.len()).and_then(|n| n.checked_add(1))?;
        let Some(after) = tag.find("charset").and_then(|i| tag.get(i.checked_add(7)?..)) else { continue };
        let Some(after) = after.trim_start().strip_prefix('=') else { continue };
        let after = after.trim_start();
        let (after, quoted) = match after.strip_prefix('"').or_else(|| after.strip_prefix('\'')) {
            Some(rest) => (rest, true),
            None => (after, false),
        };
        let end = after
            .find(|c: char| if quoted { c == '"' || c == '\'' } else { c.is_ascii_whitespace() || c == ';' || c == '"' || c == '\'' })
            .unwrap_or(after.len());
        let label = after.get(..end)?.trim();
        if label.is_empty() {
            continue;
        }
        let encoding = encoding_rs::Encoding::for_label(label.as_bytes())?;
        return Some(if encoding == encoding_rs::UTF_16LE || encoding == encoding_rs::UTF_16BE { encoding_rs::UTF_8 } else { encoding });
    }
    None
}

/// `&amp;`, `&lt;`, `&gt;`, `&quot;`, `&apos;`, `&nbsp;` and numeric references decoded; any
/// other reference is kept as written.
fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(rest.get(..at).unwrap_or_default());
        let after = rest.get(at..).unwrap_or_default();
        let Some(end) = after.find(';').filter(|&e| e <= 10) else {
            out.push('&');
            rest = after.get(1..).unwrap_or_default();
            continue;
        };
        let entity = after.get(1..end).unwrap_or_default();
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ => entity
                .strip_prefix('#')
                .and_then(|n| match n.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => n.parse().ok(),
                })
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => out.push(c),
            None => out.push_str(after.get(..=end).unwrap_or_default()),
        }
        rest = after.get(end.saturating_add(1)..).unwrap_or_default();
    }
    out.push_str(rest);
    out
}

/// The pictures an HTML file carries inline, embedded: `data:<type>;base64,<payload>` URIs the
/// body refers to (`referenced`), at most [`MAX_DATA_PICTURES`] of at most [`MAX_DATA_PICTURE`]
/// bytes each, to the image XObject and its size in pixels. One that isn't an image Create can
/// embed, or isn't base64, is left out (its text is shown).
fn data_images(doc: &mut Document, referenced: &HashSet<String>) -> HashMap<String, (ObjRef, (u32, u32))> {
    use base64::Engine as _;
    let mut out = HashMap::new();
    let mut keys: Vec<&String> = referenced.iter().filter(|k| k.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("data:"))).collect();
    keys.sort();
    for key in keys.into_iter().take(MAX_DATA_PICTURES) {
        let Some((header, payload)) = key.split_once(',') else { continue };
        if !header.to_ascii_lowercase().ends_with(";base64") || payload.len() > MAX_DATA_PICTURE {
            continue;
        }
        let compact: String = payload.chars().filter(|c| !c.is_whitespace()).collect();
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(compact.as_bytes()) else { continue };
        if let Ok(image) = image_xobject_with_pixels(doc, "picture", &bytes) {
            out.insert(key.clone(), (image.xobject, image.pixels));
        }
    }
    out
}

// ── the message as blocks ───────────────────────────────────────────────────────────────────

/// The four faces of the Helvetica family, as page resources F1 to F4.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Face {
    Regular,
    Bold,
    Oblique,
    BoldOblique,
}

impl Face {
    const ALL: [Face; 4] = [Face::Regular, Face::Bold, Face::Oblique, Face::BoldOblique];

    fn of(bold: bool, italic: bool) -> Face {
        match (bold, italic) {
            (false, false) => Face::Regular,
            (true, false) => Face::Bold,
            (false, true) => Face::Oblique,
            (true, true) => Face::BoldOblique,
        }
    }

    fn std14(self) -> Std14 {
        match self {
            Face::Regular => Std14::Helvetica,
            Face::Bold => Std14::HelveticaBold,
            Face::Oblique => Std14::HelveticaOblique,
            Face::BoldOblique => Std14::HelveticaBoldOblique,
        }
    }

    fn resource(self) -> &'static str {
        match self {
            Face::Regular => "F1",
            Face::Bold => "F2",
            Face::Oblique => "F3",
            Face::BoldOblique => "F4",
        }
    }

    fn width(self, text: &str, size: f64) -> f64 {
        self.std14().text_width(text, size)
    }
}

/// A run of text in one face.
#[derive(Clone, Debug, PartialEq)]
struct Span {
    face: Face,
    text: String,
}

/// The message in reading order, before layout.
#[derive(Debug, PartialEq)]
enum Block {
    /// A paragraph, indented `quote` levels (a quoted reply).
    Para { spans: Vec<Span>, quote: usize },
    /// A picture the HTML embeds, by its key (a Content-ID in a message, a `data:` URI in a
    /// file), with the text shown when it can't be drawn and the size the HTML asks for, in CSS
    /// pixels (width, height), where it gives one.
    Image { key: String, alt: String, css: (Option<f64>, Option<f64>) },
    /// Half a line of space.
    Blank,
    /// A line across the column (under the header block).
    Rule,
}

/// The header block: one row per header the message has, the label in bold, then a rule. The
/// Attachments row lists `files` and says how many more (`left_out`) were not embedded.
fn header_blocks(m: &Message, files: &[Attachment], left_out: usize, escaped: &mut usize) -> Vec<Block> {
    let mut rows: Vec<(&str, String)> = Vec::new();
    let from = mailboxes(m.from());
    if !from.is_empty() {
        rows.push(("From", from));
    }
    if let Some(date) = m.date() {
        rows.push(("Sent", date.to_rfc822()));
    }
    for (label, list) in [("To", m.to()), ("Cc", m.cc()), ("Bcc", m.bcc())] {
        let value = mailboxes(list);
        if !value.is_empty() {
            rows.push((label, value));
        }
    }
    if let Some(subject) = m.subject().map(str::trim).filter(|s| !s.is_empty()) {
        rows.push(("Subject", subject.to_string()));
    }
    if !files.is_empty() {
        let list: Vec<String> = files.iter().map(|f| format!("{} ({})", f.name, human_size(f.bytes.len()))).collect();
        let mut row = list.join(", ");
        if left_out > 0 {
            row.push_str(&format!(" (and {left_out} more, not embedded)"));
        }
        rows.push(("Attachments", row));
    }
    let mut out: Vec<Block> = rows
        .into_iter()
        .map(|(label, value)| Block::Para {
            spans: vec![Span { face: Face::Bold, text: format!("{label}: ") }, Span { face: Face::Regular, text: escape(&value, escaped) }],
            quote: 0,
        })
        .collect();
    out.push(Block::Rule);
    out.push(Block::Blank);
    out
}

/// `Name <address>` for each mailbox (groups flattened), joined with commas.
fn mailboxes(list: Option<AddressList<'_>>) -> String {
    let Some(list) = list else { return String::new() };
    let all: Vec<String> = list
        .mailboxes()
        .filter_map(|mb| {
            let name = mb.name().map(str::trim).filter(|n| !n.is_empty());
            let address = mb.address().map(str::trim).filter(|a| !a.is_empty());
            match (name, address) {
                (Some(n), Some(a)) if n != a => Some(format!("{n} <{a}>")),
                (_, Some(a)) => Some(a.to_string()),
                (Some(n), None) => Some(n.to_string()),
                (None, None) => None,
            }
        })
        .collect();
    all.join(", ")
}

/// `15 B`, `12 KB`, `1.5 MB`.
fn human_size(n: usize) -> String {
    const KB: f64 = 1024.0;
    let n = n as f64;
    if n < KB {
        format!("{n} B")
    } else if n < KB * KB {
        format!("{:.0} KB", n / KB)
    } else {
        format!("{:.1} MB", n / (KB * KB))
    }
}

/// `text` with every character Helvetica can't draw written as `[U+XXXX]` (counted in
/// `escaped`), control characters dropped, tabs as four spaces and no-break spaces as spaces.
fn escape(text: &str, escaped: &mut usize) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\t' => out.push_str("    "),
            '\u{a0}' => out.push(' '),
            c if c.is_control() => {}
            c if win_ansi_encodable(c) => out.push(c),
            c => {
                *escaped = escaped.saturating_add(1);
                out.push_str(&format!("[U+{:04X}]", u32::from(c)));
            }
        }
    }
    out
}

/// The body as blocks, and the keys of the pictures it draws: the first HTML body part that
/// says anything, else the first plain-text one, else a note that there is none.
fn body_blocks(m: &Message, escaped: &mut usize) -> (Vec<Block>, HashSet<String>) {
    if let Some(part) = m.html_body().find(|p| p.role() == PartRole::HtmlBody)
        && let Some(html) = part.html_utf8().or_else(|| part.text())
    {
        let mut count = 0usize;
        if let Ok(result) = blocks_from_html(&html, &mut count)
            && result.0.iter().any(|b| !matches!(b, Block::Blank))
        {
            *escaped = escaped.saturating_add(count);
            return result;
        }
    }
    // A part without a Content-Type is text/plain (RFC 2045 §5.2).
    let plain = |p: &mail_parser::MessagePart<'_>| {
        p.content_type().is_none_or(|c| c.ctype().eq_ignore_ascii_case("text") && c.subtype().is_none_or(|s| s.eq_ignore_ascii_case("plain")))
    };
    if let Some(part) = m.text_body().find(plain)
        && let Some(text) = part.text().filter(|t| !t.trim().is_empty())
    {
        return (blocks_from_text(&text, escaped), HashSet::new());
    }
    (vec![Block::Para { spans: vec![Span { face: Face::Oblique, text: "(no message body)".into() }], quote: 0 }], HashSet::new())
}

/// What html2text shows for a picture without alternative text, so the picture still reaches
/// the annotations (a signature logo has none, and is embedded by Content-ID all the same).
const NO_ALT: &str = "[image]";

/// HTML as blocks, and the keys of the pictures they draw (Content-IDs and `data:` URIs):
/// html2text's annotated lines, one per paragraph (it is given a width nothing reaches, and
/// tables in reading order), with bold, italic, links and pictures from the annotations, `> `
/// runs as quote levels and `* ` markers as bullets (nested ones indented).
fn blocks_from_html(html: &str, escaped: &mut usize) -> Result<(Vec<Block>, HashSet<String>), html2text::Error> {
    use html2text::config::ImageRenderMode;
    let config = html2text::config::with_decorator(Marks(RichDecorator::new()))
        .raw_mode(true)
        .allow_width_overflow()
        .no_link_wrapping()
        .empty_img_mode(ImageRenderMode::Replace(NO_ALT));
    let dom = config.parse_html(html.as_bytes())?;
    let sizes = picture_sizes(&dom);
    let lines = config.render_to_lines(config.dom_to_render_tree(&dom)?, HTML_WIDTH)?;
    let mut blocks = Vec::new();
    let mut cids = HashSet::new();
    for line in &lines {
        let mut spans: Vec<Span> = Vec::new();
        let mut quote = 0usize;
        let mut at_start = true;
        let mut heading = false;
        // A link's address is shown once the link ends, however many runs its text spans.
        let mut link: Option<(String, String)> = None;
        let mut bullet = false;
        for ts in line.tagged_strings() {
            let mut text: &str = &ts.s;
            if at_start {
                at_start = false;
                // Structure, as the decorator marks it: the renderer indents a nested item or
                // quote by two cells per level, then come the marks, each behind its space.
                // Preformatted text keeps its spaces.
                if !ts.tag.iter().any(|t| matches!(t, RichAnnotation::Preformat(_))) {
                    let unindented = text.trim_start_matches(' ');
                    let spaces = text.len().saturating_sub(unindented.len());
                    text = unindented;
                    let mut marks = 0usize;
                    while marks == 0 && spaces > 0 || marks > 0 {
                        let Some(rest) = text.strip_prefix([HEADING_MARK, QUOTE_MARK, BULLET_MARK]) else { break };
                        match text.chars().next() {
                            Some(QUOTE_MARK) => quote = quote.saturating_add(1),
                            Some(BULLET_MARK) => bullet = true,
                            _ => heading = true,
                        }
                        marks = marks.saturating_add(1);
                        // Another mark follows behind its own space; anything else is text.
                        match rest.strip_prefix(' ').filter(|r| r.starts_with([HEADING_MARK, QUOTE_MARK, BULLET_MARK])) {
                            Some(next) => text = next,
                            None => {
                                text = rest.trim_start_matches(' ');
                                break;
                            }
                        }
                    }
                    let depth = if marks > 0 { spaces.saturating_sub(1) / 2 } else { spaces / 2 };
                    quote = quote.saturating_add(depth);
                }
            }
            let bold = ts.tag.iter().any(|t| matches!(t, RichAnnotation::Strong));
            let italic = ts.tag.iter().any(|t| matches!(t, RichAnnotation::Emphasis));
            let image = ts.tag.iter().find_map(|t| if let RichAnnotation::Image(src) = t { Some(src.as_str()) } else { None });
            let url = ts.tag.iter().find_map(|t| if let RichAnnotation::Link(u) = t { Some(u.as_str()) } else { None });
            if url.is_none_or(|u| link.as_ref().is_none_or(|(l, _)| l != u)) {
                end_link(&mut spans, link.take(), escaped);
            }
            if bullet && spans.is_empty() {
                push_span(&mut spans, Face::Regular, "\u{2022} ".into());
            }
            if let Some(src) = image {
                let alt = if text.trim() == NO_ALT { String::new() } else { escape(text.trim(), escaped) };
                let src = src.trim();
                let scheme = |s: &str| src.get(..s.len()).is_some_and(|p| p.eq_ignore_ascii_case(s));
                let key = if scheme("cid:") {
                    src.get(4..).map(|cid| content_id(cid).to_string())
                } else if scheme("data:") {
                    Some(src.to_string())
                } else {
                    None
                };
                if let Some(key) = key {
                    if !spans.is_empty() {
                        blocks.push(Block::Para { spans: std::mem::take(&mut spans), quote });
                    }
                    cids.insert(key.clone());
                    let css = sizes.get(src).copied().unwrap_or((None, None));
                    blocks.push(Block::Image { key, alt, css });
                } else {
                    push_span(&mut spans, Face::Oblique, if alt.is_empty() { "[image]".into() } else { format!("[image: {alt}]") });
                }
                continue;
            }
            push_span(&mut spans, Face::of(bold || heading, italic), escape(text, escaped));
            if let Some(u) = url {
                match &mut link {
                    Some((_, shown)) => shown.push_str(text),
                    None => link = Some((u.to_string(), text.to_string())),
                }
            }
        }
        end_link(&mut spans, link.take(), escaped);
        if spans.iter().all(|s| s.text.trim().is_empty()) {
            push_blank(&mut blocks);
        } else {
            blocks.push(Block::Para { spans, quote });
        }
    }
    Ok((blocks, cids))
}

/// The size each `<img>` asks for, by its `src`: `width` and `height` attributes, overridden by
/// `width:` and `height:` in its `style`, in CSS pixels. A percentage or anything unreadable is
/// left out, so the picture takes its own size.
fn picture_sizes(dom: &html2text::RcDom) -> HashMap<String, (Option<f64>, Option<f64>)> {
    let mut sizes = HashMap::new();
    let mut stack: Vec<html2text::Handle> = vec![dom.document.clone()];
    let mut visited = 0usize;
    while let Some(node) = stack.pop() {
        visited = visited.saturating_add(1);
        if visited > MAX_NODES {
            break;
        }
        if let html2text::Element { name, attrs, .. } = &node.data
            && &*name.local == "img"
        {
            let (mut src, mut width, mut height, mut style) = (None, None, None, None);
            for a in attrs.borrow().iter() {
                let value = a.value.as_ref();
                match &*a.name.local {
                    "src" => src = Some(value.trim().to_string()),
                    "width" => width = css_length(value),
                    "height" => height = css_length(value),
                    "style" => style = Some(value.to_string()),
                    _ => {}
                }
            }
            if let Some(style) = style {
                for declaration in style.split(';') {
                    if let Some((property, value)) = declaration.split_once(':') {
                        match property.trim().to_ascii_lowercase().as_str() {
                            "width" => width = css_length(value).or(width),
                            "height" => height = css_length(value).or(height),
                            _ => {}
                        }
                    }
                }
            }
            if let Some(src) = src.filter(|s| !s.is_empty()) {
                sizes.entry(src).or_insert((width, height));
            }
        }
        stack.extend(node.children.borrow().iter().rev().cloned());
    }
    sizes
}

/// A CSS length in pixels: a bare number or `px` (`468`, `468px`), or `pt` (converted). Anything
/// else (`50%`, `auto`, `12em`) is `None`.
fn css_length(value: &str) -> Option<f64> {
    let v = value.trim().to_ascii_lowercase();
    let v = v.trim_end_matches("!important").trim_end().to_string();
    let (number, scale) = match v.strip_suffix("px") {
        Some(n) => (n.trim_end().to_string(), 1.0),
        None => match v.strip_suffix("pt") {
            Some(n) => (n.trim_end().to_string(), 4.0 / 3.0),
            None => (v, 1.0),
        },
    };
    let n: f64 = number.parse().ok()?;
    (n.is_finite() && n > 0.0 && n <= 10_000.0).then_some(n * scale)
}

/// Plain text as blocks: a paragraph per line, `>` runs as quote levels.
fn blocks_from_text(text: &str, escaped: &mut usize) -> Vec<Block> {
    let mut blocks = Vec::new();
    for raw in text.replace("\r\n", "\n").split(['\n', '\r']) {
        let mut line = raw;
        let mut quote = 0usize;
        while let Some(rest) = line.strip_prefix('>') {
            quote = quote.saturating_add(1);
            line = rest.strip_prefix(' ').unwrap_or(rest);
        }
        if line.trim().is_empty() {
            push_blank(&mut blocks);
        } else {
            blocks.push(Block::Para { spans: vec![Span { face: Face::Regular, text: escape(line, escaped) }], quote });
        }
    }
    blocks
}

/// A Content-ID without its angle brackets or surrounding space.
fn content_id(raw: &str) -> &str {
    raw.trim().trim_start_matches('<').trim_end_matches('>').trim()
}

/// Append `text` in `face`, joining the previous span when it is the same face.
fn push_span(spans: &mut Vec<Span>, face: Face, text: String) {
    if text.is_empty() {
        return;
    }
    match spans.last_mut() {
        Some(last) if last.face == face => last.text.push_str(&text),
        _ => spans.push(Span { face, text }),
    }
}

/// A link has ended: show its address after its text when the text doesn't already say it.
fn end_link(spans: &mut Vec<Span>, link: Option<(String, String)>, escaped: &mut usize) {
    if let Some((url, text)) = link
        && shows_address(&text, &url)
    {
        push_span(spans, Face::Regular, escape(&format!(" <{}>", shorten(&url)), escaped));
    }
}

/// Whether a link's address adds anything to its text: a web, mail or phone address that the
/// text doesn't spell out already (ignoring scheme, `www.`, case and a trailing slash).
pub(crate) fn shows_address(text: &str, url: &str) -> bool {
    let url = url.trim();
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:") || lower.starts_with("tel:")) {
        return false;
    }
    let bare = |s: &str| {
        let s = s.trim().to_ascii_lowercase();
        let s = ["https://", "http://", "mailto:", "tel:"].iter().find_map(|p| s.strip_prefix(p)).unwrap_or(&s);
        s.strip_prefix("www.").unwrap_or(s).trim_end_matches('/').to_string()
    };
    bare(text) != bare(url)
}

/// `url` cut to [`MAX_LINK`] characters, with an ellipsis when it was longer.
pub(crate) fn shorten(url: &str) -> String {
    let mut out: String = url.chars().take(MAX_LINK).collect();
    if url.chars().nth(MAX_LINK).is_some() {
        out.push('\u{2026}');
    }
    out
}

/// Blank lines collapse: at most two in a row, one line of space.
fn push_blank(blocks: &mut Vec<Block>) {
    if blocks.iter().rev().take_while(|b| matches!(b, Block::Blank)).count() < 2 {
        blocks.push(Block::Blank);
    }
}

// ── pictures and attachments ────────────────────────────────────────────────────────────────

/// The pictures the body refers to (`referenced` Content-IDs), embedded: Content-ID to the image
/// XObject and its size in pixels. A part that isn't an image Create can embed is left out (its
/// text is shown, and it stays an attachment).
fn inline_images(doc: &mut Document, m: &Message, referenced: &HashSet<String>) -> HashMap<String, (ObjRef, (u32, u32))> {
    let mut out = HashMap::new();
    if referenced.is_empty() {
        return out;
    }
    for part in m.parts() {
        let Some(cid) = part.content_id().map(content_id) else { continue };
        if !referenced.contains(cid) || out.contains_key(cid) {
            continue;
        }
        let name = part.attachment_name().unwrap_or("picture");
        if let Ok(image) = image_xobject_with_pixels(doc, name, &part.decoded()) {
            out.insert(cid.to_string(), (image.xobject, image.pixels));
        }
    }
    out
}

/// A file the message carries, to embed in the PDF.
struct Attachment {
    name: String,
    bytes: Vec<u8>,
    mime: Option<String>,
    description: Option<String>,
}

/// The message's attachments, except pictures the body draws (`drawn`), at most
/// [`MAX_ATTACHMENTS`], and how many more there were. A nameless one is `attachment-N` with
/// the extension its type suggests.
fn attachments(m: &Message, drawn: &HashSet<String>) -> (Vec<Attachment>, usize) {
    let mut out = Vec::new();
    let mut left_out = 0usize;
    for (i, part) in m.attachments().enumerate() {
        if part.content_id().map(content_id).is_some_and(|c| drawn.contains(c)) {
            continue;
        }
        if out.len() >= MAX_ATTACHMENTS {
            left_out = left_out.saturating_add(1);
            continue;
        }
        let mime = part.content_type().map(|c| match c.subtype() {
            Some(sub) => format!("{}/{sub}", c.ctype()).to_ascii_lowercase(),
            None => c.ctype().to_ascii_lowercase(),
        });
        let name = match part.attachment_name().map(file_name).filter(|n| !n.is_empty()) {
            Some(name) => name,
            None => format!("attachment-{}{}", i.saturating_add(1), extension(mime.as_deref())),
        };
        let description = part.content_description().map(str::trim).filter(|d| !d.is_empty()).map(str::to_string);
        out.push(Attachment { name, bytes: part.decoded().into_owned(), mime, description });
    }
    (out, left_out)
}

/// The file name part of `name` (folders and control characters dropped).
fn file_name(name: &str) -> String {
    name.rsplit(['/', '\\']).next().unwrap_or_default().chars().filter(|c| !c.is_control()).collect::<String>().trim().to_string()
}

/// A file extension for a MIME type, for attachments without a name.
fn extension(mime: Option<&str>) -> &'static str {
    match mime {
        Some("application/pdf") => ".pdf",
        Some("message/rfc822") => ".eml",
        Some("image/png") => ".png",
        Some("image/jpeg") => ".jpg",
        Some("image/gif") => ".gif",
        Some("text/plain") => ".txt",
        Some("text/html") => ".html",
        _ => "",
    }
}

/// Embed `files` as document-level attachments: a flat, sorted `/Names /EmbeddedFiles` tree of
/// file specifications (names made unique with " (2)", " (3)"…), each with its stream, size,
/// type and the message's date as its modification date.
fn embed_attachments(doc: &mut Document, files: Vec<Attachment>, date: Option<&DateTime>) -> Result<(), CreateError> {
    if files.is_empty() {
        return Ok(());
    }
    let mut taken: HashSet<String> = HashSet::new();
    let mut entries: Vec<(Vec<u8>, Object)> = Vec::with_capacity(files.len());
    for file in files {
        let mut key = file.name.clone();
        let mut n = 2usize;
        while !taken.insert(key.clone()) {
            key = format!("{} ({n})", file.name);
            n = n.saturating_add(1);
        }
        let mut params = Dict::new();
        params.set(b"Size".to_vec(), Object::Int(i64::try_from(file.bytes.len()).unwrap_or(i64::MAX)));
        if let Some(date) = date {
            params.set(b"ModDate".to_vec(), Object::String(PdfString::literal(pdf_date(date).into_bytes())));
        }
        let mut embedded = Dict::new();
        embedded.set(b"Type".to_vec(), Object::name("EmbeddedFile"));
        if let Some(mime) = &file.mime {
            embedded.set(b"Subtype".to_vec(), Object::Name(mime.as_bytes().to_vec()));
        }
        embedded.set(b"Params".to_vec(), Object::Dict(params));
        let stream = doc.add(Object::Stream(Stream::flate(embedded, &file.bytes)));

        let mut spec = Dict::new();
        spec.set(b"Type".to_vec(), Object::name("Filespec"));
        spec.set(b"F".to_vec(), Object::String(PdfString::text(&key)));
        spec.set(b"UF".to_vec(), Object::String(PdfString::text(&key)));
        if let Some(description) = &file.description {
            spec.set(b"Desc".to_vec(), Object::String(PdfString::text(description)));
        }
        let mut ef = Dict::new();
        ef.set(b"F".to_vec(), Object::Ref(stream));
        ef.set(b"UF".to_vec(), Object::Ref(stream));
        spec.set(b"EF".to_vec(), Object::Dict(ef));
        let spec = doc.add(spec);
        entries.push((PdfString::text(&key).bytes, Object::Ref(spec)));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut names = Vec::with_capacity(entries.len().saturating_mul(2));
    for (key, spec) in entries {
        names.push(Object::String(PdfString::literal(key)));
        names.push(spec);
    }
    let mut tree = Dict::new();
    tree.set(b"Names".to_vec(), Object::Array(names));
    let tree = doc.add(tree);
    let root = doc.root().ok_or_else(|| CreateError::Invalid("the new document has no catalog".into()))?;
    doc.update_dict(root, |catalog| {
        let mut names = match catalog.get(b"Names") {
            Some(Object::Dict(d)) => d.clone(),
            _ => Dict::new(),
        };
        names.set(b"EmbeddedFiles".to_vec(), Object::Ref(tree));
        catalog.set(b"Names".to_vec(), Object::Dict(names));
    })
    .map_err(|e| CreateError::Invalid(e.to_string()))
}

/// A PDF date string (ISO 32000-2 §7.9.4) from the fields of a parsed message date. This is
/// output formatting only: the fields are the message's own, no calendar arithmetic is done.
fn pdf_date(d: &DateTime) -> String {
    let sign = if d.tz_before_gmt { '-' } else { '+' };
    format!("D:{:04}{:02}{:02}{:02}{:02}{:02}{sign}{:02}'{:02}'", d.year, d.month, d.day, d.hour, d.minute, d.second, d.tz_hour, d.tz_minute)
}

/// `/Author` in the document information dictionary [`set_title`] made.
fn set_author(doc: &mut Document, author: &str) {
    if author.is_empty() {
        return;
    }
    if let Some(info) = doc.trailer_mut().reference(b"Info") {
        // The dictionary was just added by `set_title`; a failure here would only lose /Author.
        let _ = doc.update_dict(info, |d| d.set(b"Author".to_vec(), PdfString::text(author)));
    }
}

// ── layout ──────────────────────────────────────────────────────────────────────────────────

/// Something placed on a page, in page points (`y` is a text baseline or a picture's bottom).
#[derive(Debug, PartialEq)]
enum Item {
    Text { x: f64, y: f64, spans: Vec<Span> },
    Image { x: f64, y: f64, w: f64, h: f64, xobject: ObjRef },
    Rule { x: f64, y: f64, w: f64 },
}

/// Where the next line goes: pages fill from the top baseline down to the bottom one.
struct Cursor {
    pages: Vec<Vec<Item>>,
    y: f64,
    top: f64,
    bottom: f64,
    line_h: f64,
}

impl Cursor {
    fn new_page(&mut self) {
        self.pages.push(Vec::new());
        self.y = self.top;
    }

    fn page_is_empty(&self) -> bool {
        self.pages.last().is_none_or(Vec::is_empty)
    }

    fn put(&mut self, item: Item) {
        match self.pages.last_mut() {
            Some(page) => page.push(item),
            None => self.pages.push(vec![item]),
        }
    }

    /// A line of text at `x`, on the next page when this one is full.
    fn line(&mut self, x: f64, spans: Vec<Span>) {
        if self.y < self.bottom {
            self.new_page();
        }
        let y = self.y;
        self.put(Item::Text { x, y, spans });
        self.y -= self.line_h;
    }
}

/// Blocks on pages of `(w, h)` points with `margin` points around: paragraphs wrapped to the
/// column, quotes indented, pictures at the size the HTML asks for, else their pixel size (CSS
/// pixels, as a mail program shows them), scaled down to the column and at most
/// [`MAX_IMAGE_SHARE`] of the inner height (a picture that doesn't fit what is left of a page
/// starts the next), blanks as half lines.
fn layout(blocks: &[Block], images: &HashMap<String, (ObjRef, (u32, u32))>, (w, h): (f64, f64), size: f64, margin: f64) -> Vec<Vec<Item>> {
    let line_h = size * 1.22;
    let column = (w - 2.0 * margin).max(36.0);
    let top = h - margin - size;
    let mut cursor = Cursor { pages: vec![Vec::new()], y: top, top, bottom: margin, line_h };
    for block in blocks {
        match block {
            Block::Blank => cursor.y -= line_h * 0.5,
            Block::Rule => {
                if cursor.y < cursor.bottom {
                    cursor.new_page();
                }
                let y = cursor.y + size * 0.3;
                cursor.put(Item::Rule { x: margin, y, w: column });
                cursor.y -= line_h * 0.5;
            }
            Block::Para { spans, quote } => {
                let indent = (*quote as f64 * size * 1.5).min(column * 0.5);
                for line in wrap_spans(spans, size, (column - indent).max(36.0)) {
                    cursor.line(margin + indent, line);
                }
            }
            Block::Image { key, alt, css } => match images.get(key) {
                Some(&(xobject, (px_w, px_h))) if px_w > 0 && px_h > 0 => {
                    let (iw, ih) = shown_size((f64::from(px_w), f64::from(px_h)), *css);
                    let scale = (column / iw).min((h - 2.0 * margin) * MAX_IMAGE_SHARE / ih).min(1.0);
                    let (dw, dh) = (iw * scale, ih * scale);
                    if cursor.y + size * 0.8 - dh < cursor.bottom && !cursor.page_is_empty() {
                        cursor.new_page();
                    }
                    let top_edge = cursor.y + size * 0.8;
                    cursor.put(Item::Image { x: margin, y: top_edge - dh, w: dw, h: dh, xobject });
                    cursor.y = top_edge - dh - line_h;
                }
                _ => {
                    let label = if alt.is_empty() { "[image]".to_string() } else { format!("[image: {alt}]") };
                    for line in wrap_spans(&[Span { face: Face::Oblique, text: label }], size, column) {
                        cursor.line(margin, line);
                    }
                }
            },
        }
    }
    cursor.pages
}

/// A picture's size in points before fitting: the CSS pixels the HTML asks for (a missing side
/// follows the picture's proportions), else its own pixels, at [`CSS_PX`] a pixel.
fn shown_size((px_w, px_h): (f64, f64), css: (Option<f64>, Option<f64>)) -> (f64, f64) {
    let ratio = px_h / px_w;
    let (w, h) = match css {
        (Some(w), Some(h)) => (w, h),
        (Some(w), None) => (w, w * ratio),
        (None, Some(h)) => (h / ratio, h),
        (None, None) => (px_w, px_h),
    };
    (w * CSS_PX, h * CSS_PX)
}

/// Greedy line breaking of mixed-face spans within `width` points: each word is measured in its
/// own face, a word wider than a line is broken by character, and spaces at the start of a line
/// are dropped except on the first (a paragraph may be indented on purpose).
fn wrap_spans(spans: &[Span], size: f64, width: f64) -> Vec<Vec<Span>> {
    let mut lines: Vec<Vec<Span>> = vec![Vec::new()];
    let mut used = 0.0f64;
    let append = |lines: &mut Vec<Vec<Span>>, face: Face, text: &str| {
        if let Some(line) = lines.last_mut() {
            push_span(line, face, text.to_string());
        }
    };
    for span in spans {
        for token in tokens(&span.text) {
            let is_space = token.starts_with(' ');
            let tw = span.face.width(token, size);
            if used + tw <= width {
                append(&mut lines, span.face, token);
                used += tw;
            } else if is_space {
                lines.push(Vec::new());
                used = 0.0;
            } else {
                if used > 0.0 {
                    lines.push(Vec::new());
                    used = 0.0;
                }
                if tw <= width {
                    append(&mut lines, span.face, token);
                    used = tw;
                } else {
                    let mut buf = [0u8; 4];
                    for c in token.chars() {
                        let s: &str = c.encode_utf8(&mut buf);
                        let cw = span.face.width(s, size);
                        if used + cw > width && used > 0.0 {
                            lines.push(Vec::new());
                            used = 0.0;
                        }
                        append(&mut lines, span.face, s);
                        used += cw;
                    }
                }
            }
        }
    }
    while lines.len() > 1 && lines.last().is_some_and(|l| l.iter().all(|s| s.text.trim().is_empty())) {
        lines.pop();
    }
    lines
}

/// `text` as alternating runs of spaces and non-spaces.
fn tokens(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut in_space: Option<bool> = None;
    for (i, c) in text.char_indices() {
        let space = c == ' ';
        if in_space.is_some_and(|s| s != space) {
            out.extend(text.get(start..i));
            start = i;
        }
        in_space = Some(space);
    }
    out.extend(text.get(start..).filter(|t| !t.is_empty()));
    out
}

// ── writing ─────────────────────────────────────────────────────────────────────────────────

/// The Helvetica family as Type 1 font dictionaries, in WinAnsiEncoding.
fn font_resources(doc: &mut Document) -> [(Face, ObjRef); 4] {
    Face::ALL.map(|face| {
        let mut font = Dict::new();
        font.set(b"Type".to_vec(), Object::name("Font"));
        font.set(b"Subtype".to_vec(), Object::name("Type1"));
        font.set(b"BaseFont".to_vec(), Object::name(face.std14().base_font()));
        font.set(b"Encoding".to_vec(), Object::name("WinAnsiEncoding"));
        (face, doc.add(font))
    })
}

/// A number for a content stream.
fn num(v: f64) -> String {
    format!("{v:.2}")
}

/// One page of items as a page object with its content stream and resources, added to `tree`.
fn write_page(doc: &mut Document, tree: &mut Pages, items: &[Item], (w, h): (f64, f64), size: f64, fonts: &[(Face, ObjRef); 4]) {
    let mut content = Vec::new();
    let mut xobjects = Dict::new();
    let mut pictures = 0usize;
    for item in items {
        match item {
            Item::Text { x, y, spans } => {
                content.extend_from_slice(format!("BT 1 0 0 1 {} {} Tm\n", num(*x), num(*y)).as_bytes());
                let mut face = None;
                for span in spans {
                    if face != Some(span.face) {
                        content.extend_from_slice(format!("/{} {} Tf ", span.face.resource(), num(size)).as_bytes());
                        face = Some(span.face);
                    }
                    content.extend(literal(&win_ansi(&span.text)));
                    content.extend_from_slice(b" Tj\n");
                }
                content.extend_from_slice(b"ET\n");
            }
            Item::Rule { x, y, w } => {
                content.extend_from_slice(format!("q 0.5 w 0.6 G {} {} m {} {} l S Q\n", num(*x), num(*y), num(x + w), num(*y)).as_bytes());
            }
            Item::Image { x, y, w, h, xobject } => {
                pictures = pictures.saturating_add(1);
                let name = format!("Im{pictures}");
                content.extend_from_slice(format!("q {} 0 0 {} {} {} cm /{name} Do Q\n", num(*w), num(*h), num(*x), num(*y)).as_bytes());
                xobjects.set(name.into_bytes(), Object::Ref(*xobject));
            }
        }
    }
    let mut font_dict = Dict::new();
    for (face, font) in fonts {
        font_dict.set(face.resource().as_bytes().to_vec(), Object::Ref(*font));
    }
    let mut resources = Dict::new();
    resources.set(b"Font".to_vec(), Object::Dict(font_dict));
    if pictures > 0 {
        resources.set(b"XObject".to_vec(), Object::Dict(xobjects));
    }
    tree.add(doc, w, h, resources, Some(content));
}
