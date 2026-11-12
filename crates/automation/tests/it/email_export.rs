//! `email_export`: folders of email messages to PDFs, with each attachment its own file and
//! every name starting with the sent time.

use std::path::Path;

use pdfcraft_automation::{Automation, Content, ToolError};
use serde_json::{Value, json};

fn auto(dir: &Path) -> Automation {
    Automation::new().with_root(dir).unwrap().with_clock(|| 1_700_000_000)
}

fn ok(a: &mut Automation, tool: &str, args: Value) -> Value {
    match a.call(tool, &args) {
        Ok(mut c) => match c.remove(0) {
            Content::Json(v) => v,
            other => panic!("{tool}: expected JSON, got {other:?}"),
        },
        Err(e) => panic!("{tool} {args}: {e}"),
    }
}

/// A message sent at `date` with a text attachment `name` holding `text`.
fn eml(subject: &str, date: &str, attachment: Option<(&str, &str)>) -> String {
    let mut out = format!(
        "From: Ada <ada@example.com>\r\nTo: b@example.com\r\nSubject: {subject}\r\nDate: {date}\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"B\"\r\n\r\n--B\r\nContent-Type: text/plain\r\n\r\nHello.\r\n"
    );
    if let Some((name, text)) = attachment {
        out.push_str(&format!(
            "--B\r\nContent-Type: text/plain; name=\"{name}\"\r\nContent-Disposition: attachment; filename=\"{name}\"\r\n\r\n{text}\r\n"
        ));
    }
    out.push_str("--B--\r\n");
    out
}

#[test]
fn email_export_writes_dated_pdfs_and_separate_attachments() {
    let dir = tempfile::tempdir().unwrap();
    let mail = dir.path().join("mail");
    std::fs::create_dir_all(mail.join("2025")).unwrap();
    std::fs::write(mail.join("b.eml"), eml("RE: Second", "Wed, 4 Nov 2026 10:00:00 +0000", Some(("minutes.txt", "Kea minutes")))).unwrap();
    std::fs::write(mail.join("2025/a.EML"), eml("First / draft", "Tue, 3 Nov 2026 10:00:00 +0000", None)).unwrap();
    std::fs::write(mail.join("notes.txt"), "not an email").unwrap();
    std::fs::write(mail.join("broken.msg"), "not an Outlook message").unwrap();
    let mut a = auto(dir.path());

    let made = ok(&mut a, "email_export", json!({ "paths": ["mail"], "out_dir": "out" }));
    assert_eq!(made["emails"], 2);
    let failed = made["failed"].as_array().unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["file"], "broken.msg");
    let mut names: Vec<String> =
        std::fs::read_dir(dir.path().join("out")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    // The local time zone decides the hour; the order and the shape don't depend on it.
    assert_eq!(names.len(), 3, "{names:?}");
    assert!(
        names[0].ends_with(" First draft - 0 Email.pdf")
            && names[1].ends_with(" RE Second - 0 Email.pdf")
            && names[2].ends_with(" RE Second - 1 minutes.pdf"),
        "{names:?}"
    );
    assert!(names.iter().all(|n| n.as_bytes()[4] == b'-' && n.as_bytes()[7] == b'-' && n.as_bytes()[10] == b' '), "{names:?}");
    let minutes = dir.path().join("out").join(&names[2]);
    let doc = ok(&mut a, "doc_open", json!({ "path": minutes.to_string_lossy() }))["doc"].as_u64().unwrap();
    let text = ok(&mut a, "text_extract", json!({ "doc": doc }))["pages"][0]["text"].as_str().unwrap().to_string();
    assert!(text.contains("Kea minutes"), "{text}");

    // Again into the same folder: nothing is replaced, and the message's files are numbered as a
    // group, so they still sort together.
    let again = ok(&mut a, "email_export", json!({ "paths": ["mail/b.eml"], "out_dir": "out" }));
    let written: Vec<&str> = again["written"].as_array().unwrap().iter().map(|p| p.as_str().unwrap()).collect();
    assert!(written[0].ends_with(" RE Second (2) - 0 Email.pdf") && written[1].ends_with(" RE Second (2) - 1 minutes.pdf"), "{written:?}");
    assert_eq!(std::fs::read_dir(dir.path().join("out")).unwrap().count(), 5);
}

#[test]
fn email_export_refuses_what_it_cannot_do() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("empty")).unwrap();
    let mut a = auto(dir.path());
    assert!(
        matches!(a.call("email_export", &json!({ "paths": ["empty"], "out_dir": "out" })), Err(ToolError::InvalidArgs(m)) if m.contains("no .eml or .msg"))
    );
    assert!(matches!(a.call("email_export", &json!({ "paths": [], "out_dir": "out" })), Err(ToolError::InvalidArgs(_))));
    assert!(a.call("email_export", &json!({ "paths": ["empty"] })).is_err());
    // Outside the automation root, for reading or for writing.
    assert!(a.call("email_export", &json!({ "paths": ["/etc"], "out_dir": "out" })).is_err());
    std::fs::write(dir.path().join("empty/m.eml"), eml("s", "Tue, 3 Nov 2026 10:00:00 +0000", None)).unwrap();
    assert!(a.call("email_export", &json!({ "paths": ["empty"], "out_dir": "../escape" })).is_err());
    assert!(!dir.path().join("../escape").exists());
}
