//! XBroadcaster studio window.
//!
//! The window is a native egui shell. It does not embed a browser, which is
//! the cost that makes Streamlabs-style apps heavy at idle.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui::{self, Color32, RichText, Vec2};
use xb_core::{
    load_peak, LiveStatus, Project, SharedState, SourceKind, CHAT_EVERYONE, CHAT_FOLLOWED, CHAT_OFF,
    CHAT_SUBSCRIBERS, CHAT_VERIFIED,
};
use xb_media::{OutputControl, Pipeline};
use xb_x::{
    config_dir, read_json, write_json, AppConfig, Session, StreamSource, XClient, XError,
};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("XBroadcaster\n  xbroadcaster          open the studio\n  xbroadcaster --check  capture one frame and encode it");
        return;
    }
    if args.iter().any(|arg| arg == "--check") {
        let report = xb_media::self_check();
        println!("monitors:");
        for monitor in &report.monitors {
            println!("  {monitor}");
        }
        println!(
            "capture: {} in {:.2} ms",
            if report.captured { "frame" } else { "no new frame" },
            report.capture_ms
        );
        println!(
            "encoder: {} ({}) in {:.2} ms, {} nal bytes",
            report.encoder,
            if report.hardware { "hardware" } else { "software" },
            report.encode_ms,
            report.nal_bytes
        );
        println!("audio: {}", report.audio);
        for note in &report.notes {
            println!("note: {note}");
        }
        let failed = report.notes.iter().any(|note| {
            note.starts_with("encoder:") || note.starts_with("capture open") || note.starts_with("encode:")
        });
        std::process::exit(if failed { 1 } else { 0 });
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([1100.0, 700.0])
            .with_title("XBroadcaster"),
        ..Default::default()
    };
    if let Err(err) = eframe::run_native(
        "XBroadcaster",
        options,
        Box::new(|cc| Ok(Box::new(Studio::new(cc)))),
    ) {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

struct Studio {
    shared: Arc<SharedState>,
    output: Option<OutputControl>,
    pipeline: Option<Pipeline>,
    config: AppConfig,
    session: Option<Session>,
    client: Arc<Mutex<Option<XClient>>>,
    selected: Option<u64>,
    settings: bool,
    chat_input: String,
    chat_log: Vec<String>,
    preview: Option<egui::TextureHandle>,
    preview_gen: u64,
    busy: Arc<AtomicBool>,
    /// Filled by the sign-in thread. The UI thread copies it into `session`.
    pending_session: Arc<Mutex<Option<Session>>>,
}

impl Studio {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        style(cc);
        let dir = config_dir();
        let mut config: AppConfig = read_json(&dir.join("config.json")).unwrap_or_default();
        // Public OAuth client id. It is not a secret. A saved OAuth 1.0 consumer key must not override it.
        config.client_id = option_env!("XB_X_CLIENT_ID")
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .unwrap_or(PUBLIC_CLIENT_ID)
            .to_string();
        let session: Option<Session> = read_json(&dir.join("session.json"));
        let project = read_json::<Project>(&dir.join("project.json")).unwrap_or_default();
        let shared = SharedState::new(project);
        // Capture stays off until someone is signed in.
        let (pipeline, output) = if session.is_some() {
            let pipeline = Pipeline::start(Arc::clone(&shared));
            let output = pipeline.control();
            (Some(pipeline), Some(output))
        } else {
            (None, None)
        };
        let client = match session.clone().map(|session| XClient::new(config.clone(), session)) {
            Some(Ok(client)) => Some(client),
            Some(Err(err)) => {
                shared.set_live(LiveStatus {
                    phase: "Signed out".into(),
                    detail: err.to_string(),
                    ..LiveStatus::default()
                });
                None
            }
            None => None,
        };
        let studio = Self {
            shared,
            output,
            pipeline,
            config,
            session,
            client: Arc::new(Mutex::new(client)),
            selected: None,
            chat_input: String::new(),
            chat_log: Vec::new(),
            preview: None,
            preview_gen: 0,
            busy: Arc::new(AtomicBool::new(false)),
            pending_session: Arc::new(Mutex::new(None)),
            settings: false,
        };
        // Rewrite config so an older file cannot keep an API secret on disk.
        studio.save_config();
        studio
    }

    fn ensure_studio(&mut self) {
        if self.pipeline.is_some() {
            return;
        }
        let pipeline = Pipeline::start(Arc::clone(&self.shared));
        self.output = Some(pipeline.control());
        self.pipeline = Some(pipeline);
    }

    fn save_project(&self) {
        let project = self.shared.lock_project().clone();
        let _ = write_json(&config_dir().join("project.json"), &project);
    }

    fn save_config(&self) {
        let _ = write_json(&config_dir().join("config.json"), &self.config);
    }

    fn save_session(&self) {
        if let Some(session) = &self.session {
            let _ = write_json(&config_dir().join("session.json"), session);
        }
    }
}

