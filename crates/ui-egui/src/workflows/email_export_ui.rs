//! Create a PDF ▸ Export emails to PDF…: pick a folder of email messages (`.eml`, Outlook `.msg`;
//! subfolders included), then a folder for the PDFs. Each message becomes a PDF and each
//! attachment its own file, named by when the message was sent ([`pdfcraft_engine::EmailExport`]).
//! The export runs on its own thread with its own session, so a large folder doesn't hold up the
//! window; progress and a summary show as notifications.

use crate::PdfCraftApp;

/// (messages done, messages in all, the summary once finished).
pub(crate) type EmailExportStatus = std::sync::Arc<std::sync::Mutex<(usize, usize, Option<String>)>>;

/// How many file names a summary lists before saying how many more.
#[cfg(not(target_arch = "wasm32"))]
const LISTED: usize = 5;

impl PdfCraftApp {
    /// Ask for the folder of emails, then for the folder the PDFs go in, then export.
    pub fn export_emails_dialog(&mut self) {
        #[cfg(target_arch = "wasm32")]
        self.notify_tr("Exporting a folder of emails works in the desktop app");
        #[cfg(not(target_arch = "wasm32"))]
        {
            if self.email_export_status.is_some() {
                self.notify_tr("Emails are already being exported");
                return;
            }
            let pick_source = rfd::AsyncFileDialog::new().set_title(tl!("Choose the folder of emails (.eml, .msg) to export"));
            self.ask_one(crate::pickers::Ask::Folder(pick_source), None, |app: &mut Self, source: std::path::PathBuf| {
                let pick_out = rfd::AsyncFileDialog::new().set_title(tl!("Choose a folder for the PDFs"));
                app.ask_one(crate::pickers::Ask::Folder(pick_out), None, move |app: &mut Self, out: std::path::PathBuf| {
                    app.start_email_export(&source, out);
                });
            });
        }
    }

    /// Export every `.eml` and `.msg` in `source` (and its subfolders) into `out`, on a thread.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn start_email_export(&mut self, source: &std::path::Path, out: std::path::PathBuf) {
        if self.email_export_status.is_some() {
            self.notify_tr("Emails are already being exported");
            return;
        }
        let paths = pdfcraft_engine::email_files(source);
        if paths.is_empty() {
            self.notify_fmt("No .eml or .msg files in {dir}", &[("dir", &source.display().to_string())]);
            return;
        }
        let count = paths.len();
        let status: EmailExportStatus = std::sync::Arc::new(std::sync::Mutex::new((0, count, None)));
        let shared = std::sync::Arc::clone(&status);
        let paper = self.session.paper();
        let ctx = self.ctx.clone();
        let spawned = std::thread::Builder::new().name("email-export".into()).spawn(move || {
            let set = |value: (usize, usize, Option<String>)| {
                if let Ok(mut s) = shared.lock() {
                    *s = value;
                }
            };
            // A panic in a conversion ends the export with a message, never the app.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut session = pdfcraft_engine::Session::new();
                session.set_paper(paper);
                let mut write = |path: &std::path::Path, bytes: &[u8]| crate::editing::write_atomically(&path.to_string_lossy(), bytes);
                let mut progress = |done: usize, total: usize| {
                    set((done, total, None));
                    if let Some(ctx) = &ctx {
                        ctx.request_repaint();
                    }
                };
                session.export_email_files(&paths, &out, &mut write, &mut progress)
            }));
            let summary = match result {
                Ok(report) => summary(&report, &out),
                Err(_) => tl!("The email export stopped unexpectedly; the files written so far are in the folder").to_string(),
            };
            set((0, 0, Some(summary)));
            if let Some(ctx) = &ctx {
                ctx.request_repaint();
            }
        });
        match spawned {
            Ok(_) => {
                self.email_export_status = Some(status);
                self.notify_fmt("Exporting {n} emails…", &[("n", &count.to_string())]);
            }
            Err(e) => self.notify_fmt("Couldn't start the export: {e}", &[("e", &e.to_string())]),
        }
    }

    /// Each frame while an export runs: its progress, then its summary.
    pub(crate) fn poll_email_export(&mut self) {
        let Some(status) = self.email_export_status.clone() else { return };
        let snapshot = status.lock().ok().map(|s| s.clone());
        match snapshot {
            Some((_, _, Some(summary))) => {
                self.email_export_status = None;
                self.notify(summary);
            }
            Some((done, total, None)) if done > 0 => {
                self.notify(crate::i18n::fmt(tl!("Exporting emails… {d} of {t}"), &[("d", &done.to_string()), ("t", &total.to_string())]));
            }
            None => {
                // A poisoned lock: the worker panicked while holding it.
                self.email_export_status = None;
                self.notify_tr("The email export stopped unexpectedly");
            }
            _ => {}
        }
        if let Some(ctx) = &self.ctx {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
    }
}

/// What a finished export says: how many messages and files, where, and what needs a look.
#[cfg(not(target_arch = "wasm32"))]
fn summary(report: &pdfcraft_engine::FolderExport, out: &std::path::Path) -> String {
    let list = |items: &[(String, String)]| {
        let mut shown: Vec<String> = items.iter().take(LISTED).map(|(file, why)| format!("{file} ({why})")).collect();
        if items.len() > LISTED {
            shown.push(crate::i18n::fmt(tl!("and {n} more"), &[("n", &(items.len() - LISTED).to_string())]));
        }
        shown.join(", ")
    };
    let mut text = crate::i18n::fmt(
        tl!("Exported {emails} emails as {files} files to {dir}"),
        &[("emails", &report.emails.to_string()), ("files", &report.written.len().to_string()), ("dir", &out.display().to_string())],
    );
    if !report.originals.is_empty() {
        text.push_str(". ");
        text.push_str(&crate::i18n::fmt(tl!("Kept as the original file: {names}"), &[("names", &list(&report.originals))]));
    }
    if !report.failed.is_empty() {
        text.push_str(". ");
        text.push_str(&crate::i18n::fmt(tl!("Left out: {names}"), &[("names", &list(&report.failed))]));
    }
    text
}
