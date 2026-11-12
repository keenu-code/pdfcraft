//! `email_export`: folders of email messages (`.eml`, Outlook `.msg`) as PDFs, with each
//! attachment as its own file, every name starting with the sent date and time.

use serde_json::Value;
#[cfg(not(target_arch = "wasm32"))]
use serde_json::json;

#[cfg(not(target_arch = "wasm32"))]
use crate::ToolError;
#[cfg(not(target_arch = "wasm32"))]
use crate::paths::write_atomic;
use crate::{Args, Automation, Result, failed};

impl Automation {
    /// `email_export` in the web build, which has no folders to read or write.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn email_export(&mut self, _a: &Args) -> Result<Value> {
        Err(failed("email_export works in the desktop app and pdfcraft-cli, which can read and write folders"))
    }

    /// `email_export`: every `.eml` and `.msg` in `paths` (files, or folders searched with
    /// their subfolders) exported into `out_dir`, which is made if missing.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn email_export(&mut self, a: &Args) -> Result<Value> {
        let paths = a.strs("paths")?;
        if paths.is_empty() || paths.len() > 1000 {
            return Err(Args::wrong("paths", "1 to 1000 files or folders"));
        }
        let out = self.resolve(a.str("out_dir")?, true)?;
        let mut files = Vec::new();
        for p in paths {
            let src = self.resolve(p, false)?;
            if src.is_dir() {
                files.extend(pdfcraft_engine::email_files(&src));
            } else {
                files.push(src);
            }
        }
        if files.is_empty() {
            return Err(ToolError::InvalidArgs("paths hold no .eml or .msg files".into()));
        }
        std::fs::create_dir_all(&out).map_err(|e| failed(format!("{}: {e}", out.display())))?;
        let mut write = |path: &std::path::Path, bytes: &[u8]| write_atomic(path, bytes).map_err(|e| std::io::Error::other(e.to_string()));
        let report = self.session.export_email_files(&files, &out, &mut write, &mut |_, _| {});
        let pairs = |list: &[(String, String)]| list.iter().map(|(file, why)| json!({ "file": file, "why": why })).collect::<Vec<_>>();
        Ok(json!({
            "emails": report.emails,
            "written": report.written.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>(),
            "originals": pairs(&report.originals),
            "failed": pairs(&report.failed),
        }))
    }
}