impl eframe::App for Studio {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let signed_in = {
            let next = self.pending_session.lock().unwrap_or_else(|err| err.into_inner()).take();
            if let Some(session) = next {
                self.session = Some(session);
                self.settings = false;
                true
            } else {
                false
            }
        };
        if signed_in {
            self.ensure_studio();
        }
        ctx.request_repaint_after(Duration::from_millis(33));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.session.is_none() {
            sign_in_screen(self, ui);
            return;
        }
        let ctx = ui.ctx().clone();
        top_bar(self, ui);
        bottom_bar(self, ui);
        side_scenes(self, ui);
        side_chat(self, ui);
        preview(self, ui);
        if self.settings {
            settings_window(self, &ctx);
        }
    }

    fn on_exit(&mut self) {
        self.shared.shutdown.store(true, Ordering::Relaxed);
        self.save_project();
        self.pipeline.take();
    }
}

fn top_bar(app: &mut Studio, ui: &mut egui::Ui) {
    egui::Panel::top("top").exact_size(56.0).show(ui, |ui| {
        ui.horizontal_centered(|ui| {
            ui.label(RichText::new("XBroadcaster").strong().size(18.0));
            ui.add_space(12.0);
            let mut title = app.shared.lock_project().broadcast.title.clone();
            let response = ui.add(egui::TextEdit::singleline(&mut title).desired_width(360.0).hint_text("Stream title"));
            if response.changed() {
                app.shared.lock_project().broadcast.title = title;
                app.shared.touch_project();
                app.save_project();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let live = app.shared.live_snapshot();
                let busy = app.busy.load(Ordering::Relaxed);
                if live.on_air {
                    if ui.add_enabled(!busy, egui::Button::new(RichText::new("End").color(Color32::WHITE)).fill(RED).min_size(Vec2::new(96.0, 32.0))).clicked() {
                        spawn_end(app);
                    }
                } else if ui
                    .add_enabled(!busy, egui::Button::new(RichText::new("Go live").color(Color32::BLACK).strong()).fill(Color32::WHITE).min_size(Vec2::new(110.0, 32.0)))
                    .clicked()
                {
                    spawn_go_live(app);
                }
                if ui.button("Settings").clicked() {
                    app.settings = true;
                }
                if let Some(name) = app.session.as_ref().map(|session| format!("@{}", session.username)) {
                    ui.label(RichText::new(name).color(TEXT));
                }
            });
        });
    });
}

