//! Outlook messages (`.msg`: MAPI properties in an OLE compound file, [MS-OXMSG]) as RFC 5322
//! bytes, so a `.msg` goes through the same pipeline as a `.eml` ([`crate::from_email`]).
//!
//! What is read: the subject, the sender, the recipients (To and Cc; Bcc is left out, as a
//! printed email leaves it out), the sent time (submit time, else delivery time, else last
//! modified), the HTML body (decoded from its code page) and the plain-text body, and every
//! attachment with its name, type and Content-ID. An attached Outlook message is converted the
//! same way and attached as a `message/rfc822` part. The RTF body is not read: when a message
//! has no HTML body its plain-text body stands in, with the same words.
//!
//! The output is plain ASCII: header values that aren't are RFC 2047 encoded words, and every
//! body and attachment is base64, so nothing in the message can forge a header or a boundary.

use std::io::{Cursor, Read};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use crate::CreateError;

/// The OLE compound-file signature.
const OLE: &[u8] = &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
/// How deep attached messages are followed; one deeper is left out with a note.
const MAX_DEPTH: usize = 8;
/// The most attachments read from one message (attached messages' own included).
const MAX_ATTACHMENTS: usize = 2_000;

// MAPI property ids ([MS-OXPROPS]).
const SUBJECT: u16 = 0x0037;
const SENDER_NAME: u16 = 0x0C1A;
const SENDER_EMAIL: u16 = 0x0C1F;
const SENDER_SMTP: u16 = 0x5D01;
const DISPLAY_TO: u16 = 0x0E04;
const DISPLAY_CC: u16 = 0x0E03;
const BODY: u16 = 0x1000;
const HTML: u16 = 0x1013;
const MESSAGE_ID: u16 = 0x1035;
const TRANSPORT_HEADERS: u16 = 0x007D;
const RECIPIENT_NAME: u16 = 0x3001;
const RECIPIENT_EMAIL: u16 = 0x3003;
const RECIPIENT_SMTP: u16 = 0x39FE;
const ATTACH_LONG_NAME: u16 = 0x3707;
const ATTACH_NAME: u16 = 0x3704;
const ATTACH_DATA: u16 = 0x3701;
const ATTACH_MIME: u16 = 0x370E;
const ATTACH_CID: u16 = 0x3712;
/// Fixed-size properties (tag = id << 16 | type).
const SUBMIT_TIME: u32 = 0x0039_0040;
const DELIVERY_TIME: u32 = 0x0E06_0040;
const MODIFIED_TIME: u32 = 0x3008_0040;
const INTERNET_CPID: u32 = 0x3FDE_0003;
const MESSAGE_CODEPAGE: u32 = 0x3FFD_0003;
const RECIPIENT_TYPE: u32 = 0x0C15_0003;
const ATTACH_METHOD: u32 = 0x3705_0003;
/// `PR_ATTACH_METHOD` of an attached Outlook message.
const ATTACH_EMBEDDED_MESSAGE: u64 = 5;

type Cfb<'a> = cfb::CompoundFile<Cursor<&'a [u8]>>;

/// Whether `bytes` is an Outlook message: an OLE compound file holding a message's property
/// stream. (Word 97–2003 documents are OLE files too; they have no such stream.)
pub fn is_msg(bytes: &[u8]) -> bool {
    bytes.starts_with(OLE) && cfb::CompoundFile::open(Cursor::new(bytes)).is_ok_and(|f| f.is_stream("/__properties_version1.0"))
}

/// The Outlook message `bytes` (file `name`, for errors) as an RFC 5322 message with MIME.
pub fn msg_to_eml(name: &str, bytes: &[u8]) -> Result<Vec<u8>, CreateError> {
    let not_a_message = || CreateError::Invalid(format!("{name}: not an Outlook message"));
    if !bytes.starts_with(OLE) {
        return Err(not_a_message());
    }
    let mut file = cfb::CompoundFile::open(Cursor::new(bytes)).map_err(|_| not_a_message())?;
    if !file.is_stream("/__properties_version1.0") {
        return Err(not_a_message());
    }
    // Every stream read counts against what the file holds four times over: a file whose
    // directory points many entries at the same sectors can't multiply into gigabytes.
    let mut reader = Reader { file: &mut file, budget: (bytes.len() as u64).saturating_mul(4), attachments: 0 };
    let mut out = Vec::new();
    reader.message("/", 32, 0, &mut out)?;
    Ok(out)
}

struct Reader<'f, 'a> {
    file: &'f mut Cfb<'a>,
    /// Bytes still allowed to be read from streams.
    budget: u64,
    attachments: usize,
}

