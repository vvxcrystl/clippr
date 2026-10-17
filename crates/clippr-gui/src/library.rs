//! The Clips tab: recorder status, search and filters, and a lazily rendered grid of clips.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use clippr_core::clip::ClipMeta;
use clippr_core::library::Library;
use gtk::{gdk, gio, glib};

use crate::state::State;
use crate::util;

pub fn build(
    state: &Rc<State>,
    toasts: &adw::ToastOverlay,
    open_editor: Rc<dyn Fn(ClipMeta)>,
) -> gtk::Widget {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 0);

    let banner = adw::Banner::builder()
        .title("The recorder isn't running, so nothing is being buffered")
        .button_label("Start recorder")
        .build();
    banner.connect_button_clicked(|_| util::start_daemon());
    page.append(&banner);

    page.append(&status_strip(state, toasts));

    // Filters
    let filters = Rc::new(Filters::default());
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search titles, apps and tags")
        .hexpand(true)
        .build();
    let apps = gtk::StringList::new(&["All apps"]);
    let app_drop = gtk::DropDown::builder().model(&apps).tooltip_text("Filter by app").build();
    let sort_drop = gtk::DropDown::from_strings(&["Newest", "Oldest", "Longest", "Largest"]);
    sort_drop.set_tooltip_text(Some("Sort order"));
    let fav_toggle = gtk::ToggleButton::builder()
        .icon_name("starred-symbolic")
        .tooltip_text("Favorites only")
        .build();

    let filter_bar = gtk::Box::builder()
        .spacing(8)
        .margin_start(18)
        .margin_end(18)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    filter_bar.append(&search);
    filter_bar.append(&app_drop);
    filter_bar.append(&sort_drop);
    filter_bar.append(&fav_toggle);
    page.append(&filter_bar);

    // Model: store → filter → sort → grid
    let store = gio::ListStore::new::<glib::BoxedAnyObject>();
    let filter = {
        let filters = filters.clone();
        gtk::CustomFilter::new(move |obj| {
            let item = obj.downcast_ref::<glib::BoxedAnyObject>().expect("clip item");
            filters.matches(&item.borrow::<ClipMeta>())
        })
    };
    let sort = Rc::new(Cell::new(0u32));
    let sorter = {
        let sort = sort.clone();
        gtk::CustomSorter::new(move |a, b| {
            let a = a.downcast_ref::<glib::BoxedAnyObject>().expect("clip item");
            let b = b.downcast_ref::<glib::BoxedAnyObject>().expect("clip item");
            let (a, b) = (a.borrow::<ClipMeta>(), b.borrow::<ClipMeta>());
            let order = match sort.get() {
                1 => a.created_at.cmp(&b.created_at),
                2 => b.duration_secs.total_cmp(&a.duration_secs),
                3 => b.size_bytes.cmp(&a.size_bytes),
                _ => b.created_at.cmp(&a.created_at),
            };
            order.into()
        })
    };
    let filtered = gtk::FilterListModel::new(Some(store.clone()), Some(filter.clone()));
    let sorted = gtk::SortListModel::new(Some(filtered.clone()), Some(sorter.clone()));
    let selection = gtk::NoSelection::new(Some(sorted.clone()));

    let thumbs = Thumbnailer::new();
    let factory = gtk::SignalListItemFactory::new();
    let menu_ctx = Rc::new(CardContext {
        state: state.clone(),
        toasts: toasts.clone(),
        open_editor: open_editor.clone(),
    });
    factory.connect_setup(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("list item");
        let card = ClipCard::default();
        card.install_menu(menu_ctx.clone());
        item.set_child(Some(&card));
    });
    {
        let thumbs = thumbs.clone();
        let state = state.clone();
        factory.connect_bind(move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().expect("list item");
            let card = item.child().and_downcast::<ClipCard>().expect("clip card");
            let obj = item.item().and_downcast::<glib::BoxedAnyObject>().expect("clip item");
            let clip = obj.borrow::<ClipMeta>().clone();
            card.bind(&clip, &thumbs, state.config().general.clips_dir);

            // Debug builds: CLIPPR_GUI_OPEN_MENU opens the first card's menu for layout checks.
            #[cfg(debug_assertions)]
            if item.position() == 0 && std::env::var_os("CLIPPR_GUI_OPEN_MENU").is_some() {
                let card = card.clone();
                glib::timeout_add_local_once(std::time::Duration::from_millis(1200), move || {
                    // Wayland drops grabbing popups that no real input event triggered.
                    if let Some(menu) = card.imp().menu.borrow().as_ref() {
                        menu.set_autohide(false);
                    }
                    card.popup_menu(40.0, 40.0)
                });
            }
        });
    }

    let grid = gtk::GridView::builder()
        .model(&selection)
        .factory(&factory)
        .single_click_activate(true)
        .max_columns(8)
        .min_columns(1)
        .build();
    grid.add_css_class("clip-grid");
    {
        let sorted = sorted.clone();
        grid.connect_activate(move |_, pos| {
            if let Some(obj) = sorted.item(pos).and_downcast::<glib::BoxedAnyObject>() {
                open_editor(obj.borrow::<ClipMeta>().clone());
            }
        });
    }
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&grid)
        .build();

    let empty = adw::StatusPage::builder()
        .icon_name("camera-video-symbolic")
        .title("No clips yet")
        .build();
    let no_match = adw::StatusPage::builder()
        .icon_name("system-search-symbolic")
        .title("No matching clips")
        .description("Try a different search or filter")
        .build();

    let content = gtk::Stack::new();
    content.add_named(&scroller, Some("grid"));
    content.add_named(&empty, Some("empty"));
    content.add_named(&no_match, Some("no-match"));
    page.append(&content);

    // Footer
    let summary = gtk::Label::builder().xalign(0.0).hexpand(true).build();
    summary.add_css_class("dim-label");
    summary.add_css_class("caption");
    let open_folder = gtk::Button::builder()
        .icon_name("folder-open-symbolic")
        .tooltip_text("Open clips folder")
        .build();
    open_folder.add_css_class("flat");
    let refresh = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Reload")
        .build();
    refresh.add_css_class("flat");
    let footer = gtk::Box::builder()
        .spacing(4)
        .margin_start(18)
        .margin_end(12)
        .margin_top(4)
        .margin_bottom(4)
        .build();
    footer.append(&summary);
    footer.append(&open_folder);
    footer.append(&refresh);
    page.append(&footer);

    let update_view = {
        let content = content.clone();
        let store = store.clone();
        let filtered = filtered.clone();
        let summary = summary.clone();
        let empty = empty.clone();
        let state = state.clone();
        move || {
            let total = store.n_items();
            let shown = filtered.n_items();
            content.set_visible_child_name(match (total, shown) {
                (0, _) => "empty",
                (_, 0) => "no-match",
                _ => "grid",
            });
            let cfg = state.config();
            // Status page descriptions are markup, and hotkeys like "<Super>F8" contain '<'.
            empty.set_description(Some(&glib::markup_escape_text(&format!(
                "Press {} to save the last {}, or {} for the last {}",
                cfg.hotkeys.clip_short,
                util::clip_length(cfg.general.clip_short_secs),
                cfg.hotkeys.clip_long,
                util::clip_length(cfg.general.clip_long_secs),
            ))));
            let bytes: u64 = (0..total)
                .filter_map(|i| store.item(i).and_downcast::<glib::BoxedAnyObject>())
                .map(|o| o.borrow::<ClipMeta>().size_bytes)
                .sum();
            let count = if shown == total {
                format!("{total} clips")
            } else {
                format!("{shown} of {total} clips")
            };
            summary.set_label(&format!("{count} · {}", util::size(bytes)));
        }
    };
    let update_view = Rc::new(update_view);

    // Reload from disk. Generation counter drops results of superseded loads.
    let generation = Rc::new(Cell::new(0u64));
    let reload: Rc<dyn Fn()> = {
        let state = state.clone();
        let store = store.clone();
        let apps = apps.clone();
        let app_drop = app_drop.clone();
        let filters = filters.clone();
        let update_view = update_view.clone();
        let toasts = toasts.clone();
        Rc::new(move || {
            let lib = state.library();
            let gen = generation.get() + 1;
            generation.set(gen);
            let generation = generation.clone();
            let store = store.clone();
            let apps = apps.clone();
            let app_drop = app_drop.clone();
            let filters = filters.clone();
            let update_view = update_view.clone();
            let toasts = toasts.clone();
            glib::spawn_future_local(async move {
                let result = gio::spawn_blocking(move || lib.list())
                    .await
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("library scan panicked")));
                if generation.get() != gen {
                    return;
                }
                let clips = match result {
                    Ok(clips) => clips,
                    Err(e) => {
                        toasts.add_toast(util::toast(&format!("Couldn't read clips: {e}")));
                        Vec::new()
                    }
                };
                let mut names: Vec<String> =
                    clips.iter().filter_map(|c| c.app.as_ref().map(|a| a.name.clone())).collect();
                names.sort_by_key(|n| n.to_lowercase());
                names.dedup();
                let items: Vec<glib::BoxedAnyObject> =
                    clips.into_iter().map(glib::BoxedAnyObject::new).collect();
                store.splice(0, store.n_items(), &items);

                // Rebuild the app list, keeping the current choice if it still exists.
                let current = filters.app.borrow().clone();
                let mut entries = vec!["All apps".to_string()];
                entries.extend(names);
                let refs: Vec<&str> = entries.iter().map(String::as_str).collect();
                apps.splice(0, apps.n_items(), &refs);
                let index = current
                    .and_then(|name| entries.iter().position(|e| *e == name))
                    .unwrap_or(0);
                app_drop.set_selected(index as u32);
                update_view();
            });
        })
    };

    // Filter wiring
    let refilter = {
        let filter = filter.clone();
        let update_view = update_view.clone();
        move || {
            filter.changed(gtk::FilterChange::Different);
            update_view();
        }
    };
    {
        let filters = filters.clone();
        let refilter = refilter.clone();
        search.connect_search_changed(move |entry| {
            *filters.query.borrow_mut() = entry.text().to_lowercase();
            refilter();
        });
    }
    {
        let filters = filters.clone();
        let refilter = refilter.clone();
        app_drop.connect_selected_notify(move |drop| {
            let name = drop
                .selected_item()
                .and_downcast::<gtk::StringObject>()
                .map(|s| s.string().to_string());
            let new = if drop.selected() == 0 { None } else { name };
            if *filters.app.borrow() != new {
                *filters.app.borrow_mut() = new;
                refilter();
            }
        });
    }
    {
        let filters = filters.clone();
        fav_toggle.connect_toggled(move |btn| {
            filters.favorites.set(btn.is_active());
            refilter();
        });
    }
    sort_drop.connect_selected_notify(move |drop| {
        sort.set(drop.selected());
        sorter.changed(gtk::SorterChange::Different);
    });

    {
        let reload = reload.clone();
        refresh.connect_clicked(move |_| reload());
    }
    {
        let state = state.clone();
        open_folder.connect_clicked(move |btn| {
            let dir = state.config().general.clips_dir;
            let _ = std::fs::create_dir_all(&dir);
            let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(dir)));
            launcher.launch(btn.root().and_downcast_ref::<gtk::Window>(), gio::Cancellable::NONE, |_| {});
        });
    }

    // Keep in sync with the daemon and the editor.
    {
        let reload = reload.clone();
        state.connect_clip_saved(move |_| reload());
    }
    {
        let reload = reload.clone();
        state.connect_library_changed(move || reload());
    }
    {
        let banner = banner.clone();
        let state_ref = state.clone();
        let last_dir: RefCell<Option<PathBuf>> = RefCell::new(None);
        let reload = reload.clone();
        let update_view = update_view.clone();
        state.connect_changed(move || {
            banner.set_revealed(!state_ref.online());
            let dir = state_ref.config().general.clips_dir;
            let changed = last_dir.borrow().as_ref() != Some(&dir);
            if changed {
                *last_dir.borrow_mut() = Some(dir);
                reload();
            } else {
                update_view();
            }
        });
    }
    banner.set_revealed(!state.online());
    reload();

    page.upcast()
}

