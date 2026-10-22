//! The Settings tab. Edits apply automatically after a short pause, so dragging a spin button
//! doesn't restart the capture pipeline on every step.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use clippr_core::clip::AudioTrackKind;
use clippr_core::config::{normalize_trigger, ClipBinding, Codec, Config, EncoderPref, Quality, Resolution};
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
        if let Err(e) = new.hotkeys.validate() {
            self.toasts.add_toast(util::toast(&e.to_string()));
            return;
        }
        let old = self.state.config();
        if new == old {
            return;
        }
        let restarts = self.state.online() && old.needs_capture_restart(&new);
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
        .description("Where clips go and save notifications")
        .build();

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
                let bytes = kbps * 1000 / 8 * c.buffer_secs() as u64;
                r.set_subtitle(&format!(
                    "About {} for {} s at {} kbps",
                    util::size(bytes),
                    c.buffer_secs(),
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

    let separate = adw::SwitchRow::builder()
        .title("Separate audio tracks")
        .subtitle("Game, microphone and voice chat each get a track. The first track is always the full mix, so clips still play everywhere")
        .build();
    audio.add(&separate);
    let voice_apps = adw::EntryRow::builder()
        .title("Voice chat apps, comma separated")
        .show_apply_button(true)
        .build();
    audio.add(&voice_apps);
    {
        let ed = ed.clone();
        separate.connect_active_notify(move |r| ed.edit(|c| c.audio.separate_tracks = r.is_active()));
    }
    {
        let ed = ed.clone();
        voice_apps.connect_apply(move |r| {
            let mut apps: Vec<String> = r
                .text()
                .split(',')
                .map(|a| a.trim().to_lowercase())
                .filter(|a| !a.is_empty())
                .collect();
            apps.dedup();
            ed.edit(|c| c.audio.voice_apps = apps);
        });
    }
    load(Box::new({
        let (s, v) = (separate.clone(), voice_apps.clone());
        move |c| {
            s.set_active(c.audio.separate_tracks);
            v.set_text(&c.audio.voice_apps.join(", "));
            v.set_sensitive(c.audio.separate_tracks);
        }
    }));
    page.add(&audio);

    let levels = adw::PreferencesGroup::builder()
        .title("Default levels")
        .description("Each source's volume in the saved mix. Needs separate audio tracks. New recordings start with these levels. Saved clips keep their own levels")
        .build();
    type LevelField = fn(&mut Config) -> &mut u32;
    let level_rows: [(AudioTrackKind, LevelField); 3] = [
        (AudioTrackKind::Desktop, |c| &mut c.audio.levels.game),
        (AudioTrackKind::Mic, |c| &mut c.audio.levels.mic),
        (AudioTrackKind::Voice, |c| &mut c.audio.levels.voice),
    ];
    for (kind, field) in level_rows {
        let scale = util::level_scale(100);
        let row = adw::ActionRow::builder().title(kind.label()).build();
        row.add_suffix(&scale);
        levels.add(&row);
        {
            let ed = ed.clone();
            scale.connect_value_changed(move |s| ed.edit(|c| *field(c) = s.value().round() as u32));
        }
        load(Box::new(move |c| {
            scale.set_value(c.audio.levels.percent(kind) as f64);
            row.set_sensitive(c.audio.separate_tracks);
        }));
    }
    page.add(&levels);

    // --- Hotkeys -------------------------------------------------------------------------
    let keys = adw::PreferencesGroup::builder()
        .title("Hotkeys")
        .description(
            "Preferred keys, like F8 or SHIFT+F8. Your desktop may ask to confirm them, \
             and on KDE you can rebind them in System Settings › Shortcuts.",
        )
        .build();
    let binding_rows = Rc::new(RefCell::new(Vec::<adw::ActionRow>::new()));
    load(Box::new({
        let keys = keys.clone();
        let ed = ed.clone();
        let rows = binding_rows.clone();
        let previous = RefCell::new(None);
        move |c| {
            if previous.borrow().as_ref() == Some(&c.hotkeys.clips) {
                return;
            }
            *previous.borrow_mut() = Some(c.hotkeys.clips.clone());
            for row in rows.borrow_mut().drain(..) {
                keys.remove(&row);
            }
            for binding in &c.hotkeys.clips {
                let row = adw::ActionRow::builder()
                    .title(&binding.trigger)
                    .subtitle(format!("Save the last {}", util::clip_length(binding.seconds)))
                    .build();
                let edit = gtk::Button::builder()
                    .icon_name("document-edit-symbolic")
                    .tooltip_text("Edit shortcut")
                    .valign(gtk::Align::Center)
                    .build();
                edit.add_css_class("flat");
                let remove = gtk::Button::builder()
                    .icon_name("user-trash-symbolic")
                    .tooltip_text("Remove shortcut")
                    .valign(gtk::Align::Center)
                    .build();
                remove.add_css_class("flat");
                row.add_suffix(&edit);
                row.add_suffix(&remove);
                row.set_activatable_widget(Some(&edit));
                {
                    let ed = ed.clone();
                    let trigger = binding.trigger.clone();
                    edit.connect_clicked(move |button| {
                        binding_dialog(&ed, button, Some(trigger.clone()));
                    });
                }
                {
                    let ed = ed.clone();
                    let trigger = binding.trigger.clone();
                    remove.connect_clicked(move |button| {
                        ed.edit(|c| c.hotkeys.clips.retain(|b| b.trigger != trigger));
                        button.set_sensitive(false);
                        ed.apply();
                    });
                }
                keys.add(&row);
                rows.borrow_mut().push(row);
            }
        }
    }));
    let add = gtk::Button::builder().label("Add clip shortcut").build();
    keys.set_header_suffix(Some(&add));
    {
        let ed = ed.clone();
        add.connect_clicked(move |button| binding_dialog(&ed, button, None));
    }
    let overlay_key = adw::EntryRow::builder()
        .title("Toggle quick overlay")
        .show_apply_button(true)
        .build();
    keys.add(&overlay_key);
    {
        let ed = ed.clone();
        overlay_key.connect_apply(move |row| {
            match normalize_trigger(&row.text()) {
                Ok(trigger) => ed.edit(|c| c.hotkeys.toggle_overlay = trigger),
                Err(e) => ed.toasts.add_toast(util::toast(&e.to_string())),
            }
        });
    }
    load(Box::new(move |c| overlay_key.set_text(&c.hotkeys.toggle_overlay)));
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

fn binding_dialog(ed: &Rc<Editor>, parent: &gtk::Button, original: Option<String>) {
    let binding = ed.pending.borrow().hotkeys.clips.iter().find(|b| Some(&b.trigger) == original.as_ref()).cloned();
    let dialog = adw::AlertDialog::builder()
        .heading(if original.is_some() { "Edit clip shortcut" } else { "Add clip shortcut" })
        .body("Choose a key and how much of the replay buffer it saves.")
        .build();
    dialog.add_responses(&[("cancel", "Cancel"), ("save", "Save")]);
    dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("save"));
    dialog.set_close_response("cancel");
    let trigger = adw::EntryRow::builder().title("Shortcut, e.g. F9 or CTRL+F9").build();
    trigger.set_text(binding.as_ref().map_or("", |b| b.trigger.as_str()));
    let seconds = adw::SpinRow::with_range(1.0, 3600.0, 1.0);
    seconds.set_title("Seconds to save");
    seconds.set_subtitle("45 = 45 seconds, 300 = 5 minutes");
    seconds.set_value(binding.as_ref().map_or(30, |b| b.seconds) as f64);
    let fields = adw::PreferencesGroup::new();
    fields.add(&trigger);
    fields.add(&seconds);
    let error = gtk::Label::builder().wrap(true).xalign(0.0).build();
    error.add_css_class("error");
    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.append(&fields);
    content.append(&error);
    dialog.set_extra_child(Some(&content));
    let validate = Rc::new({
        let ed = ed.clone();
        let original = original.clone();
        let dialog = dialog.downgrade();
        let trigger = trigger.downgrade();
        let seconds = seconds.downgrade();
        move || {
            let (Some(dialog), Some(trigger), Some(seconds)) = (dialog.upgrade(), trigger.upgrade(), seconds.upgrade()) else { return };
            let result = normalize_trigger(&trigger.text()).and_then(|trigger| {
                let mut hotkeys = ed.pending.borrow().hotkeys.clone();
                let binding = ClipBinding { trigger, seconds: seconds.value() as u32 };
                update_binding(&mut hotkeys, original.as_deref(), binding)?;
                hotkeys.validate()
            });
            dialog.set_response_enabled("save", result.is_ok());
            error.set_label(&result.err().map_or(String::new(), |e| e.to_string()));
        }
    });
    validate();
    {
        let validate = validate.clone();
        trigger.connect_changed(move |_| validate());
    }
    seconds.connect_value_notify(move |_| validate());
    let ed = ed.clone();
    let parent = parent.clone();
    glib::spawn_future_local(async move {
        if dialog.choose_future(Some(&parent)).await == "save" {
            if let Ok(trigger) = normalize_trigger(&trigger.text()) {
                let binding = ClipBinding { trigger, seconds: seconds.value() as u32 };
                let mut hotkeys = ed.pending.borrow().hotkeys.clone();
                match update_binding(&mut hotkeys, original.as_deref(), binding).and_then(|_| hotkeys.validate()) {
                    Ok(()) => ed.edit(|c| c.hotkeys = hotkeys),
                    Err(e) => ed.toasts.add_toast(util::toast(&e.to_string())),
                }
            }
        }
    });
}

fn update_binding(hotkeys: &mut clippr_core::config::Hotkeys, original: Option<&str>, binding: ClipBinding) -> anyhow::Result<()> {
    if let Some(trigger) = original {
        let existing = hotkeys.clips.iter_mut().find(|b| b.trigger == trigger)
            .ok_or_else(|| anyhow::anyhow!("This shortcut was removed. Close this dialog and add a new one"))?;
        *existing = binding;
    } else {
        hotkeys.clips.push(binding);
    }
    Ok(())
}