/// One attachment as it goes into the message.
struct Part {
    name: String,
    mime: String,
    content_id: Option<String>,
    body: PartBody,
}

enum PartBody {
    Bytes(Vec<u8>),
    /// An attached message, already written as RFC 5322 (ASCII only, so 7bit).
    Message(Vec<u8>),
}

impl Reader<'_, '_> {
    /// A stream's bytes, within the budget.
    fn read(&mut self, path: &str) -> Option<Vec<u8>> {
        let stream = self.file.open_stream(path).ok()?;
        let len = stream.len();
        if len > self.budget {
            self.budget = 0;
            return None;
        }
        self.budget -= len;
        let mut bytes = Vec::with_capacity(usize::try_from(len).ok()?);
        stream.take(len).read_to_end(&mut bytes).ok()?;
        Some(bytes)
    }

    /// A string property of the object in `dir`: Unicode (`001F`), else 8-bit (`001E`) in
    /// `codepage`.
    fn string(&mut self, dir: &str, id: u16, codepage: Option<u32>) -> Option<String> {
        if let Some(bytes) = self.read(&format!("{dir}__substg1.0_{id:04X}001F")) {
            return Some(utf16(&bytes));
        }
        let bytes = self.read(&format!("{dir}__substg1.0_{id:04X}001E"))?;
        Some(decode(&bytes, codepage).trim_end_matches('\0').to_string())
    }

    /// The fixed-size properties of the object in `dir` (its property stream has a header of
    /// `header` bytes: 32 for the message, 24 for an attached message, 8 otherwise).
    fn fixed(&mut self, dir: &str, header: usize) -> Vec<(u32, u64)> {
        let Some(stream) = self.read(&format!("{dir}__properties_version1.0")) else { return Vec::new() };
        stream
            .get(header..)
            .unwrap_or_default()
            .as_chunks::<16>()
            .0
            .iter()
            .map(|e| (u32::from_le_bytes([e[0], e[1], e[2], e[3]]), u64::from_le_bytes([e[8], e[9], e[10], e[11], e[12], e[13], e[14], e[15]])))
            .collect()
    }

    /// The storages directly in `dir` whose name starts with `prefix`, in name order.
    fn children(&self, dir: &str, prefix: &str) -> Vec<String> {
        let Ok(entries) = self.file.read_storage(dir) else { return Vec::new() };
        let mut out: Vec<String> =
            entries.filter(|e| e.is_storage() && e.name().starts_with(prefix)).map(|e| format!("{dir}{}/", e.name())).collect();
        out.sort();
        out
    }

