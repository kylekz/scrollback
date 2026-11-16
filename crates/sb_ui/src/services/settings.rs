//! The app's own settings (PLAN §11 `Settings`, DESIGN §4.13): where the archive is, light or
//! dark, the window material and how often to sync. They live with the app, in
//! `<data dir>/settings.json`, outside the archive (decision 4), so `--data-dir` and
//! `SCROLLBACK_DATA_DIR` isolate them with everything else. What belongs to an archive (how
//! soon a location is stale, what verify found) is in its catalogue's `kv` instead and goes
//! with the archive.
//!
//! **The file** is `{"v": 1, "archive": …, "mode": …, "material": …, "sync_every": …}`, as
//! dv keeps its own (`archive` only once another folder was chosen). It is read leniently: a
//! missing key takes its default, a value that can't be read is logged and takes its default,
//! and keys this version doesn't know are written back as they were. A file that is there
//! and can't be read at all is never written over this session: it may be a newer
//! scrollback's. A change is written at once in the background through a temporary file and a
//! rename, so a crash never leaves half a file; one made while a write runs is written after
//! it, and what is unwritten when the app quits is written then.
//!
//! **The service** also says whether Settings is shown in the main area, and carries out
//! **Use a different folder…** ([`SettingsService::use_folder`]): the archive there is opened,
//! or made when the folder holds nothing (PLAN §4.4), and only once it is open is the app
//! moved onto it ([`super::reopen`]); a folder that holds anything else is refused. Nothing is
//! moved, and the archive left stays as it is.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gpui::{AppContext as _, Context, EventEmitter, Subscription, Task};
use sb_core::archive::{self, Archive};
use serde_json::{Map, Value, json};

use super::ActiveServices as _;
use crate::appearance::{Mode, default_material};
use crate::theme::Material;

/// The settings file in the data dir.
pub const SETTINGS_FILE: &str = "settings.json";
/// The archive's folder in the data dir, unless the owner chose another (PLAN D3, §4.4).
pub const ARCHIVE_FOLDER: &str = "archive";
/// The `"v"` this version writes.
const VERSION: u64 = 1;

/// How often every location is synced while the app is open (DESIGN §4.13).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SyncEvery {
    Five,
    #[default]
    Fifteen,
    Thirty,
    Sixty,
    /// Only at the start and when the owner asks.
    Asked,
}

impl SyncEvery {
    /// In the order the menu lists them.
    pub const ALL: [Self; 5] = [
        Self::Five,
        Self::Fifteen,
        Self::Thirty,
        Self::Sixty,
        Self::Asked,
    ];

    /// How long between rounds; `None` for only when asked.
    pub fn interval(self) -> Option<Duration> {
        let minutes = match self {
            Self::Five => 5,
            Self::Fifteen => 15,
            Self::Thirty => 30,
            Self::Sixty => 60,
            Self::Asked => return None,
        };
        Some(Duration::from_secs(minutes * 60))
    }

    /// As the menu says it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Five => "5 minutes",
            Self::Fifteen => "15 minutes",
            Self::Thirty => "30 minutes",
            Self::Sixty => "60 minutes",
            Self::Asked => "Only when I ask",
        }
    }

    /// As the file spells it.
    pub fn key(self) -> &'static str {
        match self {
            Self::Five => "5m",
            Self::Fifteen => "15m",
            Self::Thirty => "30m",
            Self::Sixty => "60m",
            Self::Asked => "asked",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|every| every.key() == key)
    }
}

/// The settings file's contents.
#[derive(Clone, Debug, PartialEq)]
pub struct AppSettings {
    /// The archive's folder, once the owner chose one; until then the data dir's.
    pub archive: Option<PathBuf>,
    pub mode: Mode,
    /// The material asked for; the platform may show None instead (`crate::appearance`).
    pub material: Material,
    pub sync_every: SyncEvery,
    /// Keys this version doesn't know, written back as they were read.
    extra: Map<String, Value>,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            archive: None,
            mode: Mode::System,
            material: default_material(),
            sync_every: SyncEvery::default(),
            extra: Map::new(),
        }
    }
}