#[derive(Default)]
struct Filters {
    query: RefCell<String>,
    app: RefCell<Option<String>>,
    favorites: Cell<bool>,
}

impl Filters {
    fn matches(&self, clip: &ClipMeta) -> bool {
        if self.favorites.get() && !clip.favorite {
            return false;
        }
        if let Some(app) = self.app.borrow().as_deref() {
            if clip.app.as_ref().map(|a| a.name.as_str()) != Some(app) {
                return false;
            }
        }
        let query = self.query.borrow();
        if query.is_empty() {
            return true;
        }
        clip.title.to_lowercase().contains(query.as_str())
            || clip.app.as_ref().is_some_and(|a| {
                a.name.to_lowercase().contains(query.as_str())
                    || a.window_title.as_ref().is_some_and(|t| t.to_lowercase().contains(query.as_str()))
            })
            || clip.tags.iter().any(|t| t.to_lowercase().contains(query.as_str()))
    }
}

/// Recorder state with pause and quick-clip buttons.
fn status_strip(state: &Rc<State>, toasts: &adw::ToastOverlay) -> gtk::Widget {
    let dot = util::status_dot();
    let title = gtk::Label::builder().xalign(0.0).build();
    title.add_css_class("heading");
    let detail = gtk::Label::builder()
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .hexpand(true)
        .build();
    detail.add_css_class("dim-label");
    detail.add_css_class("caption");

    let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
    text.append(&title);
    text.append(&detail);

    let pause = gtk::Button::builder().valign(gtk::Align::Center).build();
    pause.add_css_class("flat");
    let clip_short = gtk::Button::builder().valign(gtk::Align::Center).build();
    clip_short.add_css_class("suggested-action");
    clip_short.add_css_class("pill-small");
    let clip_long = gtk::Button::builder().valign(gtk::Align::Center).build();
    clip_long.add_css_class("pill-small");

    let strip = gtk::Box::builder()
        .spacing(12)
        .margin_start(18)
        .margin_end(18)
        .margin_top(12)
        .margin_bottom(6)
        .build();
    strip.add_css_class("status-strip");
    strip.append(&dot);
    strip.append(&text);
    strip.append(&pause);
    strip.append(&clip_long);
    strip.append(&clip_short);

    let refresh = {
        let state = state.clone();
        let (dot, title, detail) = (dot.clone(), title.clone(), detail.clone());
        let (pause, clip_short, clip_long) = (pause.clone(), clip_short.clone(), clip_long.clone());
        move || {
            let status = state.status();
            let cfg = state.config();
            util::set_dot_state(&dot, util::state_class(status.as_ref()));
            title.set_label(util::state_label(status.as_ref()));
            detail.set_label(&match &status {
                Some(s) => util::status_line(s),
                None => "Start clipprd to begin buffering".into(),
            });
            let online = status.is_some();
            let paused = state.paused();
            pause.set_visible(online);
            pause.set_icon_name(if paused {
                "media-playback-start-symbolic"
            } else {
                "media-playback-pause-symbolic"
            });
            pause.set_tooltip_text(Some(if paused { "Resume recording" } else { "Pause recording" }));
            let recording = matches!(
                status.as_ref().map(|s| s.state),
                Some(clippr_core::ipc::CaptureState::Recording)
            );
            clip_short.set_label(&format!("Clip {}", util::clip_length(cfg.general.clip_short_secs)));
            clip_long.set_label(&format!("Clip {}", util::clip_length(cfg.general.clip_long_secs)));
            clip_short.set_sensitive(recording);
            clip_long.set_sensitive(recording);
        }
    };
    refresh();
    state.connect_changed(refresh);

    {
        let state = state.clone();
        let toasts = toasts.clone();
        pause.connect_clicked(move |_| {
            let state = state.clone();
            let toasts = toasts.clone();
            glib::spawn_future_local(async move {
                if let Err(e) = state.set_paused(!state.paused()).await {
                    toasts.add_toast(util::toast(&format!("{e}")));
                }
            });
        });
    }
    for (button, long) in [(clip_short, false), (clip_long, true)] {
        let state = state.clone();
        let toasts = toasts.clone();
        button.connect_clicked(move |button| {
            let cfg = state.config();
            let secs = if long { cfg.general.clip_long_secs } else { cfg.general.clip_short_secs };
            let state = state.clone();
            let toasts = toasts.clone();
            let button = button.clone();
            button.set_sensitive(false);
            glib::spawn_future_local(async move {
                let toast = match state.save_clip(Some(secs)).await {
                    Ok(clip) => util::toast(&format!("Saved \u{201c}{}\u{201d}", clip.title)),
                    Err(e) => util::toast(&format!("Couldn't save clip: {e}")),
                };
                toasts.add_toast(toast);
                button.set_sensitive(true);
            });
        });
    }

    strip.upcast()
}

