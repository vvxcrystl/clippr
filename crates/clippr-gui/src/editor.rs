//! Clip editor: playback, trimming and cutting, metadata, and export.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use adw::prelude::*;
use clippr_core::clip::ClipMeta;
use clippr_core::edit::{self, ExportRequest};
use gtk::{gdk, gio, glib};

use crate::state::State;
use crate::timeline::Timeline;
use crate::util;

type MetaChange = Box<dyn FnOnce(&mut ClipMeta)>;

const SEEK_STEP_US: i64 = 5_000_000;

pub fn build(state: &Rc<State>, toasts: &adw::ToastOverlay, clip: ClipMeta) -> adw::NavigationPage {
    let library = state.library();
    let video_path = library.video_path(&clip);
    let clip = Rc::new(RefCell::new(clip));

    // --- Video -------------------------------------------------------------------------------
    let media = gtk::MediaFile::for_filename(&video_path);
    let picture = gtk::Picture::builder()
        .paintable(&media)
        .content_fit(gtk::ContentFit::Contain)
        .vexpand(true)
        .hexpand(true)
        .build();
    let video_frame = gtk::Overlay::new();
    video_frame.set_child(Some(&picture));
    video_frame.add_css_class("video-frame");
    video_frame.set_overflow(gtk::Overflow::Hidden);

    // Click the video to play or pause.
    let click = gtk::GestureClick::new();
    {
        let media = media.clone();
        click.connect_released(move |_, n, _, _| {
            if n == 1 {
                media.set_playing(!media.is_playing());
            }
        });
    }
    picture.add_controller(click);

    // --- Transport and edit tools ------------------------------------------------------------
    let duration = clip.borrow().duration_secs;
    let timeline = Timeline::new(duration);

    let play = icon_button("media-playback-start-symbolic", "Play (Space)");
    play.add_css_class("circular");
    play.add_css_class("play-button");
    let time = gtk::Label::new(Some(&format!("0:00.0 / {}", util::duration_precise(duration))));
    time.add_css_class("numeric");
    time.add_css_class("dim-label");
    let mute = gtk::ToggleButton::builder()
        .icon_name("audio-volume-high-symbolic")
        .tooltip_text("Mute (M)")
        .build();
    mute.add_css_class("flat");

    let split = labeled_button("edit-cut-symbolic", "Split", "Split at playhead (S)");
    let trim_in = labeled_button("go-first-symbolic", "Trim start", "Trim part start to playhead (I)");
    let trim_out = labeled_button("go-last-symbolic", "Trim end", "Trim part end to playhead (O)");
    let delete_part = labeled_button("list-remove-symbolic", "Remove part", "Cut the selected part out (Delete)");
    let undo = icon_button("edit-undo-symbolic", "Undo (Ctrl+Z)");
    let reset = icon_button("edit-clear-all-symbolic", "Reset all cuts");
    let skip_cuts = gtk::ToggleButton::builder()
        .icon_name("media-skip-forward-symbolic")
        .tooltip_text("Skip cut parts during playback")
        .active(true)
        .build();
    skip_cuts.add_css_class("flat");

    let tools = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    tools.add_css_class("linked-tools");
    for b in [&split, &trim_in, &trim_out, &delete_part] {
        tools.append(b);
    }

    let transport = gtk::Box::builder().spacing(8).build();
    transport.append(&play);
    transport.append(&time);
    transport.append(&mute);
    let spacer = gtk::Box::builder().hexpand(true).build();
    transport.append(&spacer);
    transport.append(&tools);
    transport.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    transport.append(&undo);
    transport.append(&reset);
    transport.append(&skip_cuts);

    let editor_column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .margin_start(18)
        .margin_end(12)
        .margin_top(12)
        .margin_bottom(18)
        .hexpand(true)
        .build();
    editor_column.append(&video_frame);
    editor_column.append(&transport);
    editor_column.append(timeline.widget());

    // --- Sidebar: details and export ---------------------------------------------------------
    let title_row = adw::EntryRow::builder()
        .title("Title")
        .text(&clip.borrow().title)
        .show_apply_button(true)
        .build();
    let tags_row = adw::EntryRow::builder()
        .title("Tags, comma separated")
        .text(clip.borrow().tags.join(", "))
        .show_apply_button(true)
        .build();
    let details = adw::PreferencesGroup::builder().title("Details").build();
    details.add(&title_row);
    details.add(&tags_row);

    let info = adw::PreferencesGroup::builder().title("Recorded").build();
    fill_info(&info, &clip.borrow());

    let kept_row = adw::ActionRow::builder().title("Keeping").build();
    kept_row.add_css_class("property");
    let precise_row = adw::SwitchRow::builder()
        .title("Frame-accurate cuts")
        .subtitle("Re-encodes on the GPU. Off is instant but snaps cuts to the nearest second")
        .build();
    let export_group = adw::PreferencesGroup::builder().title("Export").build();
    export_group.add(&kept_row);
    export_group.add(&precise_row);

    let save_new = gtk::Button::with_label("Save as new clip");
    save_new.add_css_class("suggested-action");
    save_new.add_css_class("pill");
    let replace = gtk::Button::with_label("Replace original");
    replace.add_css_class("pill");
    let progress = gtk::ProgressBar::builder().show_text(true).visible(false).build();
    let cancel = gtk::Button::with_label("Cancel");
    cancel.add_css_class("pill");
    cancel.set_visible(false);
    let export_buttons = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(12)
        .build();
    export_buttons.append(&save_new);
    export_buttons.append(&replace);
    export_buttons.append(&progress);
    export_buttons.append(&cancel);

    let sidebar_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_start(12)
        .margin_end(18)
        .margin_top(12)
        .margin_bottom(18)
        .build();
    sidebar_box.append(&details);
    sidebar_box.append(&export_group);
    sidebar_box.append(&export_buttons);
    sidebar_box.append(&info);
    let sidebar = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .width_request(320)
        .child(&sidebar_box)
        .build();
    sidebar.add_css_class("editor-sidebar");

    let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    body.append(&editor_column);
    body.append(&sidebar);

    // --- Header ------------------------------------------------------------------------------
    let window_title = adw::WindowTitle::new(&clip.borrow().title, &subtitle(&clip.borrow()));
    let header = adw::HeaderBar::builder().title_widget(&window_title).build();
    let favorite = gtk::ToggleButton::builder()
        .icon_name(star_icon(clip.borrow().favorite))
        .active(clip.borrow().favorite)
        .tooltip_text("Favorite")
        .build();
    favorite.add_css_class("flat");
    let menu = gio::Menu::new();
    menu.append(Some("Copy file"), Some("clip.copy"));
    menu.append(Some("Open in video player"), Some("clip.open"));
    menu.append(Some("Show in folder"), Some("clip.folder"));
    let danger = gio::Menu::new();
    danger.append(Some("Delete clip"), Some("clip.delete"));
    menu.append_section(None, &danger);
    let menu_button = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .menu_model(&menu)
        .tooltip_text("More")
        .build();
    header.pack_end(&menu_button);
    header.pack_end(&favorite);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&body));

    let page = adw::NavigationPage::builder()
        .title(clip.borrow().title.as_str())
        .child(&toolbar)
        .build();

    // Stacks the sidebar under the editor on narrow windows.
    let bp = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        820.0,
        adw::LengthUnit::Sp,
    ));
    bp.add_setter(&body, "orientation", Some(&gtk::Orientation::Vertical.to_value()));
    bp.add_setter(&sidebar, "vexpand", Some(&true.to_value()));
    bp.add_setter(&tools, "visible", Some(&false.to_value()));
    page.connect_realize(move |page| {
        if let Some(win) = page.root().and_downcast::<adw::ApplicationWindow>() {
            win.add_breakpoint(bp.clone());
        }
    });

    // --- Behaviour ---------------------------------------------------------------------------
    let refresh_tools: Rc<dyn Fn()> = {
        let timeline = timeline.clone();
        let (kept_row, undo, reset, delete_part, replace) =
            (kept_row.clone(), undo.clone(), reset.clone(), delete_part.clone(), replace.clone());
        let save_new = save_new.clone();
        Rc::new(move || {
            let parts = timeline.parts().len();
            kept_row.set_subtitle(&format!(
                "{} of {} in {} part{}",
                util::duration_precise(timeline.kept_secs()),
                util::duration_precise(timeline.duration()),
                parts,
                if parts == 1 { "" } else { "s" },
            ));
            let untouched = timeline.is_untouched();
            undo.set_sensitive(timeline.can_undo());
            reset.set_sensitive(!untouched);
            delete_part.set_sensitive(parts > 1);
            save_new.set_sensitive(!untouched);
            replace.set_sensitive(!untouched);
        })
    };
    refresh_tools();
    {
        let refresh_tools = refresh_tools.clone();
        timeline.connect_changed(move || refresh_tools());
    }
    {
        let media = media.clone();
        timeline.connect_seek(move |t| media.seek((t * 1e6) as i64));
    }

    // Playhead follows playback; skip over cut ranges when enabled.
    {
        let timeline = timeline.clone();
        let time = time.clone();
        let skip_cuts = skip_cuts.clone();
        media.connect_timestamp_notify(move |media| {
            let t = media.timestamp() as f64 / 1e6;
            timeline.set_playhead(t);
            time.set_label(&format!(
                "{} / {}",
                util::duration_precise(t),
                util::duration_precise(timeline.duration())
            ));
            if media.is_playing() && skip_cuts.is_active() {
                match timeline.resolve_playback(t) {
                    Some(next) if (next - t).abs() > 0.05 => media.seek((next * 1e6) as i64),
                    Some(_) => {}
                    None => {
                        media.pause();
                        media.seek((timeline.first_start() * 1e6) as i64);
                    }
                }
            }
        });
    }
    {
        let play = play.clone();
        media.connect_playing_notify(move |media| {
            let playing = media.is_playing();
            play.set_icon_name(if playing {
                "media-playback-pause-symbolic"
            } else {
                "media-playback-start-symbolic"
            });
            play.set_tooltip_text(Some(if playing { "Pause (Space)" } else { "Play (Space)" }));
        });
    }
    {
        let timeline = timeline.clone();
        media.connect_ended_notify(move |media| {
            if media.is_ended() {
                media.seek((timeline.first_start() * 1e6) as i64);
            }
        });
    }
    media.connect_error_notify({
        let toasts = toasts.clone();
        move |media| {
            if let Some(err) = media.error() {
                toasts.add_toast(util::toast(&format!("Can't play this clip: {err}")));
            }
        }
    });

    let toggle_play: Rc<dyn Fn()> = {
        let media = media.clone();
        let timeline = timeline.clone();
        let skip_cuts = skip_cuts.clone();
        Rc::new(move || {
            if !media.is_playing() && skip_cuts.is_active() {
                let t = media.timestamp() as f64 / 1e6;
                if timeline.resolve_playback(t).is_none() {
                    media.seek((timeline.first_start() * 1e6) as i64);
                }
            }
            media.set_playing(!media.is_playing());
        })
    };
    {
        let toggle_play = toggle_play.clone();
        play.connect_clicked(move |_| toggle_play());
    }
    {
        let media = media.clone();
        mute.connect_toggled(move |b| {
            media.set_muted(b.is_active());
            b.set_icon_name(if b.is_active() {
                "audio-volume-muted-symbolic"
            } else {
                "audio-volume-high-symbolic"
            });
        });
    }
    {
        let t = timeline.clone();
        split.connect_clicked(move |_| {
            t.split();
        });
    }
    {
        let t = timeline.clone();
        trim_in.connect_clicked(move |_| {
            t.trim_start();
        });
    }
    {
        let t = timeline.clone();
        trim_out.connect_clicked(move |_| {
            t.trim_end();
        });
    }
    {
        let t = timeline.clone();
        delete_part.connect_clicked(move |_| {
            t.delete_selected();
        });
    }
    {
        let t = timeline.clone();
        undo.connect_clicked(move |_| {
            t.undo();
        });
    }
    {
        let t = timeline.clone();
        reset.connect_clicked(move |_| {
            t.reset();
        });
    }

    // Keyboard shortcuts. Bubble phase, so typing in the title field is unaffected.
    let shortcuts = gtk::ShortcutController::new();
    shortcuts.set_scope(gtk::ShortcutScope::Managed);
    let add = |trigger: &str, f: Rc<dyn Fn()>| {
        let action = gtk::CallbackAction::new(move |_, _| {
            f();
            glib::Propagation::Stop
        });
        shortcuts.add_shortcut(gtk::Shortcut::new(gtk::ShortcutTrigger::parse_string(trigger), Some(action)));
    };
    add("space", toggle_play.clone());
    add("k", toggle_play.clone());
    for (trigger, op) in [
        ("s", 0u8),
        ("i", 1),
        ("o", 2),
        ("Delete", 3),
        ("x", 3),
        ("<Control>z", 4),
    ] {
        let t = timeline.clone();
        add(
            trigger,
            Rc::new(move || {
                match op {
                    0 => t.split(),
                    1 => t.trim_start(),
                    2 => t.trim_end(),
                    3 => t.delete_selected(),
                    _ => t.undo(),
                };
            }),
        );
    }
    for (trigger, delta) in [("Left", -SEEK_STEP_US), ("j", -SEEK_STEP_US), ("Right", SEEK_STEP_US), ("l", SEEK_STEP_US)] {
        let media = media.clone();
        add(
            trigger,
            Rc::new(move || {
                let target = (media.timestamp() + delta).clamp(0, media.duration().max(0));
                media.seek(target);
            }),
        );
    }
    {
        let mute = mute.clone();
        add("m", Rc::new(move || mute.set_active(!mute.is_active())));
    }
    page.add_controller(shortcuts);

    // Metadata edits are written straight to the sidecar.
    let save_meta: Rc<dyn Fn(MetaChange)> = {
        let clip = clip.clone();
        let state = state.clone();
        let toasts = toasts.clone();
        let window_title = window_title.clone();
        let page = page.clone();
        Rc::new(move |change| {
            let mut updated = clip.borrow().clone();
            change(&mut updated);
            let lib = state.library();
            let to_write = updated.clone();
            let clip = clip.clone();
            let state = state.clone();
            let toasts = toasts.clone();
            let window_title = window_title.clone();
            let page = page.clone();
            glib::spawn_future_local(async move {
                let result = gio::spawn_blocking(move || lib.update(&to_write))
                    .await
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("writer panicked")));
                match result {
                    Ok(()) => {
                        window_title.set_title(&updated.title);
                        page.set_title(&updated.title);
                        *clip.borrow_mut() = updated;
                        state.library_changed();
                    }
                    Err(e) => toasts.add_toast(util::toast(&format!("Couldn't save: {e}"))),
                }
            });
        })
    };
    {
        let save_meta = save_meta.clone();
        title_row.connect_apply(move |row| {
            let title = row.text().trim().to_string();
            if title.is_empty() {
                return;
            }
            save_meta(Box::new(move |c| c.title = title));
        });
    }
    {
        let save_meta = save_meta.clone();
        tags_row.connect_apply(move |row| {
            let mut tags: Vec<String> = row
                .text()
                .split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect();
            tags.dedup();
            save_meta(Box::new(move |c| c.tags = tags));
        });
    }
    {
        let save_meta = save_meta.clone();
        favorite.connect_toggled(move |b| {
            let on = b.is_active();
            b.set_icon_name(star_icon(on));
            save_meta(Box::new(move |c| c.favorite = on));
        });
    }

    // Header menu actions
    let actions = gio::SimpleActionGroup::new();
    {
        let path = video_path.clone();
        let toasts = toasts.clone();
        let page = page.clone();
        let copy = gio::SimpleAction::new("copy", None);
        copy.connect_activate(move |_, _| {
            if util::copy_file(&page, &path) {
                toasts.add_toast(util::toast("Copied clip file"));
            }
        });
        actions.add_action(&copy);
    }
    {
        let path = video_path.clone();
        let page = page.clone();
        let open = gio::SimpleAction::new("open", None);
        open.connect_activate(move |_, _| util::open_file(&page, &path));
        actions.add_action(&open);
    }
    {
        let path = video_path.clone();
        let page = page.clone();
        let folder = gio::SimpleAction::new("folder", None);
        folder.connect_activate(move |_, _| util::show_in_folder(&page, &path));
        actions.add_action(&folder);
    }
    {
        let state = state.clone();
        let toasts = toasts.clone();
        let page = page.clone();
        let clip = clip.clone();
        let media = media.clone();
        let delete = gio::SimpleAction::new("delete", None);
        delete.connect_activate(move |_, _| {
            media.set_playing(false);
            let (state, toasts, page) = (state.clone(), toasts.clone(), page.clone());
            let target = clip.borrow().clone();
            glib::spawn_future_local(async move {
                if crate::library::delete_clip(page.upcast_ref(), &state, &toasts, &target).await {
                    pop(&page);
                }
            });
        });
        actions.add_action(&delete);
    }
    page.insert_action_group("clip", Some(&actions));

    // Export
    let cancel_flag: Rc<RefCell<Option<Arc<AtomicBool>>>> = Rc::default();
    let exporting = Rc::new(Cell::new(false));
    for (button, replace_source) in [(save_new.clone(), false), (replace.clone(), true)] {
        let state = state.clone();
        let toasts = toasts.clone();
        let timeline = timeline.clone();
        let clip = clip.clone();
        let precise_row = precise_row.clone();
        let (progress, cancel, save_new, replace) =
            (progress.clone(), cancel.clone(), save_new.clone(), replace.clone());
        let cancel_flag = cancel_flag.clone();
        let exporting = exporting.clone();
        let page = page.clone();
        let media = media.clone();
        let refresh_tools = refresh_tools.clone();
        button.connect_clicked(move |_| {
            if exporting.get() {
                return;
            }
            let src = clip.borrow().clone();
            let req = ExportRequest {
                source_id: src.id.clone(),
                keep: timeline.parts(),
                title: if replace_source { src.title.clone() } else { format!("{} (edit)", src.title) },
                precise: precise_row.is_active(),
                replace_source,
            };
            let run = {
                let state = state.clone();
                let toasts = toasts.clone();
                let (progress, cancel, save_new, replace) =
                    (progress.clone(), cancel.clone(), save_new.clone(), replace.clone());
                let cancel_flag = cancel_flag.clone();
                let exporting = exporting.clone();
                let page = page.clone();
                let media = media.clone();
                let refresh_tools = refresh_tools.clone();
                move || {
                    exporting.set(true);
                    if replace_source {
                        media.set_playing(false);
                    }
                    save_new.set_visible(false);
                    replace.set_visible(false);
                    progress.set_visible(true);
                    progress.set_fraction(0.0);
                    progress.set_text(Some("Exporting"));
                    cancel.set_visible(true);
                    let flag = Arc::new(AtomicBool::new(false));
                    *cancel_flag.borrow_mut() = Some(flag.clone());

                    let (tx, rx) = async_channel::unbounded::<f32>();
                    let lib = state.library();
                    let worker = gio::spawn_blocking(move || {
                        let mut report = |p: f32| {
                            let _ = tx.send_blocking(p);
                        };
                        edit::export(&lib, &req, &mut report, &flag)
                    });
                    let progress_ui = progress.clone();
                    glib::spawn_future_local(async move {
                        while let Ok(p) = rx.recv().await {
                            progress_ui.set_fraction(p.clamp(0.0, 1.0) as f64);
                            progress_ui.set_text(Some(&format!("Exporting {:.0}%", p * 100.0)));
                        }
                    });
                    let state = state.clone();
                    let toasts = toasts.clone();
                    let cancel_flag = cancel_flag.clone();
                    let exporting = exporting.clone();
                    let page = page.clone();
                    let (progress, cancel, save_new, replace) =
                        (progress.clone(), cancel.clone(), save_new.clone(), replace.clone());
                    let refresh_tools = refresh_tools.clone();
                    glib::spawn_future_local(async move {
                        let result = worker
                            .await
                            .unwrap_or_else(|_| Err(anyhow::anyhow!("export panicked")));
                        let cancelled = cancel_flag
                            .borrow_mut()
                            .take()
                            .is_some_and(|f| f.load(Ordering::Relaxed));
                        exporting.set(false);
                        progress.set_visible(false);
                        cancel.set_visible(false);
                        save_new.set_visible(true);
                        replace.set_visible(true);
                        refresh_tools();
                        match result {
                            Ok(new_clip) => {
                                state.library_changed();
                                let toast = util::toast(&format!("Saved \u{201c}{}\u{201d}", new_clip.title));
                                toasts.add_toast(toast);
                                pop(&page);
                            }
                            Err(_) if cancelled => toasts.add_toast(util::toast("Export cancelled")),
                            Err(e) => toasts.add_toast(util::toast(&format!("Export failed: {e}"))),
                        }
                    });
                }
            };

            if replace_source {
                let dialog = adw::AlertDialog::new(
                    Some("Replace the original clip?"),
                    Some("The cut parts are removed from the file for good."),
                );
                dialog.add_response("cancel", "Cancel");
                dialog.add_response("replace", "Replace");
                dialog.set_response_appearance("replace", adw::ResponseAppearance::Destructive);
                dialog.set_close_response("cancel");
                let page = page.clone();
                glib::spawn_future_local(async move {
                    if dialog.choose_future(Some(&page)).await == "replace" {
                        run();
                    }
                });
            } else {
                run();
            }
        });
    }
    {
        let cancel_flag = cancel_flag.clone();
        cancel.connect_clicked(move |b| {
            if let Some(flag) = cancel_flag.borrow().as_ref() {
                flag.store(true, Ordering::Relaxed);
                b.set_sensitive(false);
            }
        });
    }
    {
        let cancel = cancel.clone();
        progress.connect_visible_notify(move |p| {
            if p.is_visible() {
                cancel.set_sensitive(true);
            }
        });
    }

    // Start with focus on the timeline so Space and the edit keys work right away.
    {
        let area = timeline.widget().clone();
        page.connect_shown(move |_| {
            area.grab_focus();
        });
    }

    // Free the decoder as soon as the page is closed.
    {
        let cancel_flag = cancel_flag.clone();
        page.connect_hidden(move |page| {
            if page.parent().is_none() || !page.is_mapped() {
                media.set_playing(false);
            }
            let _ = &cancel_flag;
        });
    }
    {
        let picture = picture.clone();
        page.connect_unrealize(move |_| {
            picture.set_paintable(None::<&gdk::Paintable>);
        });
    }

    page
}

