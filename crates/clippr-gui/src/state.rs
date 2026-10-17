//! Shared UI state: the current config and daemon status, with change callbacks.

use std::cell::RefCell;
use std::rc::Rc;

use clippr_core::clip::ClipMeta;
use clippr_core::config::Config;
use clippr_core::ipc::{CaptureState, Event, Request, Response, Status};
use clippr_core::library::Library;
use gtk::glib;

use crate::daemon::{self, Update};

type Callback = Box<dyn Fn()>;
type ClipCallback = Box<dyn Fn(&ClipMeta)>;

pub struct State {
    config: RefCell<Config>,
    /// `None` while the daemon is unreachable.
    status: RefCell<Option<Status>>,
    on_change: RefCell<Vec<Callback>>,
    on_clip: RefCell<Vec<ClipCallback>>,
    on_library: RefCell<Vec<Callback>>,
}

impl State {
    pub fn new() -> Rc<Self> {
        let config = Config::load().unwrap_or_else(|e| {
            eprintln!("clippr-gui: config unreadable, using defaults: {e:#}");
            Config::default()
        });
        Rc::new(Self {
            config: RefCell::new(config),
            status: RefCell::new(None),
            on_change: RefCell::default(),
            on_clip: RefCell::default(),
            on_library: RefCell::default(),
        })
    }

    /// Start following the daemon. Call after the UI has registered its callbacks.
    pub fn start(self: &Rc<Self>) {
        let rx = daemon::watch();
        let this = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(update) = rx.recv().await {
                let Some(this) = this.upgrade() else { break };
                this.handle(update);
            }
        });
    }

    fn handle(&self, update: Update) {
        match update {
            Update::Online { status, config } => {
                *self.status.borrow_mut() = Some(status);
                *self.config.borrow_mut() = config;
            }
            Update::Offline => *self.status.borrow_mut() = None,
            Update::Event(Event::StatusChanged { status }) => {
                *self.status.borrow_mut() = Some(status)
            }
            Update::Event(Event::ConfigChanged { config }) => {
                *self.config.borrow_mut() = config
            }
            Update::Event(Event::ClipSaved { clip }) => {
                self.clip_saved(&clip);
                return;
            }
        }
        self.notify();
    }

    pub fn config(&self) -> Config {
        self.config.borrow().clone()
    }

    pub fn status(&self) -> Option<Status> {
        self.status.borrow().clone()
    }

    pub fn online(&self) -> bool {
        self.status.borrow().is_some()
    }

    pub fn paused(&self) -> bool {
        matches!(self.status.borrow().as_ref().map(|s| s.state), Some(CaptureState::Paused))
    }

    pub fn library(&self) -> Library {
        Library::open(self.config.borrow().general.clips_dir.clone())
    }

    pub fn connect_changed(&self, f: impl Fn() + 'static) {
        self.on_change.borrow_mut().push(Box::new(f));
    }

    pub fn connect_clip_saved(&self, f: impl Fn(&ClipMeta) + 'static) {
        self.on_clip.borrow_mut().push(Box::new(f));
    }

    /// Called when clips were added, edited or removed outside the daemon.
    pub fn connect_library_changed(&self, f: impl Fn() + 'static) {
        self.on_library.borrow_mut().push(Box::new(f));
    }

    pub fn library_changed(&self) {
        for f in self.on_library.borrow().iter() {
            f();
        }
    }

    fn notify(&self) {
        for f in self.on_change.borrow().iter() {
            f();
        }
    }

    pub fn clip_saved(&self, clip: &ClipMeta) {
        for f in self.on_clip.borrow().iter() {
            f(clip);
        }
    }

    /// Persist and apply a config. Goes through the daemon when it runs so it can rebuild the
    /// pipeline; otherwise writes the file directly.
    pub async fn apply_config(&self, config: Config) -> anyhow::Result<()> {
        if self.online() {
            daemon::call(Request::SetConfig { config: config.clone() }).await?;
        } else {
            let to_save = config.clone();
            gtk::gio::spawn_blocking(move || to_save.save())
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("config writer panicked")))?;
        }
        *self.config.borrow_mut() = config;
        self.notify();
        Ok(())
    }

    pub async fn save_clip(&self, seconds: Option<u32>) -> anyhow::Result<ClipMeta> {
        match daemon::call(Request::Save { seconds }).await? {
            Response::Saved { clip } => {
                self.clip_saved(&clip);
                Ok(clip)
            }
            other => anyhow::bail!("unexpected reply: {other:?}"),
        }
    }

    pub async fn set_paused(&self, paused: bool) -> anyhow::Result<()> {
        daemon::call(Request::SetPaused { paused }).await.map(drop)
    }
}