impl AppSettings {
    /// Reads `text` leniently (the module docs): the settings, and a line for each thing that
    /// couldn't be read.
    pub fn parse(text: &str) -> (Self, Vec<String>) {
        let mut settings = Self::default();
        let mut problems = Vec::new();
        let mut object = match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(object)) => object,
            _ => {
                problems.push("the file isn't a JSON object; using the defaults".to_string());
                return (settings, problems);
            }
        };
        if let Some(v) = object.remove("v")
            && v.as_u64().is_none_or(|v| v > VERSION)
        {
            problems.push(format!("\"v\" is {v}; reading the keys this version knows"));
        }
        let mut read = |key: &str, ok: &mut dyn FnMut(&str) -> bool| {
            if let Some(value) = object.remove(key)
                && !value.as_str().is_some_and(&mut *ok)
            {
                problems.push(format!("\"{key}\" is {value}; using its default"));
            }
        };
        read("archive", &mut |path| {
            settings.archive = (!path.is_empty()).then(|| PathBuf::from(path));
            settings.archive.is_some()
        });
        read("mode", &mut |key| {
            Mode::from_key(key)
                .map(|mode| settings.mode = mode)
                .is_some()
        });
        read("material", &mut |key| {
            Material::from_key(key)
                .map(|material| settings.material = material)
                .is_some()
        });
        read("sync_every", &mut |key| {
            SyncEvery::from_key(key)
                .map(|every| settings.sync_every = every)
                .is_some()
        });
        settings.extra = object;
        (settings, problems)
    }

    /// The file's text: pretty JSON with a trailing newline, `v` and the known keys first.
    pub fn to_json(&self) -> String {
        let mut object = Map::new();
        object.insert("v".to_string(), json!(VERSION));
        if let Some(archive) = &self.archive {
            object.insert("archive".to_string(), json!(archive.display().to_string()));
        }
        object.insert("mode".to_string(), json!(self.mode.key()));
        object.insert("material".to_string(), json!(self.material.key()));
        object.insert("sync_every".to_string(), json!(self.sync_every.key()));
        object.extend(self.extra.clone());
        let mut text = serde_json::to_string_pretty(&Value::Object(object))
            .expect("a map of text always serializes");
        text.push('\n');
        text
    }
}

/// What [`load`] read from a data dir.
#[derive(Clone, Debug)]
pub struct Loaded {
    pub data_dir: PathBuf,
    pub settings: AppSettings,
    /// Set when a file is there that couldn't be read: it is not written over this session,
    /// and this says why.
    pub unsaveable: Option<String>,
}

/// Reads the settings in `data_dir`: the defaults when there is no file, or one that can't be
/// read (each problem logged).
pub fn load(data_dir: &Path) -> Loaded {
    let path = data_dir.join(SETTINGS_FILE);
    let loaded = |settings, unsaveable| Loaded {
        data_dir: data_dir.to_path_buf(),
        settings,
        unsaveable,
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return loaded(AppSettings::default(), None);
        }
        Err(error) => {
            log::warn!("{} couldn't be read: {error}", path.display());
            return loaded(
                AppSettings::default(),
                Some(format!("{} couldn't be read ({error})", path.display())),
            );
        }
    };
    let unsaveable = match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(_)) => None,
        _ => Some(format!(
            "{} isn't a settings file this scrollback can read",
            path.display()
        )),
    };
    let (settings, problems) = AppSettings::parse(&text);
    for problem in problems {
        log::warn!("{}: {problem}", path.display());
    }
    loaded(settings, unsaveable)
}

/// Writes `contents` to `path` through a temporary file beside it and a rename, so a reader sees
/// the old file or the new one, never part of one.
fn write_atomically(path: &Path, contents: &str) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let temporary = tempfile::NamedTempFile::new_in(dir)?;
    temporary.as_file().write_all(contents.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

/// A different folder being opened as the archive (the module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Switch {
    /// The archive there is being opened or made, and then the app moved onto it.
    Opening(PathBuf),
    /// It was refused, and why: the app stays on the archive it has.
    Refused { folder: PathBuf, why: String },
}

/// What the service tells its subscribers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsEvent {
    /// A setting changed.
    Changed,
}

pub struct SettingsService {
    settings: AppSettings,
    path: PathBuf,
    data_dir: PathBuf,
    unsaveable: Option<String>,
    /// Bumped by every change; the write in flight takes the newest.
    generation: u64,
    /// The newest generation written, or that failed to be.
    written: u64,
    saving: Option<Task<()>>,
    /// Why the last write failed, until one succeeds.
    save_failure: Option<String>,
    /// Whether Settings is shown in the main area.
    shown: bool,
    switch: Option<Switch>,
    _quit: Subscription,
}

impl EventEmitter<SettingsEvent> for SettingsService {}

impl SettingsService {
    pub fn new(loaded: Loaded, cx: &mut Context<Self>) -> Self {
        Self {
            path: loaded.data_dir.join(SETTINGS_FILE),
            data_dir: loaded.data_dir,
            settings: loaded.settings,
            unsaveable: loaded.unsaveable,
            generation: 0,
            written: 0,
            saving: None,
            save_failure: None,
            shown: false,
            switch: None,
            _quit: cx.on_app_quit(|this: &mut Self, _| {
                this.flush();
                async {}
            }),
        }
    }