// ---------------------------------------------------------------------------------------------
// Thumbnails

/// Generates thumbnails one at a time on a single worker thread, so scrolling a large library
/// never fans out into dozens of ffmpeg processes.
#[derive(Clone)]
pub struct Thumbnailer {
    jobs: std::sync::mpsc::Sender<ThumbJob>,
    cache: Rc<RefCell<HashMap<String, gdk::Texture>>>,
}

struct ThumbJob {
    clip: ClipMeta,
    dir: PathBuf,
    reply: async_channel::Sender<Option<PathBuf>>,
}

impl Thumbnailer {
    pub fn new() -> Self {
        let (jobs, rx) = std::sync::mpsc::channel::<ThumbJob>();
        std::thread::Builder::new()
            .name("clippr-thumbs".into())
            .spawn(move || {
                for job in rx {
                    let path = Library::open(job.dir).thumbnail(&job.clip).ok();
                    let _ = job.reply.send_blocking(path);
                }
            })
            .expect("spawn thumbnail thread");
        Self { jobs, cache: Rc::default() }
    }

    pub async fn get(&self, clip: &ClipMeta, dir: PathBuf) -> Option<gdk::Texture> {
        // Size changes when a clip is replaced by an edit, which invalidates the old frame.
        let key = format!("{}:{}", clip.id, clip.size_bytes);
        if let Some(tex) = self.cache.borrow().get(&key) {
            return Some(tex.clone());
        }
        let (reply, rx) = async_channel::bounded(1);
        self.jobs.send(ThumbJob { clip: clip.clone(), dir, reply }).ok()?;
        let path = rx.recv().await.ok().flatten()?;
        let tex = gdk::Texture::from_filename(path).ok()?;
        self.cache.borrow_mut().insert(key, tex.clone());
        Some(tex)
    }
}

