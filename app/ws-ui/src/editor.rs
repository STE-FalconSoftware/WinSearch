//! The preview/edit pane: loads a text/code file, shows it with syntax
//! highlighting, and saves edits with an on-disk-conflict guard.

use crate::highlight;
use std::path::Path;
use std::time::SystemTime;

/// Files larger than this are not loaded into the editor (a TextEdit over many
/// MB is sluggish, and it's rarely what you want to edit in a search tool).
const MAX_PREVIEW_BYTES: u64 = 4 * 1024 * 1024;

/// One open file in the preview pane.
pub struct Editor {
    pub path: String,
    pub name: String,
    pub lang: String,
    original: String,
    pub buffer: String,
    loaded_mtime: Option<SystemTime>,
    /// Content couldn't be decoded losslessly (invalid UTF-8) — show but block saving.
    pub view_only: bool,
    /// A transient status line: (is_error, message).
    pub message: Option<(bool, String)>,
    /// Save is blocked pending the user's decision about an on-disk change.
    conflict: bool,
}

impl Editor {
    /// Attempt to load a file for preview. Returns a human-readable reason on
    /// failure (binary, too large, unreadable) so the caller can show it.
    pub fn load(path: &str) -> Result<Editor, String> {
        let p = Path::new(path);
        let meta = std::fs::metadata(p).map_err(|e| format!("Can't read file: {e}"))?;
        if meta.is_dir() {
            return Err("This is a folder.".into());
        }
        if meta.len() > MAX_PREVIEW_BYTES {
            return Err(format!(
                "File is {:.1} MB — too large to preview (limit {} MB).",
                meta.len() as f64 / 1e6,
                MAX_PREVIEW_BYTES / 1_000_000
            ));
        }
        let bytes = std::fs::read(p).map_err(|e| format!("Can't read file: {e}"))?;
        // Binary sniff: a NUL byte in the first 64 KiB means "not text".
        let sniff = &bytes[..bytes.len().min(64 * 1024)];
        if sniff.contains(&0) {
            return Err("Binary file — no text preview.".into());
        }
        let (text, view_only) = match String::from_utf8(bytes) {
            Ok(s) => (s, false),
            Err(e) => (String::from_utf8_lossy(e.as_bytes()).into_owned(), true),
        };
        let name = p
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(path)
            .to_string();
        Ok(Editor {
            lang: lang_of(&name),
            name,
            path: path.to_string(),
            original: text.clone(),
            buffer: text,
            loaded_mtime: meta.modified().ok(),
            view_only,
            message: if view_only {
                Some((true, "Invalid UTF-8 — shown lossy, saving disabled.".into()))
            } else {
                None
            },
            conflict: false,
        })
    }

    pub fn dirty(&self) -> bool {
        self.buffer != self.original
    }

    fn disk_mtime(&self) -> Option<SystemTime> {
        std::fs::metadata(&self.path)
            .ok()
            .and_then(|m| m.modified().ok())
    }

    /// Save, unless the file changed on disk since we loaded it — in which case
    /// raise the conflict flag and let the user confirm.
    fn attempt_save(&mut self) {
        if self.view_only {
            return;
        }
        let disk = self.disk_mtime();
        if self.loaded_mtime.is_some() && disk != self.loaded_mtime {
            self.conflict = true;
            return;
        }
        self.write();
    }

    fn write(&mut self) {
        match std::fs::write(&self.path, self.buffer.as_bytes()) {
            Ok(()) => {
                self.original = self.buffer.clone();
                self.loaded_mtime = self.disk_mtime();
                self.conflict = false;
                self.message = Some((false, "Saved.".into()));
            }
            Err(e) => self.message = Some((true, format!("Save failed: {e}"))),
        }
    }
}

/// Render the preview/edit panel for `ed`.
pub fn panel(ui: &mut egui::Ui, ed: &mut Editor) {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.heading(&ed.name);
        if ed.dirty() {
            ui.colored_label(egui::Color32::from_rgb(0xE5, 0xC0, 0x7B), "● unsaved");
        }
    });
    ui.label(egui::RichText::new(&ed.path).weak().small());

    let dirty = ed.dirty();
    let mut save_now = false;
    let mut revert = false;
    ui.horizontal(|ui| {
        if ui
            .add_enabled(dirty && !ed.view_only, egui::Button::new("💾 Save"))
            .on_hover_text("Ctrl+S")
            .clicked()
        {
            save_now = true;
        }
        if ui
            .add_enabled(dirty, egui::Button::new("↩ Revert"))
            .clicked()
        {
            revert = true;
        }
        if ed.view_only {
            ui.weak("read-only");
        }
    });

    // On-disk conflict confirmation.
    if ed.conflict {
        egui::Frame::none()
            .fill(egui::Color32::from_rgb(0x5A, 0x2A, 0x2A))
            .inner_margin(6.0)
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(
                        egui::Color32::from_rgb(0xF0, 0xC0, 0xC0),
                        "This file changed on disk since you opened it.",
                    );
                    if ui.button("Overwrite anyway").clicked() {
                        ed.write();
                    }
                    if ui.button("Cancel").clicked() {
                        ed.conflict = false;
                    }
                });
            });
    }

    if let Some((is_err, msg)) = &ed.message {
        let color = if *is_err {
            egui::Color32::from_rgb(0xE0, 0x6C, 0x75)
        } else {
            egui::Color32::from_rgb(0x98, 0xC3, 0x79)
        };
        ui.colored_label(color, msg);
    }

    ui.separator();

    // Ctrl+S anywhere in the panel.
    if !ed.view_only && ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::S)) {
        save_now = true;
    }

    let lang = ed.lang.clone();
    let mut layouter = |ui: &egui::Ui, text: &str, wrap: f32| {
        let mut job = highlight::highlight(ui.ctx(), text, &lang);
        job.wrap.max_width = wrap;
        ui.fonts(|f| f.layout_job(job))
    };

    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add(
                egui::TextEdit::multiline(&mut ed.buffer)
                    .code_editor()
                    .desired_width(f32::INFINITY)
                    .interactive(!ed.view_only)
                    .layouter(&mut layouter),
            );
        });

    if revert {
        ed.buffer = ed.original.clone();
        ed.message = None;
    }
    if save_now {
        ed.attempt_save();
    }
}

/// Map a file name to a highlighter language key.
fn lang_of(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    if lower == "dockerfile" {
        return "dockerfile".into();
    }
    if lower == "makefile" {
        return "makefile".into();
    }
    if lower.starts_with(".gitignore") || lower == ".gitignore" {
        return "gitignore".into();
    }
    Path::new(&lower)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("txt")
        .to_string()
}
