//! Export emails as PDFs with their attachments as separate files. Every file of a message starts
//! with the date and time it was sent and its subject, then a part number: the message is part 0
//! and its attachments follow in the order it lists them (`2026-11-03 0914 RE Quarterly figures -
//! 0 Email.pdf`, `… - 1 logs.pdf`). So a folder sorted by name is in the order the messages were
//! sent, with each message's attachments right after it, in Finder and in plain byte order alike.
//! (Without the part number an attachment sorts before its message: a space sorts before the dot
//! of `.pdf`.) For gathering correspondence into a brief.
//!
//! An attachment PdfCraft can make a PDF of becomes one: a PDF stays as it is; a picture, text,
//! HTML file or attached email is converted; a Word, Excel or PowerPoint file is converted with
//! LibreOffice when it is installed, else (on macOS, Word files) with the system's own reader;
//! anything else, or anything a conversion fails on, is written out as the original file, so
//! nothing is lost.

use std::sync::Arc;

use pdfcraft_create::{EmailFacts, SourceKind};

use crate::{EditError, Session};

/// The most bytes (UTF-8) a subject or an attachment's name may take in a file name: macOS and
/// Linux allow 255 bytes a name, and the date, part number and extension need the rest.
const MAX_SUBJECT: usize = 100;
const MAX_ATTACHMENT: usize = 80;

/// What exporting one message writes: its files, each named `<stem> - <part>`.
#[derive(Debug, Clone)]
pub struct EmailExport {
    /// `YYYY-MM-DD HHMM Subject` ([`dated_stem`]); a folder export numbers it, `Subject (2)`, when
    /// another message already used it.
    pub stem: String,
    pub files: Vec<ExportFile>,
}

/// One file an email export writes.
#[derive(Debug, Clone)]
pub struct ExportFile {
    /// The end of the file name after `<stem> - `: `0 Email.pdf`, `1 logs.pdf`… (the numbers are
    /// zero-padded to one width, so ten or more attachments still sort in order).
    pub part: String,
    pub bytes: Arc<Vec<u8>>,
    /// How the file came about: the email itself, an attachment made into a PDF, or an
    /// attachment written as it was (with why, when a conversion failed).
    pub kind: ExportKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportKind {
    Email,
    Converted,
    Original { reason: Option<String> },
}

impl Session {
    /// The files exporting the email `bytes` (a `.eml` or an Outlook `.msg`, file `name`)
    /// writes: the email as a PDF on the session's paper, then each attachment, in the order the
    /// message lists them. Times are local to this computer ([`dated_stem`]).
    pub fn export_email(&self, name: &str, bytes: &[u8]) -> Result<EmailExport, EditError> {
        let (doc, facts) = pdfcraft_create::email_with_facts(name, bytes, self.paper(), 10.0)?;
        let pdf = self.write_new(&doc)?;
        let EmailFacts { sent, subject, attachments, .. } = facts;
        let stem = dated_stem(sent.map(|s| (s, local_offset_at(s))), subject.as_deref().or_else(|| file_stem(name)));
        let width = attachments.len().to_string().len();
        let mut files = vec![ExportFile { part: format!("{:0width$} Email.pdf", 0), bytes: pdf, kind: ExportKind::Email }];
        for (i, (attachment, data)) in attachments.into_iter().enumerate() {
            let (bytes, kind, ext) = self.attachment_output(&attachment, data);
            let label = clean(file_stem(&attachment).unwrap_or("attachment"), MAX_ATTACHMENT);
            let label = if label.is_empty() { "attachment".to_string() } else { label };
            files.push(ExportFile { part: format!("{:0width$} {label}.{ext}", i + 1), bytes, kind });
        }
        Ok(EmailExport { stem, files })
    }