// ---------------------------------------------------------------------------------------------
// Clip card

const THUMB_W: i32 = 288;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct ClipCard {
        pub picture: gtk::Picture,
        pub duration: gtk::Label,
        pub star: gtk::Image,
        pub icon: gtk::Image,
        pub title: gtk::Label,
        pub subtitle: gtk::Label,
        pub bound_id: RefCell<String>,
        pub clip: RefCell<Option<ClipMeta>>,
        pub menu: RefCell<Option<gtk::PopoverMenu>>,
        pub actions: RefCell<Option<gio::SimpleActionGroup>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ClipCard {
        const NAME: &'static str = "ClipprClipCard";
        type Type = super::ClipCard;
        type ParentType = gtk::Box;
    }

    impl ObjectImpl for ClipCard {
        fn dispose(&self) {
            // Popovers parented with set_parent must be unparented by hand.
            if let Some(menu) = self.menu.take() {
                menu.unparent();
            }
        }

        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_orientation(gtk::Orientation::Vertical);
            obj.set_spacing(8);
            obj.set_halign(gtk::Align::Center);
            obj.set_width_request(THUMB_W);
            obj.add_css_class("clip-card");

            // Fixed 16:9 thumbnails. GridView measures items without a width hint, so the
            // height can't follow the column width.
            self.picture.set_content_fit(gtk::ContentFit::Cover);
            self.picture.set_can_shrink(true);
            let frame = gtk::Overlay::new();
            frame.set_child(Some(&self.picture));
            frame.set_size_request(THUMB_W, THUMB_W * 9 / 16);
            frame.add_css_class("thumb");
            frame.set_overflow(gtk::Overflow::Hidden);

            self.duration.add_css_class("duration-badge");
            self.duration.set_halign(gtk::Align::End);
            self.duration.set_valign(gtk::Align::End);
            self.star.set_icon_name(Some("starred-symbolic"));
            self.star.add_css_class("fav-badge");
            self.star.set_halign(gtk::Align::End);
            self.star.set_valign(gtk::Align::Start);

            let overlay = gtk::Overlay::new();
            overlay.set_child(Some(&frame));
            overlay.add_overlay(&self.duration);
            overlay.add_overlay(&self.star);
            obj.append(&overlay);

            self.title.set_xalign(0.0);
            self.title.set_ellipsize(gtk::pango::EllipsizeMode::End);
            self.title.add_css_class("clip-title");
            self.subtitle.set_xalign(0.0);
            self.subtitle.set_ellipsize(gtk::pango::EllipsizeMode::End);
            self.subtitle.add_css_class("dim-label");
            self.subtitle.add_css_class("caption");
            let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
            text.set_hexpand(true);
            text.append(&self.title);
            text.append(&self.subtitle);

            self.icon.set_pixel_size(24);
            self.icon.set_valign(gtk::Align::Start);
            let meta = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            meta.append(&self.icon);
            meta.append(&text);
            obj.append(&meta);
        }
    }

    impl WidgetImpl for ClipCard {}
    impl BoxImpl for ClipCard {}
}

