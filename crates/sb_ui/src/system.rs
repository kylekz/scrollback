//! What scrollback asks of the operating system beyond its own windows: showing a folder in
//! Explorer or Finder, the folder picker (DESIGN §4.13: Show in Explorer, Use a different
//! folder…), opening a link and writing to the clipboard; and handing back memory
//! ([`GiveBack`]). Lifted from dv's `dv_ui::system` (MIT, PLAN D15), narrowed to these.
//!
//! Views call these rather than gpui's `App::reveal_path`, `prompt_for_paths`, `open_url` and
//! `write_to_clipboard`, so a scripted run or a test never reaches anything outside scrollback:
//! the person's file manager, browser or clipboard. The app installs [`SystemShell::Real`];
//! automation and tests install [`SystemShell::Scripted`], which opens nothing: every request
//! is recorded ([`requests`]), and the folder picker is answered with the next answer queued
//! ([`queue_answer`]), or as if cancelled when none was. Without either installed, nothing is
//! opened either: only the app opts in to the real shell.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use gpui::{App, AppContext as _, ClipboardItem, Global, PathPromptOptions, SharedString, Task};

/// How many requests a scripted shell remembers.
const LOG_LEN: usize = 50;

/// One thing asked of the system.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SystemRequest {
    /// A folder or a file shown in its folder.
    Reveal(PathBuf),
    /// The folder picker, with its button's label.
    PickFolder { prompt: SharedString },
    /// A web address opened in the browser.
    OpenUrl(String),
    /// Text put on the clipboard.
    Copy(String),
}

impl SystemRequest {
    /// `"reveal"`, `"pick_folder"`, `"open_url"` or `"copy"`, as automation's `state` spells it.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Reveal(_) => "reveal",
            Self::PickFolder { .. } => "pick_folder",
            Self::OpenUrl(_) => "open_url",
            Self::Copy(_) => "copy",
        }
    }
}

/// Where requests go.
pub enum SystemShell {
    /// The operating system's own.
    Real,
    /// Nowhere: recorded, the folder picker answered from a queue.
    Scripted {
        requests: VecDeque<SystemRequest>,
        /// Answers for the next pickers, oldest first: the folder chosen, or `None` for a
        /// cancelled one.
        answers: VecDeque<Option<PathBuf>>,
    },
}

impl Global for SystemShell {}

impl SystemShell {
    pub fn scripted() -> Self {
        Self::Scripted {
            requests: VecDeque::new(),
            answers: VecDeque::new(),
        }
    }
}

/// Installs `shell` for the app.
pub fn install(shell: SystemShell, cx: &mut App) {
    cx.set_global(shell);
}

/// Queues the answer to the next folder picker of a scripted shell: the folder chosen, or
/// `None` for a cancelled one. Refused unless the shell is scripted.
pub fn queue_answer(answer: Option<PathBuf>, cx: &mut App) -> anyhow::Result<()> {
    match cx.try_global::<SystemShell>() {
        Some(SystemShell::Scripted { .. }) => {}
        _ => anyhow::bail!("picker answers can only be queued in a scripted run"),
    }
    if let SystemShell::Scripted { answers, .. } = cx.global_mut::<SystemShell>() {
        answers.push_back(answer);
    }
    Ok(())
}

/// What a scripted shell was asked, oldest first.
pub fn requests(cx: &App) -> Vec<SystemRequest> {
    match cx.try_global::<SystemShell>() {
        Some(SystemShell::Scripted { requests, .. }) => requests.iter().cloned().collect(),
        _ => Vec::new(),
    }
}

/**
 * Records `request` unless the shell is the real one: `Some(answer)` (the next queued one,
 * else a cancel) for the picker, `Some(None)` otherwise. `None` when the shell is the real
 * one.
 */
fn scripted(request: SystemRequest, cx: &mut App) -> Option<Option<PathBuf>> {
    match cx.try_global::<SystemShell>() {
        Some(SystemShell::Real) => return None,
        None => return Some(None),
        Some(SystemShell::Scripted { .. }) => {}
    }
    let SystemShell::Scripted { requests, answers } = cx.global_mut::<SystemShell>() else {
        return Some(None);
    };
    let picker = matches!(request, SystemRequest::PickFolder { .. });
    log::info!("scripted: {} (not done)", request.kind());
    if requests.len() == LOG_LEN {
        requests.pop_front();
    }
    requests.push_back(request);
    Some(if picker {
        answers.pop_front().flatten()
    } else {
        None
    })
}

/// What [`reveal`] is called where it is offered, as each platform names its file manager.
pub const REVEAL_LABEL: &str = if cfg!(target_os = "macos") {
    "Show in Finder"
} else {
    "Show in Explorer"
};

/// Shows `path` in Explorer or Finder.
pub fn reveal(path: &Path, cx: &mut App) {
    if scripted(SystemRequest::Reveal(path.to_path_buf()), cx).is_none() {
        cx.reveal_path(path);
    }
}