fn pop(page: &adw::NavigationPage) {
    if let Some(nav) = page.ancestor(adw::NavigationView::static_type()).and_downcast::<adw::NavigationView>() {
        nav.pop();
    }
}

fn subtitle(clip: &ClipMeta) -> String {
    let mut parts = Vec::new();
    if let Some(app) = &clip.app {
        parts.push(app.name.clone());
    }
    parts.push(util::date(&clip.created_at));
    parts.join(" · ")
}

fn star_icon(on: bool) -> &'static str {
    if on {
        "starred-symbolic"
    } else {
        "non-starred-symbolic"
    }
}

fn fill_info(group: &adw::PreferencesGroup, clip: &ClipMeta) {
    let mut rows: Vec<(&str, String)> = Vec::new();
    if let Some(app) = &clip.app {
        rows.push(("App", app.name.clone()));
        if let Some(t) = app.window_title.as_ref().filter(|t| **t != app.name) {
            rows.push(("Window", t.clone()));
        }
    }
    rows.push(("Date", clip.created_at.format("%A, %B %-d %Y, %H:%M:%S").to_string()));
    rows.push(("Length", util::duration_precise(clip.duration_secs)));
    rows.push(("Video", format!("{}×{}, {} fps, {}", clip.width, clip.height, clip.fps, util::codec_name(&clip.codec))));
    rows.push(("Size", util::size(clip.size_bytes)));
    if let Some(src) = &clip.edited_from {
        rows.push(("Cut from", src.clone()));
    }
    for (title, value) in rows {
        let row = adw::ActionRow::builder()
            .title(title)
            .subtitle(glib::markup_escape_text(&value).as_str())
            .subtitle_selectable(true)
            .build();
        row.add_css_class("property");
        group.add(&row);
    }
}

fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    let b = gtk::Button::builder().icon_name(icon).tooltip_text(tooltip).build();
    b.add_css_class("flat");
    b
}

fn labeled_button(icon: &str, label: &str, tooltip: &str) -> gtk::Button {
    let content = adw::ButtonContent::builder().icon_name(icon).label(label).build();
    let b = gtk::Button::builder().child(&content).tooltip_text(tooltip).build();
    b.add_css_class("flat");
    b
}
