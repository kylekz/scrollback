//! The app's own settings (PLAN §11 `Settings`, DESIGN §4.13): where the archive is, light or
//! dark, the window material and how often to sync. They live with the app, in
//! `<data dir>/settings.json`, outside the archive (decision 4), so `--data-dir` and
//! `SCROLLBACK_DATA_DIR` isolate them with everything else. What belongs to an archive (how
//! soon a location is stale, what verify found) is in its catalogue's `kv` instead and goes
//! with the archive.
//!
//! **The file** is `{"v": 1, "archive": …, "mode": …, "material": …, "sync_every": …,
//! "last_archive": {"id", "opened", "sessions", "bytes"}}`, as dv keeps its own (`archive`
//! and `material` only once the owner chose them). A missing key takes its default, a value
//! that can't be read is logged and takes its default, and keys this version doesn't know are
//! written back as they were. But a file that is there and can't be read at all (not JSON, a
//! newer version's, an archive folder that isn't a full path) is not guessed at: no archive is
//! opened or made, the archive-unavailable page says why (DESIGN §4.14), and the file is not
//! written until the owner chooses an archive there. The folder is always written in full. A
//! change is written at once in the background through a temporary file and a rename, so a
//! crash never leaves half a file; one made while a write runs is written after it. A write
//! that fails is said in Settings and tried again with the next change and when the app quits.
//!
//! **The service** also says whether Settings is shown in the main area, and carries out
//! **Use a different folder…** ([`SettingsService::use_folder`]): the archive there is opened,
//! or made when the folder holds nothing (PLAN §4.4), and only once it is open is the app
//! moved onto it ([`super::reopen`]); a folder that holds anything else is refused. Nothing is
//! moved, and the archive left stays as it is.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use gpui::{AppContext as _, Context, EventEmitter, Subscription, Task};
use sb_core::archive::{self, Archive, resolved};
use serde_json::{Map, Value, json};

use super::ActiveServices as _;
use crate::appearance::Mode;
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

/// The archive the app last had open (DESIGN §4.14: what the archive-unavailable page shows):
/// its id, when it was opened, and what it held then.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LastArchive {
    pub id: String,
    /// In Unix milliseconds.
    pub opened: i64,
    pub sessions: usize,
    /// Its size on disk, once Settings has measured it.
    pub bytes: Option<u64>,
}

impl LastArchive {
    fn parse(value: &Value) -> Option<Self> {
        Some(Self {
            id: value.get("id")?.as_str()?.to_string(),
            opened: value.get("opened")?.as_i64()?,
            sessions: usize::try_from(value.get("sessions")?.as_u64()?).ok()?,
            bytes: value.get("bytes").and_then(Value::as_u64),
        })
    }

    fn to_json(&self) -> Value {
        let mut object = json!({
            "id": self.id,
            "opened": self.opened,
            "sessions": self.sessions,
        });
        if let (Some(bytes), Some(fields)) = (self.bytes, object.as_object_mut()) {
            fields.insert("bytes".to_string(), json!(bytes));
        }
        object
    }
}

/// The settings file's contents.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AppSettings {
    /// The archive's folder, once the owner chose one; until then the data dir's.
    pub archive: Option<PathBuf>,
    pub mode: Mode,
    /// The material asked for, once the owner chose one; until then the platform's best
    /// (`crate::appearance::default_material`). The platform may show None instead.
    pub material: Option<Material>,
    pub sync_every: SyncEvery,
    /// The archive last opened.
    pub last: Option<LastArchive>,
    /// Keys this version doesn't know, written back as they were read.
    extra: Map<String, Value>,
}