    /// Write the message in `dir` to `out`.
    fn message(&mut self, dir: &str, header: usize, depth: usize, out: &mut Vec<u8>) -> Result<(), CreateError> {
        let props = self.fixed(dir, header);
        let prop = |tag: u32| props.iter().find(|(t, _)| *t == tag).map(|(_, v)| *v);
        let codepage = prop(MESSAGE_CODEPAGE).or(prop(INTERNET_CPID)).and_then(|v| u32::try_from(v & 0xFFFF_FFFF).ok());
        let cpid = prop(INTERNET_CPID).and_then(|v| u32::try_from(v & 0xFFFF_FFFF).ok());

        let subject = self.string(dir, SUBJECT, codepage);
        let sender_name = self.string(dir, SENDER_NAME, codepage);
        let sender = self.string(dir, SENDER_SMTP, codepage).or_else(|| self.string(dir, SENDER_EMAIL, codepage)).filter(|a| a.contains('@'));
        let (to, cc) = self.recipients(dir, codepage);
        let to = if to.is_empty() { self.string(dir, DISPLAY_TO, codepage).map(|d| vec![(d, None)]).unwrap_or_default() } else { to };
        let cc = if cc.is_empty() { self.string(dir, DISPLAY_CC, codepage).map(|d| vec![(d, None)]).unwrap_or_default() } else { cc };
        let sent = [SUBMIT_TIME, DELIVERY_TIME, MODIFIED_TIME].into_iter().find_map(prop).and_then(filetime_to_unix);
        let message_id =
            self.string(dir, MESSAGE_ID, codepage).map(|id| clean(&id).replace(' ', "")).filter(|id| id.starts_with('<') && id.ends_with('>'));

        let text = self.string(dir, BODY, codepage).filter(|t| !t.trim().is_empty());
        let html = match self.read(&format!("{dir}__substg1.0_{HTML:04X}0102")) {
            Some(bytes) => Some(decode(&bytes, cpid.or(codepage))),
            None => self.string(dir, HTML, codepage),
        }
        .map(|h| h.trim_end_matches('\0').to_string())
        .filter(|h| !h.trim().is_empty());
        let (parts, left_out) = self.parts(dir, depth)?;

        let mut head = String::new();
        if let Some(name) = sender_name.as_deref().map(clean).filter(|n| !n.is_empty()) {
            match &sender {
                Some(address) => head.push_str(&format!("From: {} <{}>\r\n", phrase(&name), address_only(address))),
                None => head.push_str(&format!("From: {}\r\n", phrase(&name))),
            }
        } else if let Some(address) = &sender {
            head.push_str(&format!("From: <{}>\r\n", address_only(address)));
        }
        for (label, list) in [("To", &to), ("Cc", &cc)] {
            if !list.is_empty() {
                head.push_str(&format!("{label}: {}\r\n", list.iter().map(|(n, a)| mailbox(n, a.as_deref())).collect::<Vec<_>>().join(", ")));
            }
        }
        if let Some(subject) = &subject {
            head.push_str(&format!("Subject: {}\r\n", encode_word(&clean(subject))));
        }
        // The Date the message travelled with keeps the sender's time zone; the MAPI time is UTC.
        let travelled = self.string(dir, TRANSPORT_HEADERS, codepage).as_deref().and_then(header_date);
        if let Some(date) = travelled.or_else(|| sent.and_then(rfc2822)) {
            head.push_str(&format!("Date: {date}\r\n"));
        }
        if let Some(id) = message_id {
            head.push_str(&format!("Message-ID: {id}\r\n"));
        }
        head.push_str("MIME-Version: 1.0\r\n");
        out.extend_from_slice(head.as_bytes());

        let boundary = format!("=_pdfcraft_msg_{depth}");
        let alt = format!("=_pdfcraft_alt_{depth}");
        out.extend_from_slice(format!("Content-Type: multipart/mixed; boundary=\"{boundary}\"\r\n\r\n").as_bytes());
        out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        match (&text, &html) {
            (Some(text), Some(html)) => {
                out.extend_from_slice(format!("Content-Type: multipart/alternative; boundary=\"{alt}\"\r\n\r\n--{alt}\r\n").as_bytes());
                text_part(out, "text/plain", text);
                out.extend_from_slice(format!("--{alt}\r\n").as_bytes());
                text_part(out, "text/html", html);
                out.extend_from_slice(format!("--{alt}--\r\n").as_bytes());
            }
            (_, Some(html)) => text_part(out, "text/html", html),
            (Some(text), None) => text_part(out, "text/plain", text),
            (None, None) => text_part(out, "text/plain", ""),
        }
        if left_out > 0 {
            out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            text_part(out, "text/plain", &format!("({left_out} attached messages nested more than {MAX_DEPTH} deep were left out.)"));
        }
        for part in parts {
            out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            write_part(out, part);
        }
        out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        Ok(())
    }

    /// The To and Cc recipients of the message in `dir` as (name, address); Bcc is left out.
    #[allow(clippy::type_complexity)]
    fn recipients(&mut self, dir: &str, codepage: Option<u32>) -> (Vec<(String, Option<String>)>, Vec<(String, Option<String>)>) {
        let (mut to, mut cc) = (Vec::new(), Vec::new());
        for r in self.children(dir, "__recip_version1.0_#") {
            let kind = self.fixed(&r, 8).into_iter().find(|(t, _)| *t == RECIPIENT_TYPE).map_or(1, |(_, v)| v & 0xFFFF_FFFF);
            let name = self.string(&r, RECIPIENT_NAME, codepage).unwrap_or_default();
            let address =
                self.string(&r, RECIPIENT_SMTP, codepage).or_else(|| self.string(&r, RECIPIENT_EMAIL, codepage)).filter(|a| a.contains('@'));
            if name.trim().is_empty() && address.is_none() {
                continue;
            }
            match kind {
                1 => to.push((name, address)),
                2 => cc.push((name, address)),
                _ => {}
            }
        }
        (to, cc)
    }