    pub fn settings(&self) -> &AppSettings {
        &self.settings
    }

    /// The archive's folder in the data dir: the default (DESIGN §4.13's `default` tag).
    pub fn default_archive(&self) -> PathBuf {
        self.data_dir.join(ARCHIVE_FOLDER)
    }

    /// Whether `dir` is the archive's default folder, however it is spelt.
    pub fn is_default(&self, dir: &Path) -> bool {
        resolved(dir) == resolved(&self.default_archive())
    }

    /// Why the settings aren't being saved, when they aren't.
    pub fn save_failure(&self) -> Option<&str> {
        self.unsaveable.as_deref().or(self.save_failure.as_deref())
    }

    pub fn is_shown(&self) -> bool {
        self.shown
    }

    /// Shows Settings in the main area, or goes back from it.
    pub fn show(&mut self, shown: bool, cx: &mut Context<Self>) {
        if self.shown != shown {
            self.shown = shown;
            cx.notify();
        }
    }

    pub fn switch(&self) -> Option<&Switch> {
        self.switch.as_ref()
    }

    /// Nothing is being written or switched to: automation's `settled`.
    pub fn is_settled(&self) -> bool {
        self.saving.is_none() && !matches!(self.switch, Some(Switch::Opening(_)))
    }

    pub fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        if self.settings.mode != mode {
            self.settings.mode = mode;
            self.apply_appearance(cx);
        }
    }

    pub fn set_material(&mut self, material: Material, cx: &mut Context<Self>) {
        if self.settings.material != material {
            self.settings.material = material;
            self.apply_appearance(cx);
        }
    }

    fn apply_appearance(&mut self, cx: &mut Context<Self>) {
        gpui_appearance::set_preferences(
            gpui_appearance::Preferences {
                mode: self.settings.mode,
                material: self.settings.material,
            },
            cx,
        );
        self.changed(cx);
    }

    pub fn set_sync_every(&mut self, every: SyncEvery, cx: &mut Context<Self>) {
        if self.settings.sync_every != every {
            self.settings.sync_every = every;
            self.changed(cx);
        }
    }

    /**
     * Opens the archive in `folder`, or makes one there when it holds nothing, and moves the
     * app onto it (the module docs). The folder the app is on already, and one inside it, are
     * refused; so is one that holds anything but an archive, and one whose archive doesn't
     * open: the app then stays where it is.
     */
    pub fn use_folder(&mut self, folder: PathBuf, cx: &mut Context<Self>) {
        if matches!(self.switch, Some(Switch::Opening(_))) {
            return;
        }
        let current = resolved(cx.services().archive.read(cx).dir());
        let refused = |why: &str| Switch::Refused {
            folder: folder.clone(),
            why: why.to_string(),
        };
        let chosen = resolved(&folder);
        if chosen == current {
            self.switch = Some(refused("That is the archive open now."));
            cx.notify();
            return;
        }
        if chosen.starts_with(&current) {
            self.switch = Some(refused(
                "That folder is inside the archive open now. Choose one outside it.",
            ));
            cx.notify();
            return;
        }
        self.switch = Some(Switch::Opening(folder.clone()));
        cx.notify();
        let opening = cx.background_spawn({
            let folder = folder.clone();
            async move { archive::open_or_make(&folder) }
        });
        cx.spawn(async move |this, cx| {
            let opened = opening.await;
            this.update(cx, |this, cx| match opened {
                Ok(archive) => this.reopen(folder, archive, cx),
                Err(error) => {
                    log::warn!("{} wasn't opened as the archive: {error}", folder.display());
                    this.switch = Some(Switch::Refused {
                        why: refusal(&folder, &error),
                        folder,
                    });
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Moves the app onto `archive`, opened in `folder`, and keeps that as the archive's
    /// folder once it is there.
    fn reopen(&mut self, folder: PathBuf, archive: Archive, cx: &mut Context<Self>) {
        let moving = super::reopen(Arc::new(archive), cx);
        cx.spawn(async move |this, cx| {
            moving.await;
            this.update(cx, |this, cx| {
                this.switch = None;
                this.shown = false;
                this.settings.archive = (!this.is_default(&folder)).then_some(folder);
                this.changed(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Forgets a refused switch: the owner has read why.
    pub fn dismiss_switch(&mut self, cx: &mut Context<Self>) {
        if matches!(self.switch, Some(Switch::Refused { .. })) {
            self.switch = None;
            cx.notify();
        }
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        cx.emit(SettingsEvent::Changed);
        cx.notify();
        self.save(cx);
    }

    /// Writes the settings as they are now unless a write runs; that one writes again when it
    /// ends, if they changed meanwhile.
    fn save(&mut self, cx: &mut Context<Self>) {
        if self.saving.is_some() || self.written == self.generation || self.unsaveable.is_some() {
            return;
        }
        let (path, generation, text) =
            (self.path.clone(), self.generation, self.settings.to_json());
        let write = cx.background_spawn(async move { write_atomically(&path, &text) });
        self.saving = Some(cx.spawn(async move |this, cx| {
            let written = write.await;
            this.update(cx, |this, cx| {
                this.saving = None;
                this.written = generation;
                this.save_failure = written.err().map(|error| {
                    log::error!("the settings couldn't be saved: {error}");
                    format!("Settings couldn't be saved: {error}.")
                });
                this.save(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Writes what is unwritten, now: the app is quitting.
    fn flush(&mut self) {
        if self.written != self.generation && self.unsaveable.is_none() {
            if let Err(error) = write_atomically(&self.path, &self.settings.to_json()) {
                log::error!("the settings couldn't be saved at quit: {error}");
            }
            self.written = self.generation;
        }
    }
}

/**
 * `path` as the file system resolves it as far as it exists (links, `..`, and on Windows the
 * case of each name), the rest as it is written: so one folder compares equal however it was
 * reached, before it is made.
 */
fn resolved(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut rest = Vec::new();
    let mut at = absolute.as_path();
    loop {
        if let Ok(found) = at.canonicalize() {
            return rest.iter().rev().fold(found, |path, name| path.join(name));
        }
        match (at.parent(), at.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                at = parent;
            }
            _ => return absolute,
        }
    }
}

/// Why `folder` wasn't taken as the archive, for the owner.
fn refusal(folder: &Path, error: &archive::ArchiveError) -> String {
    match error {
        archive::ArchiveError::NotAnArchive(_) => format!(
            "{} holds other files, so no archive is made there. Choose an archive's folder or \
             an empty one.",
            folder.display()
        ),
        archive::ArchiveError::Locked(_) => {
            "The archive there is open in another scrollback window.".to_string()
        }
        error => format!("It couldn't be opened: {error}."),
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn the_file_round_trips_and_keeps_what_it_doesnt_know() {
        let settings = AppSettings {
            archive: Some(PathBuf::from("D:/Archive")),
            mode: Mode::Dark,
            material: Material::Acrylic,
            sync_every: SyncEvery::Sixty,
            ..AppSettings::default()
        };
        let (read, problems) = AppSettings::parse(&settings.to_json());
        assert_eq!((read, problems), (settings, Vec::<String>::new()));

        let (read, _) =
            AppSettings::parse(r#"{"v": 1, "mode": "light", "window": {"x": 4}, "newer": true}"#);
        assert_eq!(read.mode, Mode::Light);
        let written: Value = serde_json::from_str(&read.to_json()).unwrap();
        assert_eq!(written["window"], json!({"x": 4}));
        assert_eq!(written["newer"], json!(true));
        /* The default folder isn't written: it is the data dir's wherever that is. */
        assert_eq!(written.get("archive"), None);
    }

    #[test]
    fn what_cant_be_read_takes_its_default() {
        let (read, problems) = AppSettings::parse(
            r#"{"v": 1, "mode": "sepia", "material": 3, "sync_every": "hourly", "archive": ""}"#,
        );
        assert_eq!(read, AppSettings::default());
        assert_eq!(problems.len(), 4, "{problems:?}");
        let (read, problems) = AppSettings::parse("[1, 2]");
        assert_eq!(read, AppSettings::default());
        assert_eq!(problems.len(), 1);
    }

    #[test]
    fn a_file_that_cant_be_read_is_never_written_over() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()).unsaveable, None);
        std::fs::write(dir.path().join(SETTINGS_FILE), "not json").unwrap();
        let loaded = load(dir.path());
        assert!(loaded.unsaveable.is_some());
        assert_eq!(loaded.settings, AppSettings::default());
    }

    #[test]
    fn every_interval_has_a_key_and_only_asked_has_none() {
        for every in SyncEvery::ALL {
            assert_eq!(SyncEvery::from_key(every.key()), Some(every));
            assert_eq!(every.interval().is_none(), every == SyncEvery::Asked);
        }
        assert_eq!(
            SyncEvery::default().interval(),
            Some(super::super::sync::SYNC_INTERVAL)
        );
    }
}