glib::wrapper! {
    pub struct ClipCard(ObjectSubclass<imp::ClipCard>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Orientable, gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for ClipCard {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl ClipCard {
    fn bind(&self, clip: &ClipMeta, thumbs: &Thumbnailer, dir: PathBuf) {
        let imp = self.imp();
        *imp.bound_id.borrow_mut() = clip.id.clone();
        *imp.clip.borrow_mut() = Some(clip.clone());
        imp.title.set_label(&clip.title);
        imp.title.set_tooltip_text(Some(&clip.title));
        imp.duration.set_label(&util::duration(clip.duration_secs));
        imp.star.set_visible(clip.favorite);

        let mut sub = Vec::new();
        if let Some(app) = &clip.app {
            sub.push(app.name.clone());
        }
        sub.push(util::date(&clip.created_at));
        imp.subtitle.set_label(&sub.join(" · "));

        match clip.app.as_ref().and_then(util::app_icon) {
            Some(icon) => imp.icon.set_from_gicon(&icon),
            None => imp.icon.set_icon_name(Some("applications-games-symbolic")),
        }

        imp.picture.set_paintable(None::<&gdk::Paintable>);
        let card = self.downgrade();
        let thumbs = thumbs.clone();
        let clip = clip.clone();
        glib::spawn_future_local(async move {
            let tex = thumbs.get(&clip, dir).await;
            let Some(card) = card.upgrade() else { return };
            // The card may have been recycled for another clip while we waited.
            if *card.imp().bound_id.borrow() == clip.id {
                card.imp().picture.set_paintable(tex.as_ref());
            }
        });
    }
}

// ---------------------------------------------------------------------------------------------
// Card context menu

type CardAction = Box<dyn Fn(&ClipCard, ClipMeta, &Rc<CardContext>)>;

/// What a card's context menu needs from the page around it.
struct CardContext {
    state: Rc<State>,
    toasts: adw::ToastOverlay,
    open_editor: Rc<dyn Fn(ClipMeta)>,
}

impl CardContext {
    /// Write a sidecar change, then refresh the library.
    fn update(self: &Rc<Self>, clip: ClipMeta) {
        let lib = self.state.library();
        let ctx = self.clone();
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || lib.update(&clip))
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("writer panicked")));
            match result {
                Ok(()) => ctx.state.library_changed(),
                Err(e) => ctx.toasts.add_toast(util::toast(&format!("Couldn't save: {e}"))),
            }
        });
    }
}