    /// The attachments of the message in `dir`, and how many attached messages were too deep.
    fn parts(&mut self, dir: &str, depth: usize) -> Result<(Vec<Part>, usize), CreateError> {
        let mut parts = Vec::new();
        let mut too_deep = 0usize;
        for a in self.children(dir, "__attach_version1.0_#") {
            if self.attachments >= MAX_ATTACHMENTS {
                break;
            }
            self.attachments += 1;
            let props = self.fixed(&a, 8);
            let prop = |tag: u32| props.iter().find(|(t, _)| *t == tag).map(|(_, v)| *v);
            let name =
                self.string(&a, ATTACH_LONG_NAME, None).or_else(|| self.string(&a, ATTACH_NAME, None)).map(|n| clean(&n)).filter(|n| !n.is_empty());
            let content_id = self.string(&a, ATTACH_CID, None).map(|c| clean(c.trim_matches(['<', '>'])).replace(' ', "")).filter(|c| !c.is_empty());
            let embedded = format!("{a}__substg1.0_{ATTACH_DATA:04X}000D/");
            if prop(ATTACH_METHOD) == Some(ATTACH_EMBEDDED_MESSAGE) && self.file.is_storage(&embedded) {
                if depth + 1 >= MAX_DEPTH {
                    too_deep += 1;
                    continue;
                }
                let mut inner = Vec::new();
                self.message(&embedded, 24, depth + 1, &mut inner)?;
                let name = name.unwrap_or_else(|| "message".into());
                let name = if name.to_ascii_lowercase().ends_with(".eml") { name } else { format!("{name}.eml") };
                parts.push(Part { name, mime: "message/rfc822".into(), content_id: None, body: PartBody::Message(inner) });
                continue;
            }
            let Some(bytes) = self.read(&format!("{a}__substg1.0_{ATTACH_DATA:04X}0102")) else { continue };
            let mime = self
                .string(&a, ATTACH_MIME, None)
                .map(|m| m.trim().to_ascii_lowercase())
                .filter(|m| m.split_once('/').is_some_and(|(t, s)| token(t) && token(s)))
                .unwrap_or_else(|| "application/octet-stream".into());
            let name = name.unwrap_or_else(|| format!("attachment-{}", parts.len() + 1));
            parts.push(Part { name, mime, content_id, body: PartBody::Bytes(bytes) });
        }
        Ok((parts, too_deep))
    }
}

/// A MIME token (type or subtype): visible ASCII without specials.
fn token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_graphic() && !b"()<>@,;:\\\"/[]?=".contains(&b))
}

/// A text part, UTF-8 in base64.
fn text_part(out: &mut Vec<u8>, mime: &str, text: &str) {
    out.extend_from_slice(format!("Content-Type: {mime}; charset=\"utf-8\"\r\nContent-Transfer-Encoding: base64\r\n\r\n").as_bytes());
    base64_lines(out, text.as_bytes());
}

/// An attachment part.
fn write_part(out: &mut Vec<u8>, part: Part) {
    let ascii = part.name.is_ascii() && !part.name.contains(['"', '\\']);
    let quoted = if ascii { part.name.clone() } else { encode_word(&part.name) };
    // Always an attachment, with its Content-ID: a picture the HTML draws by it is drawn (and so
    // not listed), and one it doesn't is still a file, never dropped as an unused inline part.
    let mut head = format!("Content-Type: {}; name=\"{quoted}\"\r\nContent-Disposition: attachment; filename=\"{quoted}\"", part.mime);
    if !ascii {
        head.push_str(&format!("; filename*=UTF-8''{}", percent(&part.name)));
    }
    head.push_str("\r\n");
    if let Some(cid) = &part.content_id {
        head.push_str(&format!("Content-ID: <{cid}>\r\n"));
    }
    match part.body {
        PartBody::Bytes(bytes) => {
            head.push_str("Content-Transfer-Encoding: base64\r\n\r\n");
            out.extend_from_slice(head.as_bytes());
            base64_lines(out, &bytes);
        }
        PartBody::Message(message) => {
            head.push_str("Content-Transfer-Encoding: 7bit\r\n\r\n");
            out.extend_from_slice(head.as_bytes());
            out.extend_from_slice(&message);
            out.extend_from_slice(b"\r\n");
        }
    }
}

/// `bytes` in base64, 76 characters a line.
fn base64_lines(out: &mut Vec<u8>, bytes: &[u8]) {
    for chunk in bytes.chunks(57) {
        out.extend_from_slice(STANDARD.encode(chunk).as_bytes());
        out.extend_from_slice(b"\r\n");
    }
}

/// `s` without control characters (CR, LF, NUL included), whitespace runs collapsed.
fn clean(s: &str) -> String {
    s.split(|c: char| c.is_control() || c.is_whitespace()).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ")
}

/// An address with nothing that could close the angle brackets or start another header.
fn address_only(a: &str) -> String {
    a.chars().filter(|c| !c.is_control() && !c.is_whitespace() && !matches!(c, '<' | '>' | '"' | ',' | ';')).collect()
}

/// A display name as an RFC 5322 phrase: an encoded word, so any character is safe.
fn phrase(name: &str) -> String {
    encode_word(name)
}