fn bottom_bar(app: &mut Studio, ui: &mut egui::Ui) {
    egui::Panel::bottom("meters").exact_size(108.0).show(ui, |ui| {
        let stats = app.shared.stats_snapshot();
        let live = app.shared.live_snapshot();
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.set_min_width(280.0);
                meter(ui, "Mic", load_peak(&app.shared.mic_peak_bits), app.shared.lock_project().audio.mic_muted);
                meter(ui, "Desktop", load_peak(&app.shared.desktop_peak_bits), app.shared.lock_project().audio.desktop_muted);
            });
            ui.separator();
            ui.vertical(|ui| {
                let kind = if stats.hardware_encoder { "hardware" } else { "software" };
                ui.label(RichText::new(format!("{}  ·  {}", stats.encoder_name, kind)).color(TEXT));
                ui.label(RichText::new(format!(
                    "capture {:.1} ms   compose {:.1} ms   encode {:.1} ms",
                    stats.capture_ms, stats.compose_ms, stats.encode_ms
                )).color(MUTED));
                ui.label(RichText::new(format!(
                    "{:.0} capture fps   {:.0} output fps   {} dropped   {} idle skips   {}",
                    stats.capture_fps, stats.output_fps, stats.dropped_frames, stats.idle_skips, stats.output
                )).color(MUTED));
                let detail = live.error.clone().unwrap_or(live.detail);
                let color = if live.error.is_some() { RED } else { MUTED };
                ui.label(RichText::new(format!("{}  {}", live.phase, detail)).color(color));
            });
        });
        ui.horizontal(|ui| {
            let mut audio = app.shared.lock_project().audio.clone();
            if ui.checkbox(&mut audio.mic_muted, "Mute mic").changed()
                || ui.add(egui::Slider::new(&mut audio.mic_gain, 0.0..=2.0).text("Mic")).changed()
                || ui.checkbox(&mut audio.desktop_muted, "Mute desktop").changed()
                || ui.add(egui::Slider::new(&mut audio.desktop_gain, 0.0..=2.0).text("Desktop")).changed()
            {
                app.shared.lock_project().audio = audio;
                app.shared.touch_project();
                app.save_project();
            }
        });
    });
}

fn meter(ui: &mut egui::Ui, label: &str, level: f32, muted: bool) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("{label:8}")).color(if muted { MUTED } else { TEXT }));
        let (rect, _) = ui.allocate_exact_size(Vec2::new(180.0, 10.0), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 2.0, Color32::from_rgb(32, 35, 39));
        let width = rect.width() * if muted { 0.0 } else { level.clamp(0.0, 1.0) };
        let fill = if level > 0.9 { RED } else { GREEN };
        painter.rect_filled(egui::Rect::from_min_size(rect.min, Vec2::new(width, rect.height())), 2.0, fill);
    });
}

fn side_scenes(app: &mut Studio, ui: &mut egui::Ui) {
    egui::Panel::left("library").exact_size(280.0).show(ui, |ui| {
        ui.heading("Scenes");
        let names: Vec<(usize, String)> = app
            .shared
            .lock_project()
            .scenes
            .iter()
            .enumerate()
            .map(|(index, scene)| (index, scene.name.clone()))
            .collect();
        let mut active = app.shared.lock_project().active_scene;
        for (index, name) in names {
            if ui.selectable_label(index == active, name).clicked() {
                active = index;
                app.selected = None;
            }
        }
        if active != app.shared.lock_project().active_scene {
            app.shared.lock_project().active_scene = active;
            app.shared.touch_project();
            app.save_project();
        }
        ui.separator();
        ui.heading("Sources");
        let sources: Vec<(u64, String, bool)> = app
            .shared
            .lock_project()
            .active()
            .map(|scene| scene.sources.iter().map(|source| (source.id, source.name.clone(), source.visible)).collect())
            .unwrap_or_default();
        for (id, name, visible) in sources {
            let label = if visible { name } else { format!("{name}  (hidden)") };
            if ui.selectable_label(app.selected == Some(id), label).clicked() {
                app.selected = Some(id);
            }
        }
        ui.horizontal(|ui| {
            if ui.button("Display").clicked() {
                add_source(app, "Display", SourceKind::Display { monitor: 0 });
            }
            if ui.button("Color").clicked() {
                add_source(app, "Color", SourceKind::Color { color: [40, 90, 160, 255] });
            }
            if ui.button("Text").clicked() {
                add_source(
                    app,
                    "Text",
                    SourceKind::Text {
                        text: "Hello X".into(),
                        color: [255, 255, 255, 255],
                        px: 64,
                    },
                );
            }
        });
        ui.separator();
        source_editor(app, ui);
    });
}