/// The folder picker: the folder chosen, `None` when cancelled.
pub fn pick_folder(
    prompt: impl Into<SharedString>,
    cx: &mut App,
) -> Task<anyhow::Result<Option<PathBuf>>> {
    let prompt = prompt.into();
    if let Some(answer) = scripted(
        SystemRequest::PickFolder {
            prompt: prompt.clone(),
        },
        cx,
    ) {
        return Task::ready(Ok(answer));
    }
    let chosen = cx.prompt_for_paths(PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some(prompt),
    });
    cx.background_spawn(
        async move { Ok(chosen.await??.and_then(|paths| paths.into_iter().next())) },
    )
}

/// Opens a web address in the person's browser. Anything else (`file:`, a custom scheme, a
/// relative path) is ignored: a transcript is untrusted text, and a link in it must not start a
/// program.
pub fn open_url(url: &str, cx: &mut App) {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return;
    }
    if scripted(SystemRequest::OpenUrl(url.to_string()), cx).is_none() {
        cx.open_url(url);
    }
}

/**
 * Puts `text` on the clipboard, less every zero-width space in it: the sanitiser puts them in
 * tags and addresses to keep them from being read as markup, and one pasted into a shell or a
 * browser breaks the command or the address it is in. A transcript's own aren't worth keeping
 * on a clipboard either.
 */
pub fn copy_text(text: String, cx: &mut App) {
    let text = if text.contains('\u{200B}') {
        text.replace('\u{200B}', "")
    } else {
        text
    };
    if scripted(SystemRequest::Copy(text.clone()), cx).is_none() {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }
}

/**
 * What hands the memory the allocator holds free back to the system: set by the app when its
 * allocator keeps it otherwise (macOS's build, with mimalloc), and run after something large
 * was dropped (a closed session, a sync pass).
 */
pub struct GiveBack(pub fn());

impl Global for GiveBack {}

/// Runs [`GiveBack`], if the app set one.
pub fn give_back_memory(cx: &App) {
    if let Some(GiveBack(give_back)) = cx.try_global::<GiveBack>() {
        give_back();
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;
    use pretty_assertions::assert_eq;

    use super::*;

    #[gpui::test]
    fn a_scripted_shell_records_and_answers_from_its_queue(cx: &mut TestAppContext) {
        cx.update(|cx| {
            install(SystemShell::scripted(), cx);
            queue_answer(Some(PathBuf::from("/Volumes/Backup/scrollback")), cx).unwrap();
            reveal(
                Path::new("/Users/user/Library/Application Support/scrollback"),
                cx,
            );
        });
        let chosen = cx.update(|cx| pick_folder("Use this folder", cx));
        assert_eq!(
            cx.foreground_executor.block_test(chosen).unwrap(),
            Some(PathBuf::from("/Volumes/Backup/scrollback"))
        );
        /* Nothing queued: as if cancelled. */
        let nothing = cx.update(|cx| pick_folder("Use this folder", cx));
        assert_eq!(cx.foreground_executor.block_test(nothing).unwrap(), None);
        cx.update(|cx| {
            let kinds: Vec<&str> = requests(cx).iter().map(SystemRequest::kind).collect();
            assert_eq!(kinds, ["reveal", "pick_folder", "pick_folder"]);
        });
        assert!(!cx.did_prompt_for_paths());
    }

    #[gpui::test]
    fn without_a_shell_installed_nothing_is_opened(cx: &mut TestAppContext) {
        cx.update(|cx| {
            reveal(Path::new("/Users/user"), cx);
            open_url("https://example.com/docs", cx);
            copy_text("copied".to_string(), cx);
            assert!(queue_answer(None, cx).is_err());
        });
        let nothing = cx.update(|cx| pick_folder("Use this folder", cx));
        assert_eq!(cx.foreground_executor.block_test(nothing).unwrap(), None);
        assert!(!cx.did_prompt_for_paths());
        assert_eq!(cx.read_from_clipboard(), None);
    }

    #[gpui::test]
    fn a_scripted_run_keeps_links_and_copies_in_the_app(cx: &mut TestAppContext) {
        cx.update(|cx| {
            install(SystemShell::scripted(), cx);
            open_url("https://example.com/docs", cx);
            copy_text("copied".to_string(), cx);
            copy_text(
                "git log <\u{200B}branch> https:\u{200B}//example.com".to_string(),
                cx,
            );
            assert_eq!(
                requests(cx),
                [
                    SystemRequest::OpenUrl("https://example.com/docs".to_string()),
                    SystemRequest::Copy("copied".to_string()),
                    SystemRequest::Copy("git log <branch> https://example.com".to_string()),
                ]
            );
        });
        assert_eq!(cx.read_from_clipboard(), None);
    }

    #[gpui::test]
    fn only_web_addresses_open(cx: &mut TestAppContext) {
        cx.update(|cx| {
            install(SystemShell::scripted(), cx);
            for url in [
                "file:///etc/hosts",
                "javascript:alert(1)",
                "./notes.md",
                "app://open",
            ] {
                open_url(url, cx);
            }
            assert_eq!(requests(cx), []);
        });
    }
}
