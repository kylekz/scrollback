//! Startup: the data dir, then gpui with scrollback's assets, fonts and theme, the services
//! over the archive in the data dir, the main window and, with `--automation`, the automation
//! driver.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use gpui::{App, QuitMode, px, size};
use sb_core::transport::LocalTransport;
use sb_ui::ActiveTheme as _;
use sb_ui::appearance::MaterialSupport;
use sb_ui::services::ServicesConfig;

use crate::cli::Cli;

/// Starts the app and blocks until it quits.
///
/// Errors before the app starts are returned, for `main` to report. A failure during startup
/// inside the app (the window can't open, the fonts don't load) is logged where it happens and
/// ends the process there with status 1: `Application::run` can't hand an error back, and on
/// macOS a quit ends the process with status 0 without ever returning here.
pub fn run(cli: Cli) -> Result<()> {
    #[cfg(feature = "automation")]
    let automation = cli.automation_enabled();
    #[cfg(not(feature = "automation"))]
    let automation = false;
    let place = crate::data_dir::resolve(cli.data_dir.clone(), automation)?;
    log::info!(
        "data directory: {} ({})",
        place.dir.display(),
        place.source.as_str()
    );
    gpui_platform::application()
        .with_assets(sb_ui::Assets)
        .with_quit_mode(QuitMode::LastWindowClosed)
        .run(move |cx| {
            if let Err(error) = start(cli, place.dir, cx) {
                log::error!("scrollback couldn't start: {error:#}");
                std::process::exit(1);
            }
        });
    Ok(())
}

/// The archive's folder in the data dir (PLAN §4.4, DESIGN §4.10).
const ARCHIVE: &str = "archive";

/// Opens the main window as `cli` asks. `data_dir` is where this run keeps its files: the
/// archive is opened in it, and made there on the first run (PLAN D3).
#[cfg_attr(not(feature = "automation"), expect(unused_variables))]
fn start(cli: Cli, data_dir: PathBuf, cx: &mut App) -> Result<()> {
    sb_ui::init(cx)?;
    // Light or dark and the material, before the first frame.
    sb_ui::appearance::init(MaterialSupport::probe(), cli.theme.unwrap_or_default(), cx);
    sb_ui::services::init(
        ServicesConfig {
            archive_dir: data_dir.join(ARCHIVE),
            providers: sb_core::provider::builtin(),
            local: Arc::new(LocalTransport),
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