    /// An attachment as the export writes it: its bytes, how, and the file extension.
    fn attachment_output(&self, name: &str, data: Vec<u8>) -> (Arc<Vec<u8>>, ExportKind, String) {
        let data = Arc::new(data);
        let original = |reason: Option<String>| {
            let ext = name.rsplit_once('.').map(|(_, e)| clean(e, 16)).filter(|e| !e.is_empty() && e.chars().all(|c| c.is_ascii_alphanumeric()));
            (Arc::clone(&data), ExportKind::Original { reason }, ext.unwrap_or_else(|| "bin".into()))
        };
        let converted = match pdfcraft_create::source_kind(name, &data) {
            None => return original(None),
            Some(SourceKind::Pdf) => return (Arc::clone(&data), ExportKind::Converted, "pdf".into()),
            Some(SourceKind::Office) => self.convert_to_pdf(name, &data).map(|(_, pdf)| pdf).or_else(|e| word_fallback(self, name, &data).ok_or(e)),
            Some(_) => self.convert_to_pdf(name, &data).map(|(_, pdf)| pdf),
        };
        match converted {
            Ok(pdf) => (pdf, ExportKind::Converted, "pdf".into()),
            Err(e) => original(Some(e.to_string())),
        }
    }
}

/// A Word file read by macOS's own text system, when LibreOffice couldn't convert it.
#[cfg(target_os = "macos")]
fn word_fallback(session: &Session, name: &str, data: &[u8]) -> Option<Arc<Vec<u8>>> {
    let html = pdfcraft_create::word_to_html_macos(name, data).ok()?;
    session.create_from_html(&format!("{}.html", file_stem(name).unwrap_or("document")), &html).ok()
}

#[cfg(not(target_os = "macos"))]
fn word_fallback(_session: &Session, _name: &str, _data: &[u8]) -> Option<Arc<Vec<u8>>> {
    None
}

/// This computer's offset from UTC, in seconds, at the instant `secs` (so a summer message is
/// named in summer time).
fn local_offset_at(secs: i64) -> i64 {
    #[cfg(not(target_arch = "wasm32"))]
    {
        use chrono::TimeZone;
        chrono::Local.timestamp_opt(secs, 0).earliest().map_or(0, |t| i64::from(chrono::Offset::fix(t.offset()).local_minus_utc()))
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = secs;
        0
    }
}

/// `YYYY-MM-DD HHMM <subject>` for a message sent at `sent` (Unix seconds, with the offset to
/// apply), `undated <subject>` without a time; the subject cleaned for a file name and cut to
/// [`MAX_SUBJECT`] characters, `(no subject)` when there is none.
pub fn dated_stem(sent: Option<(i64, i64)>, subject: Option<&str>) -> String {
    let subject = subject.map(|s| clean(s, MAX_SUBJECT)).filter(|s| !s.is_empty()).unwrap_or_else(|| "(no subject)".into());
    let when = sent
        .and_then(|(secs, offset)| chrono::DateTime::from_timestamp(secs.checked_add(offset)?, 0))
        .map_or_else(|| "undated".to_string(), |t| t.format("%Y-%m-%d %H%M").to_string());
    format!("{when} {subject}")
}

/// `s` made safe as part of a file name: the characters no file system takes (`/ \ : * ? " < > |`)
/// and control characters dropped, whitespace runs collapsed, leading and trailing dots and
/// spaces trimmed, cut to at most `max` bytes at a character boundary.
fn clean(s: &str, max: usize) -> String {
    let kept: String = s.chars().map(|c| if c.is_control() || "/\\:*?\"<>|".contains(c) { ' ' } else { c }).collect();
    let words: Vec<&str> = kept.split_whitespace().collect();
    let mut joined = String::new();
    for c in words.join(" ").chars() {
        if joined.len() + c.len_utf8() > max {
            break;
        }
        joined.push(c);
    }
    joined.trim_matches(|c: char| c == '.' || c.is_whitespace()).to_string()
}

/// A file name's stem (the part before its last dot), if it has one.
fn file_stem(name: &str) -> Option<&str> {
    let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    (!stem.trim().is_empty()).then_some(stem)
}

/// The most folder levels [`email_files`] goes down, and the largest message it reads.
#[cfg(not(target_arch = "wasm32"))]
const MAX_DEPTH: usize = 16;
pub const MAX_EMAIL_BYTES: u64 = 512 * 1024 * 1024;

/// What exporting a folder of emails did.
#[derive(Debug, Clone, Default)]
pub struct FolderExport {
    pub emails: usize,
    /// Files written (emails and attachments), as written.
    pub written: Vec<std::path::PathBuf>,
    /// Attachments written as the original file because a conversion failed: (file, why).
    pub originals: Vec<(String, String)>,
    /// Messages not exported, and files not written: (file, why).
    pub failed: Vec<(String, String)>,
}

/// The `.eml` and `.msg` files in `dir` and its subfolders (at most [`MAX_DEPTH`] deep,
/// symbolic links not followed), sorted by path; folders that can't be read are skipped.
#[cfg(not(target_arch = "wasm32"))]
pub fn email_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    fn walk(dir: &std::path::Path, depth: usize, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
            if meta.is_dir() && depth < MAX_DEPTH {
                walk(&path, depth + 1, out);
            } else if meta.is_file() && path.extension().is_some_and(|e| e.eq_ignore_ascii_case("eml") || e.eq_ignore_ascii_case("msg")) {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, 0, &mut out);
    out.sort();
    out
}

impl Session {
    /// Export each of `paths` into `out_dir` ([`Session::export_email`]), writing every file with
    /// `write` (the caller's atomic write) under a name no file in `out_dir` has yet, and calling
    /// `progress(done, total)` after each message. A message or a file that fails is listed and
    /// the rest go on.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn export_email_files(
        &self,
        paths: &[std::path::PathBuf],
        out_dir: &std::path::Path,
        write: &mut dyn FnMut(&std::path::Path, &[u8]) -> std::io::Result<()>,
        progress: &mut dyn FnMut(usize, usize),
    ) -> FolderExport {
        let mut report = FolderExport::default();
        // The names already in the folder, lower-cased (macOS and Windows compare names without
        // regard to case), and every name written since.
        let mut taken: std::collections::HashSet<String> =
            std::fs::read_dir(out_dir).map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().to_lowercase()).collect()).unwrap_or_default();
        for (i, path) in paths.iter().enumerate() {
            let shown = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
            let export = match std::fs::metadata(path) {
                Ok(meta) if meta.len() > MAX_EMAIL_BYTES => Err(format!("larger than {} MB", MAX_EMAIL_BYTES >> 20)),
                Ok(_) => {
                    std::fs::read(path).map_err(|e| e.to_string()).and_then(|bytes| self.export_email(&shown, &bytes).map_err(|e| e.to_string()))
                }
                Err(e) => Err(e.to_string()),
            };
            match export {
                Err(why) => report.failed.push((shown, why)),
                Ok(EmailExport { stem, files }) => {
                    report.emails += 1;
                    // One stem for all of this message's files, numbered as a whole when another
                    // message (or an earlier export) used it, so its files stay together.
                    let stem = (1..10_000)
                        .map(|n| if n == 1 { stem.clone() } else { format!("{stem} ({n})") })
                        .find(|s| {
                            let prefix = format!("{s} - ").to_lowercase();
                            !taken.iter().any(|t| t.starts_with(&prefix))
                        })
                        .unwrap_or(stem);
                    for file in files {
                        let name = format!("{stem} - {}", file.part);
                        let Some(target) = unused_path(out_dir, &name) else {
                            report.failed.push((name, "no unused file name".into()));
                            continue;
                        };
                        match write(&target, &file.bytes) {
                            Ok(()) => {
                                taken.insert(target.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default());
                                if let ExportKind::Original { reason: Some(why) } = &file.kind {
                                    report.originals.push((name, why.clone()));
                                }
                                report.written.push(target);
                            }
                            Err(e) => report.failed.push((name, e.to_string())),
                        }
                    }
                }
            }
            progress(i + 1, paths.len());
        }
        report
    }
}

