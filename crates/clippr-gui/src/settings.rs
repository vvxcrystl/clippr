//! The Settings tab. Edits apply automatically after a short pause, so dragging a spin button
//! doesn't restart the capture pipeline on every step.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use clippr_core::config::{Codec, Config, EncoderPref, Quality, Resolution};
use clippr_core::ipc::{Request, Response};
use gtk::{gio, glib};

use crate::state::State;
use crate::{daemon, util};

type Loaders = Rc<RefCell<Vec<Box<dyn Fn(&Config)>>>>;

const APPLY_DELAY: Duration = Duration::from_millis(700);

const RESOLUTIONS: [(Resolution, &str); 4] = [
    (Resolution::Native, "Native"),
    (Resolution::P1440, "1440p"),
    (Resolution::P1080, "1080p"),
    (Resolution::P720, "720p"),
];
const FRAMERATES: [u32; 5] = [30, 60, 90, 120, 144];
const CODECS: [(Codec, &str); 3] = [
    (Codec::H264, "H.264 (plays everywhere)"),
    (Codec::Hevc, "HEVC (smaller files)"),
    (Codec::Av1, "AV1 (smallest, newer GPUs)"),
];
const QUALITIES: [(Quality, &str); 4] = [
    (Quality::Low, "Low"),
    (Quality::Medium, "Medium"),
    (Quality::High, "High"),
    (Quality::Custom, "Custom bitrate"),
];
const ENCODERS: [(EncoderPref, &str); 3] = [
    (EncoderPref::Auto, "Automatic"),
    (EncoderPref::Hardware, "GPU only"),
    (EncoderPref::Software, "CPU (x264)"),
];

/// Applies pending edits after the user stops changing things.
struct Editor {
    state: Rc<State>,
    toasts: adw::ToastOverlay,
    pending: RefCell<Config>,
    timer: RefCell<Option<glib::SourceId>>,
    /// Set while widgets are being filled from the config, so that doesn't count as an edit.
    syncing: Cell<bool>,
}

impl Editor {
    fn edit(self: &Rc<Self>, f: impl FnOnce(&mut Config)) {
        if self.syncing.get() {
            return;
        }
        f(&mut self.pending.borrow_mut());
        if let Some(id) = self.timer.borrow_mut().take() {
            id.remove();
        }
        let this = self.clone();
        let id = glib::timeout_add_local_once(APPLY_DELAY, move || {
            this.timer.borrow_mut().take();
            this.apply();
        });
        *self.timer.borrow_mut() = Some(id);
    }

    fn apply(self: &Rc<Self>) {
        let new = self.pending.borrow().clone();
        let old = self.state.config();
        if new == old {
            return;
        }
        let restarts = self.state.online() && (new.video != old.video || new.audio != old.audio);
        let this = self.clone();
        glib::spawn_future_local(async move {
            let toast = match this.state.apply_config(new).await {
                Ok(()) if restarts => util::toast("Saved. The replay buffer restarted"),
                Ok(()) => util::toast("Saved"),
                Err(e) => util::toast(&format!("Couldn't save settings: {e}")),
            };
            toast.set_timeout(2);
            this.toasts.add_toast(toast);
        });
    }
}