impl ClipCard {
    fn install_menu(&self, ctx: Rc<CardContext>) {
        let open = gio::Menu::new();
        open.append(Some("Open in editor"), Some("card.edit"));
        open.append(Some("Play in video player"), Some("card.play"));
        let manage = gio::Menu::new();
        manage.append(Some("Rename\u{2026}"), Some("card.rename"));
        manage.append(Some("Favorite"), Some("card.favorite"));
        manage.append(Some("Copy file"), Some("card.copy"));
        manage.append(Some("Show in folder"), Some("card.folder"));
        let danger = gio::Menu::new();
        danger.append(Some("Move to trash"), Some("card.delete"));
        let model = gio::Menu::new();
        model.append_section(None, &open);
        model.append_section(None, &manage);
        model.append_section(None, &danger);

        let menu = gtk::PopoverMenu::from_model(Some(&model));
        menu.set_parent(self);
        menu.set_has_arrow(false);
        menu.set_halign(gtk::Align::Start);
        *self.imp().menu.borrow_mut() = Some(menu);

        let actions = gio::SimpleActionGroup::new();
        let add = |name: &str, f: CardAction| {
            let action = gio::SimpleAction::new(name, None);
            let card = self.downgrade();
            let ctx = ctx.clone();
            action.connect_activate(move |_, _| {
                let Some(card) = card.upgrade() else { return };
                let Some(clip) = card.imp().clip.borrow().clone() else { return };
                f(&card, clip, &ctx);
            });
            actions.add_action(&action);
        };
        add("edit", Box::new(|_, clip, ctx| (ctx.open_editor)(clip)));
        add(
            "play",
            Box::new(|card, clip, ctx| util::open_file(card, &ctx.state.library().video_path(&clip))),
        );
        add(
            "folder",
            Box::new(|card, clip, ctx| util::show_in_folder(card, &ctx.state.library().video_path(&clip))),
        );
        add(
            "copy",
            Box::new(|card, clip, ctx| {
                if util::copy_file(card, &ctx.state.library().video_path(&clip)) {
                    ctx.toasts.add_toast(util::toast("Copied clip file"));
                }
            }),
        );
        add("rename", Box::new(|card, clip, ctx| rename_dialog(card, clip, ctx.clone())));
        add("delete", Box::new(|card, clip, ctx| delete_dialog(card, clip, ctx.clone())));

        // Checkbox item: its state mirrors the clip's favorite flag when the menu opens.
        let favorite = gio::SimpleAction::new_stateful("favorite", None, &false.to_variant());
        {
            let card = self.downgrade();
            let ctx = ctx.clone();
            favorite.connect_activate(move |action, _| {
                let Some(card) = card.upgrade() else { return };
                let Some(mut clip) = card.imp().clip.borrow().clone() else { return };
                clip.favorite = !clip.favorite;
                action.set_state(&clip.favorite.to_variant());
                ctx.update(clip);
            });
        }
        actions.add_action(&favorite);

        self.insert_action_group("card", Some(&actions));
        *self.imp().actions.borrow_mut() = Some(actions);

        let right_click = gtk::GestureClick::builder().button(gdk::BUTTON_SECONDARY).build();
        {
            let card = self.downgrade();
            right_click.connect_pressed(move |gesture, _, x, y| {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                if let Some(card) = card.upgrade() {
                    card.popup_menu(x, y);
                }
            });
        }
        self.add_controller(right_click);

        let long_press = gtk::GestureLongPress::builder().touch_only(true).build();
        {
            let card = self.downgrade();
            long_press.connect_pressed(move |gesture, x, y| {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                if let Some(card) = card.upgrade() {
                    card.popup_menu(x, y);
                }
            });
        }
        self.add_controller(long_press);
    }

