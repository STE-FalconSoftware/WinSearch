// Hide the console window on Windows release builds; keep it in debug for logs.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod editor;
mod highlight;
mod tray;
mod worker;

use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;
use worker::{Req, Results, Shared, SortKey};

fn main() -> eframe::Result<()> {
    let letters = discover_letters();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 640.0])
            .with_min_inner_size([560.0, 320.0])
            .with_title("WinSearch"),
        ..Default::default()
    };

    eframe::run_native(
        "WinSearch",
        options,
        Box::new(move |cc| {
            let shared = Shared::new();
            let ctx = cc.egui_ctx.clone();
            worker::spawn_indexer(shared.clone(), ctx.clone(), letters.clone());
            let (tx, rx) = std::sync::mpsc::channel::<Req>();
            worker::spawn_search(shared.clone(), ctx, rx);
            // Tray + global hotkey are created on the event-loop thread (here).
            let tray = tray::Tray::setup();
            Ok(Box::new(App::new(shared, tx, tray)))
        }),
    )
}

struct App {
    shared: Arc<Shared>,
    tx: Sender<Req>,
    query: String,
    sort: SortKey,
    ascending: bool,
    focused_once: bool,
    was_indexing: bool,
    was_meta_done: bool,
    tray: Option<tray::Tray>,
    allow_close: bool,
    selected: Option<usize>,
    editor: Option<editor::Editor>,
    /// Shown in the preview pane when a file can't be edited (binary, too big…).
    preview_msg: Option<(String, String)>,
    /// A pending file switch (path, name) awaiting "discard unsaved changes?".
    confirm_switch: Option<(String, String)>,
    /// Closing the pane with unsaved edits awaits confirmation.
    confirm_close: bool,
}

impl App {
    fn new(shared: Arc<Shared>, tx: Sender<Req>, tray: Option<tray::Tray>) -> Self {
        App {
            shared,
            tx,
            query: String::new(),
            sort: SortKey::Name,
            ascending: true,
            focused_once: false,
            was_indexing: true,
            was_meta_done: false,
            tray,
            allow_close: false,
            selected: None,
            editor: None,
            preview_msg: None,
            confirm_switch: None,
            confirm_close: false,
        }
    }

    /// Open a file in the preview pane, guarding unsaved edits in the current one.
    fn open_preview(&mut self, path: &str, name: &str) {
        if let Some(ed) = &self.editor {
            if ed.path == path {
                return; // already open
            }
            if ed.dirty() {
                self.confirm_switch = Some((path.to_string(), name.to_string()));
                return;
            }
        }
        self.force_open_preview(path);
    }