fn add_source(app: &mut Studio, name: &str, kind: SourceKind) {
    let scene_id = app.shared.lock_project().active().map(|scene| scene.id);
    let (w, h) = {
        let output = &app.shared.lock_project().output;
        (output.width, output.height)
    };
    if let Some(scene_id) = scene_id {
        let id = app.shared.lock_project().add_source(scene_id, name, kind, 40, 40, w / 2, h / 2);
        app.selected = id;
        app.shared.touch_project();
        app.save_project();
    }
}

fn source_editor(app: &mut Studio, ui: &mut egui::Ui) {
    let Some(id) = app.selected else {
        ui.label(RichText::new("Select a source").color(MUTED));
        return;
    };
    let mut remove = false;
    let mut changed = false;
    {
        let mut project = app.shared.lock_project();
        let Some(source) = project.active_mut().and_then(|scene| scene.sources.iter_mut().find(|source| source.id == id)) else {
            return;
        };
        ui.label(RichText::new(&source.name).strong());
        changed |= ui.checkbox(&mut source.visible, "Visible").changed();
        changed |= ui.add(egui::Slider::new(&mut source.opacity, 0.0..=1.0).text("Opacity")).changed();
        changed |= ui.add(egui::DragValue::new(&mut source.x).prefix("x ")).changed();
        changed |= ui.add(egui::DragValue::new(&mut source.y).prefix("y ")).changed();
        changed |= ui.add(egui::DragValue::new(&mut source.w).prefix("w ")).changed();
        changed |= ui.add(egui::DragValue::new(&mut source.h).prefix("h ")).changed();
        match &mut source.kind {
            SourceKind::Display { monitor } => {
                let monitors = app.shared.stats_snapshot().monitors;
                let label = monitors.get(*monitor).cloned().unwrap_or_else(|| format!("Monitor {monitor}"));
                egui::ComboBox::from_label("Monitor").selected_text(label).show_ui(ui, |ui| {
                    if monitors.is_empty() {
                        changed |= ui.selectable_value(monitor, 0, "Monitor 0").changed();
                    }
                    for (index, name) in monitors.iter().enumerate() {
                        changed |= ui.selectable_value(monitor, index, name).changed();
                    }
                });
            }
            SourceKind::Color { color } => {
                changed |= color_edit(ui, color);
            }
            SourceKind::Text { text, color, px } => {
                changed |= ui.text_edit_multiline(text).changed();
                changed |= ui.add(egui::DragValue::new(px).range(12..=200).prefix("px ")).changed();
                changed |= color_edit(ui, color);
            }
        }
        remove = ui.button("Remove source").clicked();
    }
    if remove {
        app.shared.lock_project().remove_active_source(id);
        app.selected = None;
        changed = true;
    }
    if changed {
        app.shared.touch_project();
        app.save_project();
    }
}

fn color_edit(ui: &mut egui::Ui, bgra: &mut [u8; 4]) -> bool {
    let mut rgb = [bgra[2], bgra[1], bgra[0]];
    let changed = ui.color_edit_button_srgb(&mut rgb).changed();
    if changed {
        *bgra = [rgb[2], rgb[1], rgb[0], 255];
    }
    changed
}