    fn popup_menu(&self, x: f64, y: f64) {
        let imp = self.imp();
        let favorite = imp.clip.borrow().as_ref().is_some_and(|c| c.favorite);
        if let Some(action) = imp
            .actions
            .borrow()
            .as_ref()
            .and_then(|a| a.lookup_action("favorite"))
            .and_downcast::<gio::SimpleAction>()
        {
            action.set_state(&favorite.to_variant());
        }
        if let Some(menu) = imp.menu.borrow().as_ref() {
            menu.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            menu.popup();
        }
    }
}

fn rename_dialog(card: &ClipCard, clip: ClipMeta, ctx: Rc<CardContext>) {
    let entry = gtk::Entry::builder().text(&clip.title).activates_default(true).build();
    entry.connect_map(|e| {
        e.grab_focus();
    });
    let dialog = adw::AlertDialog::new(Some("Rename clip"), None);
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("rename", "Rename");
    dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("rename"));
    dialog.set_close_response("cancel");
    dialog.set_extra_child(Some(&entry));
    let card = card.clone();
    glib::spawn_future_local(async move {
        if dialog.choose_future(Some(&card)).await != "rename" {
            return;
        }
        let title = entry.text().trim().to_string();
        if title.is_empty() || title == clip.title {
            return;
        }
        ctx.update(ClipMeta { title, ..clip });
    });
}

fn delete_dialog(card: &ClipCard, clip: ClipMeta, ctx: Rc<CardContext>) {
    let dialog = adw::AlertDialog::new(
        Some("Move this clip to the trash?"),
        Some(&clip.title),
    );
    // The body is the clip title, which may come from an app's window class.
    dialog.set_body_use_markup(false);
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("delete", "Move to trash");
    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let card = card.clone();
    glib::spawn_future_local(async move {
        if dialog.choose_future(Some(&card)).await != "delete" {
            return;
        }
        let lib = ctx.state.library();
        let id = clip.id.clone();
        let result = gio::spawn_blocking(move || lib.delete(&id))
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("delete panicked")));
        match result {
            Ok(()) => {
                ctx.toasts.add_toast(util::toast("Moved to trash"));
                ctx.state.library_changed();
            }
            Err(e) => ctx.toasts.add_toast(util::toast(&format!("Couldn't delete: {e}"))),
        }
    });
}