pub fn build(state: &Rc<State>, toasts: &adw::ToastOverlay) -> gtk::Widget {
    let ed = Rc::new(Editor {
        state: state.clone(),
        toasts: toasts.clone(),
        pending: RefCell::new(state.config()),
        timer: RefCell::default(),
        syncing: Cell::new(false),
    });
    // Each loader copies one config field into its widget.
    let loaders: Loaders = Rc::default();
    let load = |f: Box<dyn Fn(&Config)>| loaders.borrow_mut().push(f);

    let page = adw::PreferencesPage::new();

    // --- Clips ---------------------------------------------------------------------------
    let clips = adw::PreferencesGroup::builder()
        .title("Clips")
        .description("How much each hotkey saves, and where clips go")
        .build();

    let short = adw::SpinRow::with_range(5.0, 600.0, 5.0);
    short.set_title("Short clip");
    short.set_subtitle("Seconds saved by the clip hotkey");
    clips.add(&short);
    {
        let ed = ed.clone();
        short.connect_value_notify(move |r| ed.edit(|c| c.general.clip_short_secs = r.value() as u32));
    }
    load(Box::new({
        let r = short.clone();
        move |c| r.set_value(c.general.clip_short_secs as f64)
    }));

    let long = adw::SpinRow::with_range(5.0, 600.0, 5.0);
    long.set_title("Long clip");
    long.set_subtitle("Seconds saved by the long clip hotkey");
    clips.add(&long);
    {
        let ed = ed.clone();
        long.connect_value_notify(move |r| ed.edit(|c| c.general.clip_long_secs = r.value() as u32));
    }
    load(Box::new({
        let r = long.clone();
        move |c| r.set_value(c.general.clip_long_secs as f64)
    }));

    let folder = adw::ActionRow::builder().title("Save to").build();
    folder.add_css_class("property");
    let choose = gtk::Button::builder()
        .icon_name("folder-open-symbolic")
        .tooltip_text("Choose folder")
        .valign(gtk::Align::Center)
        .build();
    choose.add_css_class("flat");
    folder.add_suffix(&choose);
    folder.set_activatable_widget(Some(&choose));
    clips.add(&folder);
    {
        let ed = ed.clone();
        choose.connect_clicked(move |btn| {
            let dialog = gtk::FileDialog::builder().title("Choose clips folder").modal(true).build();
            dialog.set_initial_folder(Some(&gio::File::for_path(&ed.pending.borrow().general.clips_dir)));
            let ed = ed.clone();
            let window = btn.root().and_downcast::<gtk::Window>();
            glib::spawn_future_local(async move {
                if let Ok(dir) = dialog.select_folder_future(window.as_ref()).await {
                    if let Some(path) = dir.path() {
                        ed.edit(|c| c.general.clips_dir = path);
                    }
                }
            });
        });
    }
    load(Box::new({
        let r = folder.clone();
        move |c| r.set_subtitle(&glib::markup_escape_text(&c.general.clips_dir.display().to_string()))
    }));

    let notify = adw::SwitchRow::builder()
        .title("Show a popup when a clip is saved")
        .subtitle("A small card in the corner, above fullscreen games")
        .build();
    clips.add(&notify);
    {
        let ed = ed.clone();
        notify.connect_active_notify(move |r| ed.edit(|c| c.general.notify_on_save = r.is_active()));
    }
    load(Box::new({
        let r = notify.clone();
        move |c| r.set_active(c.general.notify_on_save)
    }));
    page.add(&clips);

    // --- Video ---------------------------------------------------------------------------
    let video = adw::PreferencesGroup::builder()
        .title("Video")
        .description("Changing these restarts the replay buffer")
        .build();

    let resolution = combo("Resolution", RESOLUTIONS.iter().map(|r| r.1));
    resolution.set_subtitle("Downscaling happens on the GPU");
    video.add(&resolution);
    {
        let ed = ed.clone();
        resolution.connect_selected_notify(move |r| {
            let v = RESOLUTIONS[r.selected() as usize % RESOLUTIONS.len()].0;
            ed.edit(|c| c.video.resolution = v);
        });
    }
    load(Box::new({
        let r = resolution.clone();
        move |c| r.set_selected(index_of(&RESOLUTIONS, c.video.resolution))
    }));

    let fps_labels: Vec<String> = FRAMERATES.iter().map(|f| format!("{f} fps")).collect();
    let fps = combo("Frame rate", fps_labels.iter().map(String::as_str));
    video.add(&fps);
    {
        let ed = ed.clone();
        fps.connect_selected_notify(move |r| {
            let v = FRAMERATES[r.selected() as usize % FRAMERATES.len()];
            ed.edit(|c| c.video.fps = v);
        });
    }
    load(Box::new({
        let r = fps.clone();
        move |c| {
            let i = FRAMERATES.iter().position(|f| *f >= c.video.fps).unwrap_or(1);
            r.set_selected(i as u32);
        }
    }));

    let codec = combo("Codec", CODECS.iter().map(|c| c.1));
    video.add(&codec);
    {
        let ed = ed.clone();
        codec.connect_selected_notify(move |r| {
            let v = CODECS[r.selected() as usize % CODECS.len()].0;
            ed.edit(|c| c.video.codec = v);
        });
    }
    load(Box::new({
        let r = codec.clone();
        move |c| r.set_selected(index_of(&CODECS, c.video.codec))
    }));

    let quality = combo("Quality", QUALITIES.iter().map(|q| q.1));
    video.add(&quality);
    let bitrate = adw::SpinRow::with_range(1000.0, 100_000.0, 500.0);
    bitrate.set_title("Bitrate");
    bitrate.set_subtitle("Kilobits per second");
    video.add(&bitrate);
    {
        let ed = ed.clone();
        quality.connect_selected_notify(move |r| {
            let v = QUALITIES[r.selected() as usize % QUALITIES.len()].0;
            ed.edit(|c| c.video.quality = v);
        });
    }
    {
        let ed = ed.clone();
        bitrate.connect_value_notify(move |r| ed.edit(|c| c.video.bitrate_kbps = r.value() as u32));
    }
    load(Box::new({
        let (q, b) = (quality.clone(), bitrate.clone());
        move |c| {
            q.set_selected(index_of(&QUALITIES, c.video.quality));
            b.set_value(c.video.bitrate_kbps as f64);
            b.set_visible(c.video.quality == Quality::Custom);
        }
    }));

    let encoder = combo("Encoder", ENCODERS.iter().map(|e| e.1));
    encoder.set_subtitle("Automatic uses the GPU and falls back to the CPU");
    video.add(&encoder);
    {
        let ed = ed.clone();
        encoder.connect_selected_notify(move |r| {
            let v = ENCODERS[r.selected() as usize % ENCODERS.len()].0;
            ed.edit(|c| c.video.encoder = v);
        });
    }
    load(Box::new({
        let r = encoder.clone();
        move |c| r.set_selected(index_of(&ENCODERS, c.video.encoder))
    }));

    let cursor = adw::SwitchRow::builder().title("Record mouse cursor").build();
    video.add(&cursor);
    {
        let ed = ed.clone();
        cursor.connect_active_notify(move |r| ed.edit(|c| c.video.capture_cursor = r.is_active()));
    }
    load(Box::new({
        let r = cursor.clone();
        move |c| r.set_active(c.video.capture_cursor)
    }));

    // Memory estimate for the buffer, since that's the main cost of longer clips.
    let estimate = adw::ActionRow::builder().title("Replay buffer memory").build();
    estimate.add_css_class("property");
    video.add(&estimate);
    {
        let state = state.clone();
        load(Box::new({
            let r = estimate.clone();
            move |c| {
                let height = match c.video.resolution.height() {
                    Some(h) => h,
                    None => state.status().map(|s| s.height).filter(|h| *h > 0).unwrap_or(1440),
                };
                let kbps = c.video_bitrate_kbps(height) as u64 + c.audio.bitrate_kbps as u64;
                let bytes = kbps * 1000 / 8 * c.general.buffer_secs() as u64;
                r.set_subtitle(&format!(
                    "About {} for {} s at {} kbps",
                    util::size(bytes),
                    c.general.buffer_secs(),
                    kbps
                ));
            }
        }));
    }
    page.add(&video);

    // --- Audio ---------------------------------------------------------------------------
    let audio = adw::PreferencesGroup::builder().title("Audio").build();
    let desktop = adw::SwitchRow::builder()
        .title("Desktop audio")
        .subtitle("Everything playing on your default output")
        .build();
    audio.add(&desktop);
    {
        let ed = ed.clone();
        desktop.connect_active_notify(move |r| ed.edit(|c| c.audio.capture_desktop = r.is_active()));
    }
    load(Box::new({
        let r = desktop.clone();
        move |c| r.set_active(c.audio.capture_desktop)
    }));

    let mic = adw::SwitchRow::builder().title("Microphone").build();
    audio.add(&mic);
    let mic_device = adw::EntryRow::builder()
        .title("Microphone device (blank for default)")
        .show_apply_button(true)
        .build();
    audio.add(&mic_device);
    {
        let ed = ed.clone();
        mic.connect_active_notify(move |r| ed.edit(|c| c.audio.capture_mic = r.is_active()));
    }
    {
        let ed = ed.clone();
        mic_device.connect_apply(move |r| {
            let text = r.text().trim().to_string();
            ed.edit(|c| c.audio.mic_device = (!text.is_empty()).then_some(text));
        });
    }
    load(Box::new({
        let (m, d) = (mic.clone(), mic_device.clone());
        move |c| {
            m.set_active(c.audio.capture_mic);
            d.set_text(c.audio.mic_device.as_deref().unwrap_or(""));
            d.set_sensitive(c.audio.capture_mic);
        }
    }));

    let abitrate = adw::SpinRow::with_range(64.0, 320.0, 32.0);
    abitrate.set_title("Audio bitrate");
    abitrate.set_subtitle("Kilobits per second");
    audio.add(&abitrate);
    {
        let ed = ed.clone();
        abitrate.connect_value_notify(move |r| ed.edit(|c| c.audio.bitrate_kbps = r.value() as u32));
    }
    load(Box::new({
        let r = abitrate.clone();
        move |c| r.set_value(c.audio.bitrate_kbps as f64)
    }));
    page.add(&audio);

    // --- Hotkeys -------------------------------------------------------------------------
    let keys = adw::PreferencesGroup::builder()
        .title("Hotkeys")
        .description(
            "Preferred keys, like F8 or SHIFT+F8. Your desktop may ask to confirm them, \
             and on KDE you can rebind them in System Settings › Shortcuts.",
        )
        .build();
    type HotkeyField = fn(&mut Config) -> &mut String;
    let hotkey_rows: [(&str, HotkeyField); 3] = [
        ("Save short clip", |c| &mut c.hotkeys.clip_short),
        ("Save long clip", |c| &mut c.hotkeys.clip_long),
        ("Toggle quick overlay", |c| &mut c.hotkeys.toggle_overlay),
    ];
    for (title, field) in hotkey_rows {
        let row = adw::EntryRow::builder().title(title).show_apply_button(true).build();
        keys.add(&row);
        {
            let ed = ed.clone();
            row.connect_apply(move |r| {
                let text = r.text().trim().to_uppercase();
                if !text.is_empty() {
                    ed.edit(|c| *field(c) = text);
                }
            });
        }
        load(Box::new({
            let r = row.clone();
            move |c| {
                let mut c = c.clone();
                r.set_text(field(&mut c));
            }
        }));
    }
    page.add(&keys);

    // --- Recorder ------------------------------------------------------------------------
    let recorder = adw::PreferencesGroup::builder().title("Recorder").build();
    let start_rec = adw::SwitchRow::builder()
        .title("Record when the recorder starts")
        .subtitle("Off starts paused until you resume from the overlay")
        .build();
    recorder.add(&start_rec);
    {
        let ed = ed.clone();
        start_rec.connect_active_notify(move |r| ed.edit(|c| c.general.start_recording = r.is_active()));
    }
    load(Box::new({
        let r = start_rec.clone();
        move |c| r.set_active(c.general.start_recording)
    }));

    let source = adw::ActionRow::builder()
        .title("Screen source")
        .subtitle("Pick which screen to record")
        .build();
    let choose_source = gtk::Button::builder().label("Choose again").valign(gtk::Align::Center).build();
    source.add_suffix(&choose_source);
    recorder.add(&source);
    {
        let toasts = toasts.clone();
        choose_source.connect_clicked(move |_| {
            let toasts = toasts.clone();
            glib::spawn_future_local(async move {
                let msg = match daemon::call(Request::ResetCaptureSource).await {
                    Ok(Response::Ok) | Ok(_) => "Pick a screen in the dialog that opens".to_string(),
                    Err(e) => format!("Couldn't reset: {e}"),
                };
                toasts.add_toast(util::toast(&msg));
            });
        });
    }

    let service = adw::ActionRow::builder().title("Background recorder").build();
    let service_btn = gtk::Button::builder().valign(gtk::Align::Center).build();
    service.add_suffix(&service_btn);
    recorder.add(&service);
    {
        let state = state.clone();
        let toasts = toasts.clone();
        service_btn.connect_clicked(move |_| {
            if state.online() {
                let toasts = toasts.clone();
                glib::spawn_future_local(async move {
                    if let Err(e) = daemon::call(Request::Quit).await {
                        toasts.add_toast(util::toast(&format!("{e}")));
                    }
                });
            } else {
                util::start_daemon();
            }
        });
    }
    page.add(&recorder);

    // Fill widgets from the config now and whenever it changes elsewhere (e.g. the overlay).
    let sync = {
        let ed = ed.clone();
        let loaders = loaders.clone();
        let state = state.clone();
        move || {
            // An edit waiting to be applied wins over what the daemon last reported.
            if ed.timer.borrow().is_some() {
                return;
            }
            let cfg = state.config();
            *ed.pending.borrow_mut() = cfg.clone();
            ed.syncing.set(true);
            for f in loaders.borrow().iter() {
                f(&cfg);
            }
            ed.syncing.set(false);

            let online = state.online();
            service.set_subtitle(if online {
                "Running. Uses the GPU encoder and a few MB of RAM"
            } else {
                "Not running"
            });
            service_btn.set_label(if online { "Stop" } else { "Start" });
            source.set_sensitive(online);
        }
    };
    sync();
    state.connect_changed(sync);

    page.upcast()
}

fn combo<'a>(title: &str, items: impl Iterator<Item = &'a str>) -> adw::ComboRow {
    let items: Vec<&str> = items.collect();
    adw::ComboRow::builder()
        .title(title)
        .model(&gtk::StringList::new(&items))
        .build()
}

fn index_of<T: PartialEq + Copy>(table: &[(T, &str)], value: T) -> u32 {
    table.iter().position(|(v, _)| *v == value).unwrap_or(0) as u32
}