/// One mailbox for a To or Cc list.
fn mailbox(name: &str, address: Option<&str>) -> String {
    let name = clean(name);
    match (name.is_empty(), address) {
        (false, Some(a)) => format!("{} <{}>", phrase(&name), address_only(a)),
        (true, Some(a)) => format!("<{}>", address_only(a)),
        // A display name Outlook kept without an address: written as a group with no members,
        // which a parser shows as the name and never mistakes for an address.
        (_, None) => format!("{}:;", phrase(&name)),
    }
}

/// `s` as RFC 2047 encoded words (UTF-8, base64), each at most 75 characters, joined by
/// folding whitespace; a plain ASCII word stays as it is.
fn encode_word(s: &str) -> String {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b" .-_'!#$%&*+/=?^`{|}~".contains(&b)) && !s.contains("=?") {
        return s.to_string();
    }
    let mut words = Vec::new();
    let mut chunk = String::new();
    for c in s.chars() {
        if chunk.len() + c.len_utf8() > 45 {
            words.push(format!("=?UTF-8?B?{}?=", STANDARD.encode(&chunk)));
            chunk.clear();
        }
        chunk.push(c);
    }
    if !chunk.is_empty() || words.is_empty() {
        words.push(format!("=?UTF-8?B?{}?=", STANDARD.encode(&chunk)));
    }
    words.join("\r\n ")
}

/// RFC 2231 percent-encoding of a UTF-8 file name.
fn percent(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&b) { char::from(b).to_string() } else { format!("%{b:02X}") })
        .collect()
}

/// A UTF-16LE string property (its terminating NULs dropped).
fn utf16(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
    String::from_utf16_lossy(&units).trim_end_matches('\0').to_string()
}

/// Bytes in the Windows code page `cpid` (UTF-8 when unknown and valid, else Windows-1252).
fn decode(bytes: &[u8], cpid: Option<u32>) -> String {
    let label = match cpid {
        Some(65001) => "utf-8",
        Some(20127) => "us-ascii",
        Some(28591) => "iso-8859-1",
        Some(n @ 28592..=28599) => return decode_label(bytes, &format!("iso-8859-{}", n - 28590)),
        Some(28605) => "iso-8859-15",
        Some(n @ 1250..=1258) => return decode_label(bytes, &format!("windows-{n}")),
        Some(874) => "windows-874",
        Some(932) => "shift_jis",
        Some(936) => "gbk",
        Some(949) => "euc-kr",
        Some(950) => "big5",
        Some(20866) => "koi8-r",
        Some(21866) => "koi8-u",
        Some(50220..=50222) => "iso-2022-jp",
        Some(51932) => "euc-jp",
        Some(54936) => "gb18030",
        _ => match std::str::from_utf8(bytes) {
            Ok(s) => return s.to_string(),
            Err(_) => "windows-1252",
        },
    };
    decode_label(bytes, label)
}

fn decode_label(bytes: &[u8], label: &str) -> String {
    let encoding = encoding_rs::Encoding::for_label(label.as_bytes()).unwrap_or(encoding_rs::WINDOWS_1252);
    encoding.decode(bytes).0.into_owned()
}

/// A Windows FILETIME (100 ns ticks since 1601) as Unix seconds; `None` for zero or out of range.
fn filetime_to_unix(ft: u64) -> Option<i64> {
    let secs = i64::try_from(ft / 10_000_000).ok()? - 11_644_473_600;
    (ft != 0 && (0..=253_402_300_799).contains(&secs)).then_some(secs)
}

/// The `Date:` of internet headers (unfolded), if it is a valid RFC 2822 date, normalised.
fn header_date(headers: &str) -> Option<String> {
    let mut lines = headers.split("\r\n").flat_map(|l| l.split('\n'));
    while let Some(line) = lines.next() {
        let Some((name, value)) = line.split_once(':') else { continue };
        if !name.trim().eq_ignore_ascii_case("date") || name.starts_with([' ', '\t']) {
            continue;
        }
        let mut value = value.to_string();
        for next in lines.by_ref() {
            if !next.starts_with([' ', '\t']) {
                break;
            }
            value.push(' ');
            value.push_str(next.trim());
        }
        return chrono::DateTime::parse_from_rfc2822(&clean(&value)).ok().map(|d| d.to_rfc2822());
    }
    None
}

/// Unix seconds as an RFC 2822 date in UTC (`Tue, 3 Nov 2026 09:14:22 +0000`).
fn rfc2822(secs: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(secs, 0).map(|t| t.to_rfc2822())
}
