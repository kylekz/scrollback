//! Startup: the data dir, then gpui with scrollback's assets, fonts and theme, the main window
//! and, with `--automation`, the automation driver.

use std::path::PathBuf;

use anyhow::{Result, bail};
use gpui::{App, QuitMode, px, size};
use sb_ui::ActiveTheme as _;
use sb_ui::appearance::{MaterialSupport, Mode};
use sb_ui::shell::MainWindow;

use crate::cli::{Cli, WindowSize};

/// Starts the app and blocks until it quits.
///
/// Errors before the app starts are returned, for `main` to report. A failure during startup
/// inside the app (the window can't open, the fonts don't load) is logged where it happens and
/// ends the process there with status 1: `Application::run` can't hand an error back, and on
/// macOS a quit ends the process with status 0 without ever returning here.
pub fn run(cli: Cli) -> Result<()> {
    let automation = cli.automation_enabled();
    if automation && !cfg!(feature = "automation") {
        bail!("--automation and --automation-pipe need a build with the `automation` feature");
    }
    let place = crate::data_dir::resolve(cli.data_dir, automation)?;
    log::info!(
        "data directory: {} ({})",
        place.dir.display(),
        place.source.as_str()
    );
    let startup = StartOptions {
        data_dir: place.dir,
        automation,
        automation_pipe: cli.automation_pipe,
        background: cli.automation_background,
        window_size: cli.window_size,
        mode: cli.theme.unwrap_or_default(),
    };
    gpui_platform::application()
        .with_assets(sb_ui::Assets)
        .with_quit_mode(QuitMode::LastWindowClosed)
        .run(move |cx| {
            if let Err(error) = start(startup, cx) {
                log::error!("scrollback couldn't start: {error:#}");
                std::process::exit(1);
            }
        });
    Ok(())
}

/// Everything [`start`] needs from the command line.
struct StartOptions {
    data_dir: PathBuf,
    automation: bool,
    automation_pipe: Option<String>,
    /// `--automation-background`.
    background: bool,
    /// `--window-size`.
    window_size: Option<WindowSize>,
    /// `--theme`.
    mode: Mode,
}

fn start(startup: StartOptions, cx: &mut App) -> Result<()> {
    let StartOptions {
        data_dir,
        automation,
        automation_pipe,
        background,
        window_size,
        mode,
    } = startup;
    sb_ui::init(cx)?;
    // Light or dark and the material, before the first frame.
    sb_ui::appearance::init(MaterialSupport::probe(), mode, cx);

    sb_ui::automation::set_background(background);
    let sizes = cx.sb_theme().sizes;
    let window_size = window_size.map_or(size(sizes.window_width, sizes.window_height), |asked| {
        let window_size = size(
            px(asked.width as f32).max(sizes.window_min_width),
            px(asked.height as f32).max(sizes.window_min_height),
        );
        if window_size != size(px(asked.width as f32), px(asked.height as f32)) {
            log::warn!(
                "--window-size {asked} is below the minimum; using {}x{}",
                window_size.width.as_f32(),
                window_size.height.as_f32()
            );
        }
        window_size
    });
    let main = sb_ui::shell::open_main_window(window_size, cx)?;
    if automation {
        start_automation(&main, data_dir, automation_pipe, cx)?;
    }
    // A background run must never take the foreground from the person at the machine.
    if !sb_ui::automation::is_background() {
        cx.activate(true);
    }
    Ok(())
}

#[cfg(feature = "automation")]
fn start_automation(
    main: &MainWindow,
    data_dir: PathBuf,
    pipe: Option<String>,
    cx: &mut App,
) -> Result<()> {
    crate::automation::start(main.handle.into(), main.shell.clone(), data_dir, pipe, cx)
}

#[cfg(not(feature = "automation"))]
fn start_automation(
    _main: &MainWindow,
    _data_dir: PathBuf,
    _pipe: Option<String>,
    _cx: &mut App,
) -> Result<()> {
    // `run` refuses `--automation` in this build before the app starts.
    Ok(())
}