    fn force_open_preview(&mut self, path: &str) {
        self.confirm_switch = None;
        match editor::Editor::load(path) {
            Ok(ed) => {
                self.editor = Some(ed);
                self.preview_msg = None;
            }
            Err(reason) => {
                let name = std::path::Path::new(path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(path)
                    .to_string();
                self.editor = None;
                self.preview_msg = Some((name, reason));
            }
        }
    }

    /// Publish the current query to the search worker.
    fn submit(&self) {
        let gen = self.shared.generation.fetch_add(1, Ordering::Relaxed) + 1;
        self.shared.cancel.store(true, Ordering::Relaxed);
        let _ = self.tx.send(Req {
            gen,
            query: self.query.clone(),
            sort: self.sort,
            ascending: self.ascending,
        });
    }

    fn set_sort(&mut self, key: SortKey) {
        if self.sort == key {
            self.ascending = !self.ascending;
        } else {
            self.sort = key;
            self.ascending = matches!(key, SortKey::Name | SortKey::Path);
        }
        self.submit();
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Tray + global hotkey. When a tray is active we keep a slow repaint
        // ticking so hotkey/tray events are still handled while hidden.
        if let Some(tray) = &self.tray {
            let poll = tray.poll();
            if poll.show {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                self.focused_once = false; // refocus the search box
            }
            if poll.quit {
                self.allow_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            // Closing the window hides it to the tray instead of quitting.
            if ctx.input(|i| i.viewport().close_requested()) && !self.allow_close {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
            ctx.request_repaint_after(Duration::from_millis(200));
        }

        // Re-run the current query when live updates arrive, or when the index
        // (or its metadata) finishes so results reflect newly-available data.
        let indexing = self.shared.indexing.load(Ordering::Relaxed);
        let meta_done = self.shared.meta_done.load(Ordering::Relaxed);
        let became_ready = self.was_indexing && !indexing;
        let meta_became_ready = !self.was_meta_done && meta_done;
        self.was_indexing = indexing;
        self.was_meta_done = meta_done;
        if (self.shared.dirty.swap(false, Ordering::Relaxed) || became_ready || meta_became_ready)
            && !self.query.is_empty() {
                self.submit();
            }

        egui::TopBottomPanel::top("search").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("🔍").size(18.0));
                let resp = ui.add_sized(
                    [ui.available_width(), 28.0],
                    egui::TextEdit::singleline(&mut self.query).hint_text(
                        "Search files…  try:  report *.pdf   ext:log   size:>100mb   dm:today",
                    ),
                );
                if !self.focused_once {
                    resp.request_focus();
                    self.focused_once = true;
                }
                if resp.changed() {
                    self.submit();
                }
            });
            ui.add_space(6.0);
        });

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.add_space(3.0);
            ui.horizontal(|ui| {
                let status = self.shared.status.lock().clone();
                if self.shared.indexing.load(Ordering::Relaxed) {
                    ui.add(egui::Spinner::new());
                    ui.label(&status);
                } else {
                    let res = self.shared.results.lock();
                    let count = if res.truncated {
                        format!("{}+ matches", res.total)
                    } else {
                        format!("{} matches", res.total)
                    };
                    ui.label(format!("{}  ·  {:.1} ms", count, res.elapsed_ms));
                    ui.separator();
                    ui.weak(&status);
                    if !self.shared.meta_done.load(Ordering::Relaxed) {
                        ui.separator();
                        ui.add(egui::Spinner::new().size(12.0));
                        ui.weak("indexing sizes/dates…");
                    }
                }
            });
            ui.add_space(3.0);
        });

        // Right-hand preview / editor pane.
        let mut close_preview = false;
        if self.editor.is_some() || self.preview_msg.is_some() {
            egui::SidePanel::right("preview")
                .resizable(true)
                .default_width(560.0)
                .min_width(320.0)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("✖ Close").clicked() {
                            close_preview = true;
                        }
                    });
                    if let Some(ed) = &mut self.editor {
                        editor::panel(ui, ed);
                    } else if let Some((name, reason)) = &self.preview_msg {
                        ui.add_space(10.0);
                        ui.heading(name);
                        ui.add_space(6.0);
                        ui.weak(reason);
                    }
                });
        }

        // Results table (central).
        let mut action = TableAction::default();
        let meta_done = self.shared.meta_done.load(Ordering::Relaxed);
        egui::CentralPanel::default().show(ctx, |ui| {
            let res = self.shared.results.lock();
            if res.needs_meta && !meta_done && res.total == 0 {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(14.0));
                    ui.weak("Size/date filters activate once background indexing of sizes and dates finishes.");
                });
            }
            action = draw_table(ui, &res, self.sort, self.ascending, self.selected, &self.query);
        });

        // Modal: discard-unsaved confirmation for switching or closing.
        self.confirmation_modals(ctx);

        // Apply table actions.
        if let Some(k) = action.sort {
            self.set_sort(k);
        }
        if let Some(idx) = action.open {
            if let Some(row) = self.row_path(idx) {
                let _ = open::that(&row);
            }
        }
        if let Some(idx) = action.select {
            self.selected = Some(idx);
            if let Some((path, name)) = self.row_path_name(idx) {
                self.open_preview(&path, &name);
            }
        }
        if close_preview {
            if self.editor.as_ref().map(|e| e.dirty()).unwrap_or(false) {
                self.confirm_close = true;
            } else {
                self.editor = None;
                self.preview_msg = None;
                self.selected = None;
            }
        }
    }
}

impl App {
    fn row_path(&self, idx: usize) -> Option<String> {
        let res = self.shared.results.lock();
        res.rows.get(idx).map(|r| r.path.clone())
    }

    fn row_path_name(&self, idx: usize) -> Option<(String, String)> {
        let res = self.shared.results.lock();
        res.rows.get(idx).map(|r| (r.path.clone(), r.name.clone()))
    }

    /// Render the "discard unsaved changes?" prompts for switching files or
    /// closing the pane.
    fn confirmation_modals(&mut self, ctx: &egui::Context) {
        if let Some((path, _name)) = self.confirm_switch.clone() {
            let mut discard = false;
            let mut cancel = false;
            egui::Window::new("Unsaved changes")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label("You have unsaved edits. Discard them and open the other file?");
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("Discard & open").clicked() {
                            discard = true;
                        }
                        if ui.button("Keep editing").clicked() {
                            cancel = true;
                        }
                    });
                });
            if discard {
                self.force_open_preview(&path);
            } else if cancel {
                self.confirm_switch = None;
            }
        }

        if self.confirm_close {
            let mut discard = false;
            let mut cancel = false;
            egui::Window::new("Unsaved changes")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label("You have unsaved edits. Discard them and close the pane?");
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("Discard & close").clicked() {
                            discard = true;
                        }
                        if ui.button("Keep editing").clicked() {
                            cancel = true;
                        }
                    });
                });
            if discard {
                self.confirm_close = false;
                self.editor = None;
                self.preview_msg = None;
                self.selected = None;
            } else if cancel {
                self.confirm_close = false;
            }
        }
    }
}

/// What the user did in the results table this frame.
#[derive(Default)]
struct TableAction {
    sort: Option<SortKey>,
    select: Option<usize>,
    open: Option<usize>,
}