fn side_chat(app: &mut Studio, ui: &mut egui::Ui) {
    egui::Panel::right("chat").exact_size(320.0).show(ui, |ui| {
        ui.heading("Chat");
        let live = app.shared.live_snapshot();
        if let Some(url) = &live.share_url {
            ui.hyperlink_to("Open the broadcast", url);
        }
        ui.label(RichText::new("Messages you send from here post as you. Incoming chat is delivered by the X Activity API webhook, not a poll.").color(MUTED).size(12.0));
        egui::ScrollArea::vertical().stick_to_bottom(true).max_height(ui.available_height() - 80.0).show(ui, |ui| {
            if app.chat_log.is_empty() {
                ui.label(RichText::new("No sent messages yet.").color(MUTED));
            }
            for line in &app.chat_log {
                ui.label(line);
            }
        });
        ui.separator();
        ui.horizontal(|ui| {
            let response = ui.add(egui::TextEdit::singleline(&mut app.chat_input).desired_width(210.0).hint_text("Say something"));
            let send = ui.button("Send").clicked() || (response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
            if send {
                let text = app.chat_input.trim().to_string();
                if !text.is_empty() {
                    if let Some(id) = live.broadcast_id.clone() {
                        let client = Arc::clone(&app.client);
                        let note = text.clone();
                        std::thread::spawn(move || {
                            let result = client.lock().unwrap_or_else(|err| err.into_inner()).as_mut().map(|client| client.send_chat(&id, &note));
                            if let Some(Err(err)) = result {
                                eprintln!("chat: {err}");
                            }
                        });
                        let name = app.session.as_ref().map(|session| session.username.as_str()).unwrap_or("you");
                        app.chat_log.push(format!("@{name}: {text}"));
                        app.chat_input.clear();
                    }
                }
            }
        });
        ui.add_space(8.0);
        chat_option(app, ui);
    });
}

fn chat_option(app: &mut Studio, ui: &mut egui::Ui) {
    let mut option = app.shared.lock_project().broadcast.chat_option;
    let mut changed = false;
    egui::ComboBox::from_label("Who can chat").selected_text(chat_label(option)).show_ui(ui, |ui| {
        for value in [CHAT_EVERYONE, CHAT_VERIFIED, CHAT_FOLLOWED, CHAT_SUBSCRIBERS, CHAT_OFF] {
            changed |= ui.selectable_value(&mut option, value, chat_label(value)).changed();
        }
    });
    if changed {
        app.shared.lock_project().broadcast.chat_option = option;
        app.save_project();
    }
}

fn chat_label(option: u8) -> &'static str {
    match option {
        CHAT_OFF => "Chat off",
        CHAT_EVERYONE => "Everyone",
        CHAT_VERIFIED => "Verified",
        CHAT_FOLLOWED => "People you follow",
        CHAT_SUBSCRIBERS => "Subscribers",
        _ => "Verified",
    }
}

fn preview(app: &mut Studio, ui: &mut egui::Ui) {
    egui::CentralPanel::default().show(ui, |ui| {
        let frame = app.shared.preview.lock().unwrap_or_else(|err| err.into_inner()).clone();
        if frame.generation != app.preview_gen && frame.width > 0 && !frame.bgra.is_empty() {
            let image = egui::ColorImage::from_rgba_unmultiplied([frame.width as usize, frame.height as usize], &frame.bgra);
            match &mut app.preview {
                Some(texture) => texture.set(image, egui::TextureOptions::LINEAR),
                None => {
                    app.preview = Some(ui.ctx().load_texture("program", image, egui::TextureOptions::LINEAR));
                }
            }
            app.preview_gen = frame.generation;
        }
        let available = ui.available_size();
        let aspect = 16.0 / 9.0;
        let mut size = available;
        if size.x / size.y > aspect {
            size.x = size.y * aspect;
        } else {
            size.y = size.x / aspect;
        }
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("PROGRAM").color(MUTED).size(11.0));
            if let Some(texture) = &app.preview {
                ui.add(egui::Image::new(texture).fit_to_exact_size(size));
            } else {
                let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
                ui.painter().rect_filled(rect, 4.0, Color32::BLACK);
                ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, "Waiting for a frame", egui::FontId::proportional(16.0), MUTED);
            }
        });
    });
}