impl AppSettings {
    /**
     * Reads `text` (the module docs): the settings, and a line for each value that couldn't be
     * read and takes its default. `Err` says why the file can't be read at all: it isn't a JSON
     * object, it is a newer version's, or its archive folder can't be read.
     */
    pub fn parse(text: &str) -> Result<(Self, Vec<String>), String> {
        let mut settings = Self::default();
        let mut problems = Vec::new();
        let Ok(Value::Object(mut object)) = serde_json::from_str::<Value>(text) else {
            return Err("it isn't a JSON object".to_string());
        };
        match object.remove("v").map(|v| (v.as_u64(), v)) {
            None | Some((Some(VERSION), _)) => {}
            Some((Some(v), _)) if v > VERSION => {
                return Err(format!(
                    "it was written by a newer scrollback (version {v})"
                ));
            }
            Some((_, v)) => return Err(format!("its version, {v}, isn't one")),
        }
        if let Some(archive) = object.remove("archive") {
            /* A relative folder would be found from wherever the app was started. */
            match archive.as_str().map(PathBuf::from) {
                Some(path) if path.is_absolute() => settings.archive = Some(path),
                _ => return Err(format!("its archive folder, {archive}, isn't a full path")),
            }
        }
        let mut read = |key: &str, ok: &mut dyn FnMut(&str) -> bool| {
            if let Some(value) = object.remove(key)
                && !value.as_str().is_some_and(&mut *ok)
            {
                problems.push(format!("\"{key}\" is {value}; using its default"));
            }
        };
        read("mode", &mut |key| {
            Mode::from_key(key)
                .map(|mode| settings.mode = mode)
                .is_some()
        });
        read("material", &mut |key| {
            Material::from_key(key)
                .map(|material| settings.material = Some(material))
                .is_some()
        });
        read("sync_every", &mut |key| {
            SyncEvery::from_key(key)
                .map(|every| settings.sync_every = every)
                .is_some()
        });
        if let Some(last) = object.remove("last_archive") {
            settings.last = LastArchive::parse(&last);
            if settings.last.is_none() {
                problems.push(format!("\"last_archive\" is {last}; forgetting it"));
            }
        }
        settings.extra = object;
        Ok((settings, problems))
    }