/// Draw the results table and report any header click, row selection, or open.
fn draw_table(
    ui: &mut egui::Ui,
    res: &Results,
    sort: SortKey,
    ascending: bool,
    selected: Option<usize>,
    query: &str,
) -> TableAction {
    use egui_extras::{Column, TableBuilder};

    let mut action = TableAction::default();
    if query.is_empty() {
        ui.centered_and_justified(|ui| {
            ui.weak("Type to search. Everything on your NTFS drives is indexed in memory.");
        });
        return action;
    }

    let mut clicked: Option<SortKey> = None;
    let header = |ui: &mut egui::Ui, label: &str, key: SortKey| -> bool {
        let arrow = if sort == key {
            if ascending {
                " ▲"
            } else {
                " ▼"
            }
        } else {
            ""
        };
        ui.add(
            egui::Label::new(egui::RichText::new(format!("{}{}", label, arrow)).strong())
                .sense(egui::Sense::click()),
        )
        .clicked()
    };

    TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
        .column(Column::auto().at_least(160.0).clip(true))
        .column(Column::remainder().at_least(200.0).clip(true))
        .column(Column::auto().at_least(80.0))
        .column(Column::auto().at_least(140.0))
        .header(22.0, |mut h| {
            h.col(|ui| {
                if header(ui, "Name", SortKey::Name) {
                    clicked = Some(SortKey::Name);
                }
            });
            h.col(|ui| {
                if header(ui, "Path", SortKey::Path) {
                    clicked = Some(SortKey::Path);
                }
            });
            h.col(|ui| {
                if header(ui, "Size", SortKey::Size) {
                    clicked = Some(SortKey::Size);
                }
            });
            h.col(|ui| {
                if header(ui, "Date modified", SortKey::Modified) {
                    clicked = Some(SortKey::Modified);
                }
            });
        })
        .body(|body| {
            body.rows(20.0, res.rows.len(), |mut row| {
                let idx = row.index();
                let r = &res.rows[idx];
                row.set_selected(selected == Some(idx));
                row.col(|ui| {
                    let icon = if r.is_dir { "📁 " } else { "📄 " };
                    let resp = ui
                        .add(
                            egui::Label::new(format!("{}{}", icon, r.name))
                                .sense(egui::Sense::click()),
                        )
                        .on_hover_text(&r.path);
                    if resp.clicked() {
                        action.select = Some(idx);
                    }
                    if resp.double_clicked() {
                        action.open = Some(idx);
                    }
                    row_context_menu(&resp, r);
                });
                row.col(|ui| {
                    ui.label(parent_of(&r.path));
                });
                row.col(|ui| {
                    ui.label(fmt_size(r.size, r.is_dir));
                });
                row.col(|ui| {
                    ui.label(fmt_time(r.mtime));
                });
            });
        });

    action.sort = clicked;
    action
}

fn row_context_menu(resp: &egui::Response, r: &worker::Row) {
    resp.context_menu(|ui| {
        if ui.button("Open").clicked() {
            let _ = open::that(&r.path);
            ui.close_menu();
        }
        if ui.button("Open containing folder").clicked() {
            reveal(&r.path);
            ui.close_menu();
        }
        if ui.button("Copy full path").clicked() {
            ui.output_mut(|o| o.copied_text = r.path.clone());
            ui.close_menu();
        }
    });
}

fn reveal(path: &str) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("explorer")
            .arg(format!("/select,{}", path))
            .spawn();
    }
    #[cfg(not(windows))]
    {
        if let Some(parent) = std::path::Path::new(path).parent() {
            let _ = open::that(parent);
        }
    }
}

fn parent_of(path: &str) -> &str {
    match path.rfind('\\') {
        Some(i) => &path[..i],
        None => path,
    }
}

fn discover_letters() -> Vec<char> {
    #[cfg(windows)]
    {
        let v = ws_index::win::ntfs_volumes();
        if v.is_empty() {
            vec!['C']
        } else {
            v
        }
    }
    #[cfg(not(windows))]
    {
        vec!['C']
    }
}

fn fmt_size(bytes: u64, is_dir: bool) -> String {
    if is_dir {
        return String::new();
    }
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut b = bytes as f64;
    let mut i = 0;
    while b >= 1024.0 && i < 4 {
        b /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} B", bytes)
    } else {
        format!("{:.1} {}", b, U[i])
    }
}

/// Format a Windows FILETIME (100 ns since 1601) as `YYYY-MM-DD HH:MM`.
fn fmt_time(ft: i64) -> String {
    if ft <= 0 {
        return String::new();
    }
    const TICKS_PER_SEC: i64 = 10_000_000;
    const EPOCH_DIFF: i64 = 11_644_473_600;
    let secs = ft / TICKS_PER_SEC - EPOCH_DIFF;
    if secs < 0 {
        return String::new();
    }
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        y,
        m,
        d,
        tod / 3600,
        (tod % 3600) / 60
    )
}

/// Inverse of days_from_civil (Howard Hinnant).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