fn sign_in_screen(app: &mut Studio, ui: &mut egui::Ui) {
    let full = ui.available_rect_before_wrap();
    ui.painter().rect_filled(full, 0.0, BG);
    let card = egui::Rect::from_center_size(full.center(), Vec2::new(420.0, 420.0));
    ui.painter().rect_filled(card, 16.0, Color32::from_rgb(20, 22, 25));
    ui.painter().rect_stroke(card, 16.0, egui::Stroke::new(1.0, LINE), egui::StrokeKind::Inside);
    ui.scope_builder(egui::UiBuilder::default().max_rect(card.shrink(32.0)), |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(28.0);
            let (mark, _) = ui.allocate_exact_size(Vec2::splat(64.0), egui::Sense::hover());
            ui.painter().rect_filled(mark, 16.0, Color32::WHITE);
            ui.painter().text(
                mark.center(),
                egui::Align2::CENTER_CENTER,
                "X",
                egui::FontId::proportional(32.0),
                Color32::BLACK,
            );
            ui.add_space(20.0);
            ui.label(RichText::new("XBroadcaster").size(28.0).strong().color(TEXT));
            ui.add_space(8.0);
            ui.label(RichText::new("Sign in with your X account").size(15.0).color(MUTED));
            ui.add_space(28.0);
            let busy = app.busy.load(Ordering::Relaxed);
            let label = if busy { "Waiting for X…" } else { "Sign in with X" };
            let button = egui::Button::new(RichText::new(label).size(16.0).strong().color(Color32::BLACK))
                .fill(Color32::WHITE)
                .min_size(Vec2::new(320.0, 48.0));
            if ui.add_enabled(!busy, button).clicked() {
                spawn_sign_in(app);
            }
            ui.add_space(16.0);
            let live = app.shared.live_snapshot();
            if let Some(err) = live.error {
                ui.label(RichText::new(err).color(RED));
            } else if busy {
                ui.label(RichText::new("Approve access in the browser, then return here.").color(MUTED));
            } else if app.config.client_id.is_empty() {
                ui.label(RichText::new("This build is missing its public X app id.").color(RED));
            }
        });
    });
}

fn settings_window(app: &mut Studio, ctx: &egui::Context) {
    let mut open = app.settings;
    egui::Window::new("X account").open(&mut open).resizable(false).show(ctx, |ui| {
        if let Some(session) = &app.session {
            ui.label(RichText::new(format!("@{}", session.username)).strong());
            if !session.name.is_empty() {
                ui.label(RichText::new(&session.name).color(MUTED));
            }
        }
        ui.label(RichText::new("This account is yours. The download does not include an API secret.").color(MUTED));
        if ui.button("Sign out").clicked() {
            sign_out(app);
        }
        ui.separator();
        let mut low_latency = app.shared.lock_project().broadcast.low_latency;
        let mut post = app.shared.lock_project().broadcast.post_announcement;
        if ui.checkbox(&mut low_latency, "Low latency").changed() {
            app.shared.lock_project().broadcast.low_latency = low_latency;
            app.save_project();
        }
        if ui.checkbox(&mut post, "Post an announcement").changed() {
            app.shared.lock_project().broadcast.post_announcement = post;
            app.save_project();
        }
        ui.label(RichText::new("Output is 1280x720, 30 fps, 4 Mbps H.264, 44.1 kHz AAC. That is the configuration X recommends for a source.").color(MUTED));
    });
    app.settings = open;
}

fn sign_out(app: &mut Studio) {
    if let Some(output) = &app.output {
        output.stop_output();
    }
    app.session = None;
    *app.client.lock().unwrap_or_else(|err| err.into_inner()) = None;
    let _ = std::fs::remove_file(config_dir().join("session.json"));
    app.pipeline.take();
    app.output = None;
    app.shared.shutdown.store(false, Ordering::Relaxed);
    app.shared.set_live(LiveStatus::default());
    app.settings = false;
    app.busy.store(false, Ordering::Relaxed);
}

fn spawn_sign_in(app: &mut Studio) {
    if app.busy.swap(true, Ordering::Relaxed) {
        return;
    }
    app.save_config();
    let config = app.config.clone();
    let client_slot = Arc::clone(&app.client);
    let shared = Arc::clone(&app.shared);
    let busy = Arc::clone(&app.busy);
    let pending = Arc::clone(&app.pending_session);
    std::thread::spawn(move || {
        shared.set_live(LiveStatus {
            phase: "Signing in".into(),
            detail: "Approve XBroadcaster in the browser.".into(),
            ..LiveStatus::default()
        });
        match xb_x::sign_in(&config) {
            Ok(session) => {
                let _ = write_json(&config_dir().join("session.json"), &session);
                *pending.lock().unwrap_or_else(|err| err.into_inner()) = Some(session.clone());
                match XClient::new(config, session.clone()) {
                    Ok(client) => {
                        *client_slot.lock().unwrap_or_else(|err| err.into_inner()) = Some(client);
                        shared.set_live(LiveStatus {
                            phase: "Ready".into(),
                            detail: format!("Signed in as @{}", session.username),
                            ..LiveStatus::default()
                        });
                    }
                    Err(err) => set_error(&shared, err),
                }
            }
            Err(err) => set_error(&shared, err),
        }
        busy.store(false, Ordering::Relaxed);
    });
}