    /// The file's text: pretty JSON with a trailing newline, `v` and the known keys first.
    pub fn to_json(&self) -> String {
        let mut object = Map::new();
        object.insert("v".to_string(), json!(VERSION));
        if let Some(archive) = &self.archive {
            object.insert("archive".to_string(), json!(archive.display().to_string()));
        }
        object.insert("mode".to_string(), json!(self.mode.key()));
        if let Some(material) = self.material {
            object.insert("material".to_string(), json!(material.key()));
        }
        object.insert("sync_every".to_string(), json!(self.sync_every.key()));
        if let Some(last) = &self.last {
            object.insert("last_archive".to_string(), last.to_json());
        }
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
    /**
     * Set when a file is there that can't be read, and why: no archive is opened or made, and
     * the file is not written, until the owner chooses one (the module docs).
     */
    pub unreadable: Option<String>,
}

/// Reads the settings in `data_dir`: the defaults when there is no file. Each value that
/// can't be read is logged.
pub fn load(data_dir: &Path) -> Loaded {
    let path = data_dir.join(SETTINGS_FILE);
    let read = match std::fs::read_to_string(&path) {
        Ok(text) => AppSettings::parse(&text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok((AppSettings::default(), Vec::new()))
        }
        Err(error) => Err(error.to_string()),
    };
    let (settings, unreadable) = match read {
        Ok((settings, problems)) => {
            for problem in problems {
                log::warn!("{}: {problem}", path.display());
            }
            (settings, None)
        }
        Err(why) => {
            log::error!("{} can't be read: {why}", path.display());
            (
                AppSettings::default(),
                Some(format!("{}: {why}.", path.display())),
            )
        }
    };
    Loaded {
        data_dir: data_dir.to_path_buf(),
        settings,
        unreadable,
    }
}

/**
 * Writes `contents` to `path` through a temporary file beside it and a rename, so a reader sees
 * the old file or the new one, never part of one; on Unix the folder is flushed after, so the
 * rename itself survives a crash.
 */
fn write_atomically(path: &Path, contents: &str) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let temporary = tempfile::NamedTempFile::new_in(dir)?;
    temporary.as_file().write_all(contents.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

/// Writes `text`, the settings as of `generation`, unless the file holds a later generation
/// already.
fn write_newer(on_disk: &Mutex<u64>, path: &Path, generation: u64, text: &str) -> io::Result<()> {
    let mut newest = on_disk.lock().unwrap_or_else(PoisonError::into_inner);
    if *newest < generation {
        write_atomically(path, text)?;
        *newest = generation;
    }
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
    /// Why the file can't be read, until the owner chooses an archive ([`Loaded::unreadable`]).
    unreadable: Option<String>,
    /// Light or dark for this run, whatever the settings say (`--theme`).
    mode_override: Option<Mode>,
    /// Bumped by every change; a write takes the newest.
    generation: u64,
    /// The newest generation written.
    written: u64,
    /// The generation of the file on disk, which a write takes while it writes: one that
    /// started first never replaces one that started later (the quit's, say).
    on_disk: Arc<Mutex<u64>>,
    saving: Option<Task<()>>,
    /// Why the last write failed, until one succeeds. What is unwritten is tried again with
    /// the next change, and at quit.
    save_failure: Option<String>,
    /// Whether Settings is shown in the main area.
    shown: bool,
    switch: Option<Switch>,
    _quit: Subscription,
}

impl EventEmitter<SettingsEvent> for SettingsService {}

impl SettingsService {
    /// The settings `loaded`, with `mode_override` for light or dark this run, if any.
    pub fn new(loaded: Loaded, mode_override: Option<Mode>, cx: &mut Context<Self>) -> Self {
        Self {
            path: loaded.data_dir.join(SETTINGS_FILE),
            data_dir: loaded.data_dir,
            settings: loaded.settings,
            unreadable: loaded.unreadable,
            mode_override,
            generation: 0,
            written: 0,
            on_disk: Arc::default(),
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

    /// Whether the owner chose the archive's folder in Settings: then Settings can be opened
    /// on a first run too, to choose another (DESIGN §4.13).
    pub fn is_chosen(&self) -> bool {
        self.settings.archive.is_some()
    }

    /// Whether `dir` is the archive's default folder, however it is spelt.
    pub fn is_default(&self, dir: &Path) -> bool {
        resolved(dir) == resolved(&self.default_archive())
    }

    /// Why the settings file can't be read, while nothing has been chosen in its place.
    pub fn unreadable(&self) -> Option<&str> {
        self.unreadable.as_deref()
    }

    /// Why the last change to the settings wasn't saved, until one is.
    pub fn save_failure(&self) -> Option<&str> {
        self.save_failure.as_deref()
    }

    pub fn is_shown(&self) -> bool {
        self.shown
    }

    /// Shows Settings in the main area, or goes back from it.
    pub fn show(&mut self, shown: bool, cx: &mut Context<Self>) {
        if self.shown != shown {
            self.shown = shown;
            if !shown {
                self.save(cx);
            }
            cx.notify();
        }
    }

    pub fn switch(&self) -> Option<&Switch> {
        self.switch.as_ref()
    }

    /// Whether the app is moving onto another archive: nothing else is to be started.
    pub fn is_switching(&self) -> bool {
        matches!(self.switch, Some(Switch::Opening(_)))
    }

    /// Nothing is being written or switched to: automation's `settled`.
    pub fn is_settled(&self) -> bool {
        self.saving.is_none() && !self.is_switching()
    }

    pub fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        if self.settings.mode != mode {
            self.settings.mode = mode;
            self.apply_appearance(cx);
        }
    }

    pub fn set_material(&mut self, material: Material, cx: &mut Context<Self>) {
        if self.settings.material != Some(material) {
            self.settings.material = Some(material);
            self.apply_appearance(cx);
        }
    }

    /// Puts the appearance asked for in place, `--theme`'s mode over the setting's, and keeps it.
    fn apply_appearance(&mut self, cx: &mut Context<Self>) {
        let mut preferences = gpui_appearance::state(cx).preferences();
        preferences.mode = self.mode_override.unwrap_or(self.settings.mode);
        if let Some(material) = self.settings.material {
            preferences.material = material;
        }
        gpui_appearance::set_preferences(preferences, cx);
        self.changed(cx);
    }

    pub fn set_sync_every(&mut self, every: SyncEvery, cx: &mut Context<Self>) {
        if self.settings.sync_every != every {
            self.settings.sync_every = every;
            self.changed(cx);
        }
    }

    /// Keeps what the archive just opened is, for the archive-unavailable page (DESIGN §4.14).
    pub fn remember(&mut self, id: String, sessions: usize, cx: &mut Context<Self>) {
        let bytes = self
            .settings
            .last
            .as_ref()
            .filter(|last| last.id == id)
            .and_then(|last| last.bytes);
        self.settings.last = Some(LastArchive {
            id,
            opened: crate::format::now(),
            sessions,
            bytes,
        });
        self.changed(cx);
    }

    /// Keeps how big the archive open now is, once Settings has measured it.
    /// It is written when Settings closes, with the next change, or at quit, not with every
    /// measure.
    pub fn remember_size(&mut self, id: &str, bytes: u64) {
        if let Some(last) = self.settings.last.as_mut().filter(|last| last.id == id)
            && last.bytes != Some(bytes)
        {
            last.bytes = Some(bytes);
            self.generation += 1;
        }
    }

    /**
     * Why `folder` can't be the archive's at all, before it is looked into: the archive open
     * now, or a folder inside it. With no archive open, any folder may be tried.
     */
    pub fn refusal(&self, folder: &Path, cx: &gpui::App) -> Option<&'static str> {
        let archive = cx.services().archive.read(cx);
        archive.archive()?;
        let (current, chosen) = (resolved(archive.dir()), resolved(folder));
        if chosen == current {
            Some("That is the archive open now.")
        } else if chosen.starts_with(&current) {
            Some("That folder is inside the archive open now. Choose one outside it.")
        } else {
            None
        }
    }

    /**
     * Opens the archive in `folder`, or makes one there when it holds nothing, and moves the
     * app onto it (the module docs). The folder the app is on already, and one inside it, are
     * refused ([`Self::refusal`]); so is one that holds anything but an archive, and one whose
     * archive doesn't open: the app then stays where it is. It works with no archive open too:
     * that is how the archive-unavailable page moves on (DESIGN §4.14).
     */
    pub fn use_folder(&mut self, folder: PathBuf, cx: &mut Context<Self>) {
        if self.is_switching() {
            return;
        }
        if let Some(why) = self.refusal(&folder, cx) {
            self.switch = Some(Switch::Refused {
                folder,
                why: why.to_string(),
            });
            cx.notify();
            return;
        }
        self.switch = Some(Switch::Opening(folder.clone()));
        super::pause(true, cx);
        cx.notify();
        let opening =
            cx.background_spawn({
                let folder = folder.clone();
                async move {
                    archive::open_or_make(&folder).map_err(|error| why_refused(&folder, &error))
                }
            });
        cx.spawn(async move |this, cx| {
            let opened = opening.await;
            this.update(cx, |this, cx| match opened {
                Ok(archive) => this.reopen(archive, cx),
                Err(why) => {
                    super::pause(false, cx);
                    this.switch = Some(Switch::Refused { why, folder });
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Moves the app onto `archive`, opened in `folder`, and keeps that as the archive's
    /// folder once it is there: the settings file is written from then on, whatever it held.
    fn reopen(&mut self, archive: Archive, cx: &mut Context<Self>) {
        let folder = archive.dir().to_path_buf();
        let moving = super::reopen(Arc::new(archive), cx);
        cx.spawn(async move |this, cx| {
            moving.await;
            this.update(cx, |this, cx| {
                this.switch = None;
                this.shown = false;
                this.unreadable = None;
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
    /// ends, if they changed meanwhile. One that fails is tried again with the next change.
    fn save(&mut self, cx: &mut Context<Self>) {
        if self.saving.is_some() || self.written == self.generation || self.unreadable.is_some() {
            return;
        }
        let (path, generation, text) =
            (self.path.clone(), self.generation, self.settings.to_json());
        let on_disk = self.on_disk.clone();
        let write =
            cx.background_spawn(async move { write_newer(&on_disk, &path, generation, &text) });
        self.saving = Some(cx.spawn(async move |this, cx| {
            let written = write.await;
            this.update(cx, |this, cx| {
                this.saving = None;
                match written {
                    Ok(()) => {
                        this.written = generation;
                        this.save_failure = None;
                        this.save(cx);
                    }
                    Err(error) => {
                        log::error!("the settings couldn't be saved: {error}");
                        this.save_failure = Some(error.to_string());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Writes what is unwritten, now: the app is quitting.
    fn flush(&mut self) {
        if self.written != self.generation && self.unreadable.is_none() {
            let text = self.settings.to_json();
            match write_newer(&self.on_disk, &self.path, self.generation, &text) {
                Ok(()) => self.written = self.generation,
                Err(error) => log::error!("the settings couldn't be saved at quit: {error}"),
            }
        }
    }
}

/// Why `folder` wasn't taken as the archive, for the owner; the error itself is logged. It
/// looks at the folder, so it is called off the UI thread.
pub fn why_refused(folder: &Path, error: &archive::ArchiveError) -> String {
    log::warn!("{} wasn't taken as the archive: {error}", folder.display());
    match error {
        archive::ArchiveError::NotAnArchive(_) => format!(
            "{} holds other files, so no archive is made there. Choose an archive's folder or \
             an empty one.",
            folder.display()
        ),
        archive::ArchiveError::Nested { other, .. } => format!(
            "{} is inside the archive in {}, or holds one, so no archive is made there.",
            folder.display(),
            other.display()
        ),
        archive::ArchiveError::Locked(_) => {
            "The archive there is open in another scrollback window.".to_string()
        }
        /* The system's own words stay in the log. */
        archive::ArchiveError::Io { .. } if folder.is_file() => {
            "That's a file, not a folder.".to_string()
        }
        archive::ArchiveError::Io { action, source, .. }
            if matches!(
                source.kind(),
                io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem
            ) =>
        {
            if matches!(*action, "read" | "list" | "resolve") {
                "scrollback can't read that folder.".to_string()
            } else {
                "scrollback can't write to that folder.".to_string()
            }
        }
        archive::ArchiveError::Io { action, path, .. } => {
            format!("scrollback couldn't {action} {}.", path.display())
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
            archive: Some(PathBuf::from(if cfg!(windows) {
                r"D:\Archive"
            } else {
                "/home/user/Archive"
            })),
            mode: Mode::Dark,
            material: Some(Material::Acrylic),
            sync_every: SyncEvery::Sixty,
            last: Some(LastArchive {
                id: "a3f061c2".to_string(),
                opened: 1_791_115_200_000,
                sessions: 2318,
                bytes: Some(4_400_000_000),
            }),
            ..AppSettings::default()
        };
        assert_eq!(
            AppSettings::parse(&settings.to_json()),
            Ok((settings, Vec::new()))
        );

        let (read, _) =
            AppSettings::parse(r#"{"v": 1, "mode": "light", "window": {"x": 4}, "newer": true}"#)
                .unwrap();
        assert_eq!(read.mode, Mode::Light);
        let written: Value = serde_json::from_str(&read.to_json()).unwrap();
        assert_eq!(written["window"], json!({"x": 4}));
        assert_eq!(written["newer"], json!(true));
        /* Neither the default folder nor the platform's material is written down as chosen. */
        assert_eq!(written.get("archive"), None);
        assert_eq!(written.get("material"), None);
    }

    #[test]
    fn a_value_that_cant_be_read_takes_its_default() {
        let (read, problems) = AppSettings::parse(
            r#"{"v": 1, "mode": "sepia", "material": 3, "sync_every": "hourly", "last_archive": 7}"#,
        )
        .unwrap();
        assert_eq!(read, AppSettings::default());
        assert_eq!(problems.len(), 4, "{problems:?}");
    }

    /// What says where the archive is, or what the file is, is never guessed at: an archive
    /// folder given relative would be found from wherever the app was started.
    #[test]
    fn a_file_whose_archive_or_version_cant_be_read_is_not_read_at_all() {
        for text in [
            "[1, 2]",
            "not json",
            r#"{"v": 2, "mode": "dark"}"#,
            r#"{"v": "one"}"#,
            r#"{"v": 1, "archive": ""}"#,
            r#"{"v": 1, "archive": 3}"#,
            r#"{"v": 1, "archive": "Archive"}"#,
            r#"{"v": 1, "archive": "../scrollback/archive"}"#,
        ] {
            assert!(AppSettings::parse(text).is_err(), "{text}");
        }
    }

    /// A write of an older snapshot that ends after a newer one (a background save after the
    /// quit's) leaves the newer on disk.
    #[test]
    fn an_older_snapshot_never_replaces_a_newer_one() {
        let dir = tempfile::tempdir().unwrap();
        let (path, on_disk) = (dir.path().join(SETTINGS_FILE), Mutex::new(0));
        write_newer(&on_disk, &path, 2, "newer").unwrap();
        write_newer(&on_disk, &path, 1, "older").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "newer");
        write_newer(&on_disk, &path, 3, "newest").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "newest");
    }

    #[test]
    fn a_file_that_cant_be_read_is_never_written_over() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()).unreadable, None);
        std::fs::write(dir.path().join(SETTINGS_FILE), r#"{"v": 1, "archive": 3}"#).unwrap();
        let loaded = load(dir.path());
        assert!(loaded.unreadable.is_some());
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
