//! Startup: the data dir, then gpui with scrollback's assets, fonts and theme, the services
//! over the archive in the data dir, the main window and, with `--automation`, the automation
//! driver.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use gpui::{App, QuitMode, px, size};
use sb_core::location::LocationKind;
use sb_core::transport::wsl;
use sb_ui::ActiveTheme as _;
use sb_ui::appearance::MaterialSupport;
use sb_ui::services::{Reach, ServicesConfig, Updater};
use sb_ui::system::SystemShell;

use crate::cli::Cli;

/// Starts the app and blocks until it quits.
///
/// Errors before the app starts are returned, for `main` to report. A failure during startup
/// inside the app (the window can't open, the fonts don't load) is reported where it happens
/// (`crate::fatal`) and ends the process there with status 1: `Application::run` can't hand an error back, and on
/// macOS a quit ends the process with status 0 without ever returning here.
pub fn run(cli: Cli) -> Result<()> {
    #[cfg(feature = "automation")]
    let automation = cli.automation_enabled();
    #[cfg(not(feature = "automation"))]
    let automation = false;
    let place = crate::data_dir::resolve(cli.data_dir.clone(), automation)?;
    #[cfg(feature = "automation")]
    if cli.dev_location_home.is_some() {
        crate::data_dir::for_development(&place)?;
    }
    #[cfg(any(windows, target_os = "macos"))]
    let install_root = crate::installer::root();
    #[cfg(not(any(windows, target_os = "macos")))]
    let install_root = None;
    crate::data_dir::outside_install(&place.dir, install_root.as_deref())?;
    /* A panic's report goes beside the app's other files; under automation the runner keeps
    stderr, where the default hook writes it. */
    if !automation {
        crate::crash::install(&place.dir);
    }
    log::info!(
        "data directory: {} ({})",
        place.dir.display(),
        place.source.as_str()
    );
    let updater = crate::updates::updater(automation, place.source);
    gpui_platform::application()
        .with_assets(sb_ui::Assets)
        .with_quit_mode(QuitMode::LastWindowClosed)
        .run(move |cx| {
            if let Err(error) = start(cli, place.dir, updater, install_root, cx) {
                crate::fatal::report(&error);
                std::process::exit(1);
            }
        });
    Ok(())
}

/**
 * Opens the main window as `cli` asks. `data_dir` is where this run keeps its files: the
 * app's settings, and the archive unless they name another folder, made there on the first
 * run (PLAN D3). `updater` looks for scrollback's own updates, and `install_root` is the
 * installer's folder, in an installed copy (`crate::updates`, `crate::installer`).
 */
#[cfg_attr(not(feature = "automation"), expect(unused_variables))]
fn start(
    cli: Cli,
    data_dir: PathBuf,
    updater: Updater,
    install_root: Option<PathBuf>,
    cx: &mut App,
) -> Result<()> {
    sb_ui::init(cx)?;
    #[cfg(target_os = "macos")]
    cx.set_global(sb_ui::system::GiveBack(crate::give_back));
    // Before anything writes a date.
    #[cfg(feature = "automation")]
    if let Some(year) = cli.automation_calendar {
        sb_ui::format::pin_calendar(year);
    }
    // A scripted run never reaches Explorer or Finder, a picker, the browser or the clipboard.
    #[cfg(feature = "automation")]
    let scripted = cli.automation_enabled();
    #[cfg(not(feature = "automation"))]
    let scripted = false;
    sb_ui::system::install(
        if scripted {
            SystemShell::scripted()
        } else {
            SystemShell::Real
        },
        cx,
    );
    let settings = sb_ui::services::settings::load(&data_dir);
    // Light or dark and the material, before the first frame; `--theme` is for this run.
    sb_ui::appearance::init(
        MaterialSupport::probe(),
        cli.theme.unwrap_or(settings.settings.mode),
        settings.settings.material,
        cx,
    );
    let reach = Reach {
        connect: Arc::new(LocationKind::transport),
        tester: Arc::new(sb_ui::services::probe),
        distros: Arc::new(wsl::distros),
        launcher: Arc::new(LocationKind::launcher),
        development_home: None,
        resolve: Arc::new(sb_core::transport::SshConfig),
    };
    // A scripted run reaches only what its scenario put there; a development run with the
    // override reads its distros and hosts from a folder of generated data.
    #[cfg(feature = "automation")]
    let reach = match (crate::automation::stand_ins(&cli), &cli.dev_location_home) {
        (Some(stand_ins), _) => stand_ins,
        (None, Some(home)) => crate::automation::development_override(home),
        (None, None) => reach,
    };
    sb_ui::services::init(
        ServicesConfig {
            settings,
            mode_override: cli.theme,
            providers: sb_core::provider::builtin(),
            reach,
            updater,
            install_root,
        },
        cx,
    );

    #[cfg(feature = "automation")]
    sb_ui::automation::set_background(cli.automation_background);
    let sizes = cx.sb_theme().sizes;
    let window_size =
        cli.window_size
            .map_or(size(sizes.window_width, sizes.window_height), |asked| {
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
    #[cfg(feature = "automation")]
    if cli.automation_enabled() {
        crate::automation::start(
            main.handle.into(),
            main.shell.clone(),
            data_dir,
            cli.automation_pipe,
            cx,
        )?;
    }
    // A background run must never take the foreground from the person at the machine.
    if !sb_ui::automation::is_background() {
        cx.activate(true);
    }
    Ok(())
}