fn spawn_go_live(app: &mut Studio) {
    if app.busy.swap(true, Ordering::Relaxed) {
        return;
    }
    app.save_project();
    let shared = Arc::clone(&app.shared);
    let client_slot = Arc::clone(&app.client);
    let Some(output) = app.output.clone() else {
        app.busy.store(false, Ordering::Relaxed);
        return;
    };
    let busy = Arc::clone(&app.busy);
    std::thread::spawn(move || {
        let result = go_live(&shared, &client_slot, &output);
        if let Err(err) = result {
            output.stop_output();
            shared.output_connected.store(false, Ordering::Relaxed);
            set_error(&shared, err);
        }
        busy.store(false, Ordering::Relaxed);
    });
}

fn go_live(shared: &SharedState, client_slot: &Mutex<Option<XClient>>, output: &OutputControl) -> Result<(), XError> {
    let mut guard = client_slot.lock().unwrap_or_else(|err| err.into_inner());
    let client = guard.as_mut().ok_or_else(|| XError::message("Sign in with X first."))?;
    set_phase(shared, "Preparing", "Checking the X account.");
    client.ensure_fresh()?;
    let project = shared.lock_project().clone();
    let region = match project.broadcast.saved_region.clone() {
        Some(region) => region,
        None => {
            set_phase(shared, "Preparing", "Asking X for the closest ingest region.");
            client.recommended_region()?
        }
    };
    set_phase(shared, "Preparing", "Reusing the saved stream source.");
    let source = ensure_source(client, &project.broadcast.source_name, &region, project.broadcast.saved_source_id.as_deref())?;
    {
        let mut project = shared.lock_project();
        project.broadcast.saved_source_id = Some(source.id.clone());
        project.broadcast.saved_region = Some(source.region.clone());
    }
    let _ = write_json(&config_dir().join("project.json"), &*shared.lock_project());
    if source.rtmps_url.is_empty() {
        return Err(XError::message("X did not return an RTMPS URL for this source."));
    }
    set_phase(shared, "Encoder", "Connecting the hardware encoder to X.");
    *shared.output_error.lock().unwrap_or_else(|err| err.into_inner()) = None;
    shared.output_connected.store(false, Ordering::Relaxed);
    output.start_output(source.rtmps_url.clone(), source.stream_key.clone());
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        if shared.output_connected.load(Ordering::Relaxed) {
            break;
        }
        if let Some(err) = shared.output_error.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            return Err(XError::message(err));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if !shared.output_connected.load(Ordering::Relaxed) {
        return Err(XError::message("The encoder did not connect to the ingest in time."));
    }
    set_phase(shared, "Ingest", "Waiting until X sees the video.");
    let active_deadline = std::time::Instant::now() + Duration::from_secs(25);
    loop {
        let current = client.get_source(&source.id)?;
        if current.active {
            break;
        }
        if std::time::Instant::now() > active_deadline {
            return Err(XError::message("X has not marked the stream active yet. The ingest may still be negotiating."));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    set_phase(shared, "Creating", "Creating the broadcast.");
    let broadcast = client.create_broadcast(&source.id, &source.region, project.broadcast.low_latency)?;
    set_phase(shared, "Publishing", "Publishing to followers.");
    client.publish(
        &broadcast.id,
        &project.broadcast.title,
        project.broadcast.chat_option,
        project.broadcast.post_announcement,
    )?;
    shared.set_live(LiveStatus {
        phase: "Live".into(),
        detail: broadcast.share_url.clone().unwrap_or_else(|| "On air".into()),
        share_url: broadcast.share_url,
        broadcast_id: Some(broadcast.id),
        on_air: true,
        error: None,
    });
    Ok(())
}

fn ensure_source(client: &mut XClient, name: &str, region: &str, saved: Option<&str>) -> Result<StreamSource, XError> {
    if let Some(id) = saved {
        if let Ok(source) = client.get_source(id) {
            if source.region == region && !source.rtmps_url.is_empty() {
                return Ok(source);
            }
        }
    }
    if let Ok(sources) = client.list_sources() {
        if let Some(source) = sources.into_iter().find(|source| source.region == region && !source.rtmps_url.is_empty()) {
            return Ok(source);
        }
    }
    client.create_source(name, region)
}

fn spawn_end(app: &mut Studio) {
    if app.busy.swap(true, Ordering::Relaxed) {
        return;
    }
    let id = app.shared.live_snapshot().broadcast_id;
    if let Some(output) = &app.output {
        output.stop_output();
    }
    let shared = Arc::clone(&app.shared);
    let client_slot = Arc::clone(&app.client);
    let busy = Arc::clone(&app.busy);
    std::thread::spawn(move || {
        if let Some(id) = id {
            let mut guard = client_slot.lock().unwrap_or_else(|err| err.into_inner());
            if let Some(client) = guard.as_mut() {
                if let Err(err) = client.end(&id) {
                    shared.output_connected.store(false, Ordering::Relaxed);
                    shared.set_live(LiveStatus {
                        phase: "Ended".into(),
                        detail: format!("Encoder stopped. X did not confirm the end: {err}"),
                        error: Some(err.to_string()),
                        on_air: false,
                        ..LiveStatus::default()
                    });
                    busy.store(false, Ordering::Relaxed);
                    return;
                }
            }
        }
        shared.set_live(LiveStatus {
            phase: "Ended".into(),
            detail: "The broadcast is off the air.".into(),
            ..LiveStatus::default()
        });
        busy.store(false, Ordering::Relaxed);
    });
}

fn set_phase(shared: &SharedState, phase: &str, detail: &str) {
    shared.set_live(LiveStatus {
        phase: phase.into(),
        detail: detail.into(),
        ..LiveStatus::default()
    });
}

fn set_error(shared: &SharedState, err: XError) {
    shared.set_live(LiveStatus {
        phase: "Not live".into(),
        detail: err.to_string(),
        error: Some(err.to_string()),
        ..LiveStatus::default()
    });
}

fn style(cc: &eframe::CreationContext<'_>) {
    let mut visuals = egui::Visuals::dark();
    visuals.window_fill = BG;
    visuals.panel_fill = PANEL;
    visuals.extreme_bg_color = Color32::from_rgb(8, 9, 10);
    visuals.faint_bg_color = PANEL;
    visuals.code_bg_color = Color32::from_rgb(8, 9, 10);
    visuals.widgets.noninteractive.bg_fill = PANEL;
    visuals.widgets.inactive.bg_fill = PANEL_2;
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(36, 40, 45);
    visuals.widgets.active.bg_fill = Color32::from_rgb(46, 51, 57);
    visuals.widgets.noninteractive.fg_stroke.color = TEXT;
    visuals.widgets.inactive.fg_stroke.color = TEXT;
    visuals.selection.bg_fill = BLUE;
    visuals.hyperlink_color = BLUE;
    visuals.window_stroke = egui::Stroke::new(1.0, LINE);
    cc.egui_ctx.set_visuals(visuals);
}

/// OAuth 2.0 client id for the XBroadcaster app. Visible during sign-in. Not an account key.
const PUBLIC_CLIENT_ID: &str = "T1lwcVFRUlZ4LTZsUjZ6aUdOVWM6MTpjaQ";

const BG: Color32 = Color32::from_rgb(12, 14, 16);
const PANEL: Color32 = Color32::from_rgb(18, 20, 23);
const PANEL_2: Color32 = Color32::from_rgb(28, 32, 36);
const LINE: Color32 = Color32::from_rgb(47, 51, 54);
const TEXT: Color32 = Color32::from_rgb(231, 233, 234);
const MUTED: Color32 = Color32::from_rgb(113, 118, 123);
const BLUE: Color32 = Color32::from_rgb(29, 155, 240);
const RED: Color32 = Color32::from_rgb(244, 33, 46);
const GREEN: Color32 = Color32::from_rgb(0, 186, 124);