/// `dir/name`, or `dir/name (2)`… when a file has it; `None` if `name` would leave `dir` or
/// no number up to 10 000 is free.
#[cfg(not(target_arch = "wasm32"))]
fn unused_path(dir: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
    // Names come from `clean`, but the folder is the user's: check, never assume.
    if name.is_empty() || name.contains(['/', '\\']) || name.starts_with('.') {
        return None;
    }
    let (stem, ext) = name.rsplit_once('.').unwrap_or((name, ""));
    (1..10_000).find_map(|n| {
        let candidate = match (n, ext.is_empty()) {
            (1, _) => name.to_string(),
            (_, true) => format!("{stem} ({n})"),
            (_, false) => format!("{stem} ({n}).{ext}"),
        };
        let path = dir.join(&candidate);
        (path.parent() == Some(dir) && std::fs::symlink_metadata(&path).is_err()).then_some(path)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_start_with_the_local_sent_time_and_are_safe() {
        // 2026-11-02 23:14 UTC is 09:14 on the 3rd in Brisbane (UTC+10).
        assert_eq!(dated_stem(Some((1_793_661_240, 36_000)), Some("RE: Quarterly figures")), "2026-11-03 0914 RE Quarterly figures");
        assert_eq!(dated_stem(None, Some("Minutes")), "undated Minutes");
        assert_eq!(dated_stem(Some((0, 0)), None), "1970-01-01 0000 (no subject)");
        assert_eq!(dated_stem(None, Some("../../etc/passwd")), "undated etc passwd");
        assert_eq!(dated_stem(None, Some("a\r\nb\0c")), "undated a b c");
        assert_eq!(dated_stem(None, Some(" ... ")), "undated (no subject)");
        let long = "x".repeat(300);
        assert_eq!(dated_stem(None, Some(&long)).len(), "undated ".len() + MAX_SUBJECT);
        // Bytes, not characters: a Japanese subject stays well inside a 255-byte file name.
        let japanese = "議事録".repeat(100);
        let stem = dated_stem(None, Some(&japanese));
        assert!(stem.len() <= "undated ".len() + MAX_SUBJECT && stem.ends_with('録'), "{stem}");
        assert_eq!(dated_stem(Some((i64::MAX, 1)), Some("s")), "undated s", "an instant chrono can't hold");
    }

    /// A minimal Outlook message: subject, sent time, plain text, attachments.
    fn msg(subject: &str, sent: Option<i64>, attachments: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write;
        let utf16 = |s: &str| -> Vec<u8> { s.encode_utf16().flat_map(u16::to_le_bytes).collect() };
        let props = |header: usize, entries: &[(u32, u64)]| -> Vec<u8> {
            let mut out = vec![0u8; header];
            for (tag, value) in entries {
                out.extend_from_slice(&tag.to_le_bytes());
                out.extend_from_slice(&6u32.to_le_bytes());
                out.extend_from_slice(&value.to_le_bytes());
            }
            out
        };
        let mut cf = cfb::CompoundFile::create(std::io::Cursor::new(Vec::new())).unwrap();
        let mut put = |path: &str, bytes: &[u8]| cf.create_stream(path).unwrap().write_all(bytes).unwrap();
        let time: Vec<(u32, u64)> = sent.map(|s| (0x0039_0040, u64::try_from(s + 11_644_473_600).unwrap() * 10_000_000)).into_iter().collect();
        put("/__properties_version1.0", &props(32, &time));
        put("/__substg1.0_0037001F", &utf16(subject));
        put("/__substg1.0_1000001F", &utf16("Body."));
        drop(put);
        for (i, (name, bytes)) in attachments.iter().enumerate() {
            let a = format!("/__attach_version1.0_#{i:08X}");
            cf.create_storage(&a).unwrap();
            cf.create_stream(format!("{a}/__properties_version1.0")).unwrap().write_all(&props(8, &[(0x3705_0003, 1)])).unwrap();
            cf.create_stream(format!("{a}/__substg1.0_3707001F")).unwrap().write_all(&utf16(name)).unwrap();
            cf.create_stream(format!("{a}/__substg1.0_37010102")).unwrap().write_all(bytes).unwrap();
        }
        cf.flush().unwrap();
        cf.into_inner().into_inner()
    }

    /// An `.eml` with `attachments` (base64), sent at `sent`.
    fn eml(subject: &str, sent: Option<i64>, attachments: &[(&str, &[u8])]) -> Vec<u8> {
        use base64::Engine as _;
        let mut out = format!("From: a@example.com\r\nSubject: {subject}\r\nMIME-Version: 1.0\r\n");
        if let Some(t) = sent {
            out.push_str(&format!("Date: {}\r\n", chrono::DateTime::from_timestamp(t, 0).unwrap().to_rfc2822()));
        }
        out.push_str("Content-Type: multipart/mixed; boundary=\"B\"\r\n\r\n--B\r\nContent-Type: text/plain\r\n\r\nBody.\r\n");
        for (name, bytes) in attachments {
            let data = base64::engine::general_purpose::STANDARD.encode(bytes);
            out.push_str(&format!("--B\r\nContent-Type: application/octet-stream; name=\"{name}\"\r\nContent-Disposition: attachment; filename=\"{name}\"\r\nContent-Transfer-Encoding: base64\r\n\r\n{data}\r\n"));
        }
        out.push_str("--B--\r\n");
        out.into_bytes()
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pdfcraft-email-export-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// K2/K3: a folder of 200 messages (half Outlook, in subfolders, some undated, some with an
    /// email attached) exports to exactly one file per message and per attachment, and the
    /// names sort in sending order with each message's attachments after it.
    #[test]
    fn a_folder_of_emails_exports_every_message_and_attachment_in_order() {
        let src = temp_dir("src");
        let out = temp_dir("out");
        std::fs::create_dir_all(src.join("sub/deeper")).unwrap();
        let pdf = b"%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj 2 0 obj<</Type/Pages/Kids[]/Count 0>>endobj\ntrailer<</Root 1 0 R>>\n%%EOF";
        let inner = eml("Inner", Some(1_700_000_000), &[]);
        let mut expected = 0usize;
        let base = 1_790_000_000i64;
        for i in 0..200usize {
            let sent = (i % 67 != 5).then(|| base - i64::try_from(i).unwrap() * 3_600);
            let mut atts: Vec<(&str, &[u8])> = Vec::new();
            for k in 0..i % 5 {
                atts.push(match k {
                    0 => ("notes.txt", b"some notes".as_slice()),
                    1 => ("logs.pdf", pdf.as_slice()),
                    2 => ("data.xyz", b"\x00\x01binary".as_slice()),
                    _ => ("forwarded.eml", inner.as_slice()),
                });
            }
            expected += 1 + atts.len();
            let (dir, name, bytes) = match i % 3 {
                0 => (src.clone(), format!("m{i}.eml"), eml(&format!("Subject {i}"), sent, &atts)),
                1 => (src.join("sub"), format!("m{i}.MSG"), msg(&format!("Subject {i}"), sent, &atts)),
                _ => (src.join("sub/deeper"), format!("m{i}.msg"), msg(&format!("Subject {i}"), sent, &atts)),
            };
            std::fs::write(dir.join(name), bytes).unwrap();
        }
        std::fs::write(src.join("ignore.txt"), b"not an email").unwrap();
        // A file already in the folder keeps its name and content.
        std::fs::write(out.join("keep.pdf"), b"mine").unwrap();

        let files = email_files(&src);
        assert_eq!(files.len(), 200);
        let session = Session::new();
        let mut calls = 0;
        let mut write = |p: &std::path::Path, b: &[u8]| std::fs::write(p, b);
        let report = session.export_email_files(&files, &out, &mut write, &mut |done, total| {
            calls += 1;
            assert_eq!(total, 200);
            assert_eq!(done, calls);
        });
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert_eq!(report.emails, 200);
        assert_eq!(report.written.len(), expected, "one file per message and per attachment");
        assert!(report.written.iter().all(|p| p.parent() == Some(out.as_path())));
        assert_eq!(std::fs::read(out.join("keep.pdf")).unwrap(), b"mine");
        let names: Vec<String> = std::fs::read_dir(&out).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names.len(), expected + 1);
        // Name order (plain byte order; Finder's agrees for these names) is sending order: each
        // message's files together, part 0 (the message) first, then its attachments in order,
        // and the dated messages oldest first (a higher index was sent earlier).
        let mut sorted = names.clone();
        sorted.retain(|n| n != "keep.pdf");
        sorted.sort();
        let parsed: Vec<(bool, usize, usize)> = sorted
            .iter()
            .map(|n| {
                let after = n.split("Subject ").nth(1).unwrap();
                let (index, rest) = after.split_once(" - ").unwrap();
                let part = rest.split(' ').next().unwrap();
                (n.starts_with("undated"), index.parse().unwrap(), part.parse().unwrap())
            })
            .collect();
        let mut seen = std::collections::HashSet::new();
        for (k, &(undated, index, part)) in parsed.iter().enumerate() {
            if part == 0 {
                assert!(seen.insert(index), "message {index} appears in two groups");
            } else {
                assert_eq!((parsed[k - 1].1, parsed[k - 1].2), (index, part - 1), "{}", sorted[k]);
            }
            if part == 0 && k > 0 && !undated && !parsed[k - 1].0 {
                assert!(index < parsed[k - 1].1, "{} after {}", sorted[k], sorted[k - 1]);
            }
        }
        assert_eq!(seen.len(), 200);
        assert_eq!(names.iter().filter(|n| n.starts_with("undated ")).count(), 3 + (5..200).step_by(67).map(|i| i % 5).sum::<usize>());
        // Every converted attachment is a PDF; the unknown one is its original bytes.
        assert!(names.iter().any(|n| n.ends_with(" notes.pdf")) && names.iter().any(|n| n.ends_with(" forwarded.pdf")));
        let raw = names.iter().find(|n| n.ends_with(" data.xyz")).unwrap();
        assert_eq!(std::fs::read(out.join(raw)).unwrap(), b"\x00\x01binary");
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&out);
    }
}
