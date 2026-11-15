//! Command-line options.

use std::{fmt, path::PathBuf, str::FromStr};

use clap::Parser;
use clap::builder::{OsStringValueParser, TypedValueParser as _};
use sb_ui::appearance::Mode;

/// Back up, archive and read coding-agent transcripts.
// A repeated option takes its last value: a scenario's own `app_args` follow the runner's.
#[derive(Parser, Debug, Clone, PartialEq)]
#[command(name = "scrollback", version, args_override_self = true)]
#[cfg_attr(
    feature = "automation",
    command(group(clap::ArgGroup::new("automation_mode").args(["automation", "automation_pipe"]).multiple(true)))
)]
pub struct Cli {
    /// Where scrollback keeps its files; an empty value counts as not given [default: the
    /// platform's local data folder plus `scrollback`].
    // clap's own path parser refuses an empty value, which `SCROLLBACK_DATA_DIR=` is.
    #[arg(
        long,
        env = "SCROLLBACK_DATA_DIR",
        value_name = "PATH",
        value_parser = OsStringValueParser::new().map(PathBuf::from)
    )]
    pub data_dir: Option<PathBuf>,

    // The automation options exist only in a build with the driver (`automation`); the shipped
    // app's `--help` doesn't offer what it can't do.
    #[cfg(feature = "automation")]
    /// Accept automation commands as JSON lines on stdin (PLAN §12).
    #[arg(long)]
    pub automation: bool,

    #[cfg(feature = "automation")]
    /// Serve automation on the Windows named pipe `\\.\pipe\<NAME>` instead of stdio, so a
    /// driver can attach to a running app. Implies `--automation`.
    #[arg(long, value_name = "NAME")]
    pub automation_pipe: Option<String>,

    #[cfg(feature = "automation")]
    /// Under automation, open every window unfocused, behind all other windows and out of the
    /// taskbar, so scripted runs never disturb the person using the machine. The e2e runner
    /// passes it unless given `--foreground`. Needs `--automation` or `--automation-pipe`.
    #[arg(long, requires = "automation_mode")]
    pub automation_background: bool,

    /// Window size in logical pixels, e.g. 1440x900 (minimum 960x600) [default: 1440x900].
    #[arg(long, value_name = "WxH")]
    pub window_size: Option<WindowSize>,

    /// Light or dark for this run, whatever the system shows: system, light or dark
    /// [default: system].
    #[arg(long, value_name = "MODE", value_parser = theme)]
    pub theme: Option<Mode>,
}

#[cfg(feature = "automation")]
impl Cli {
    /// Whether the automation driver runs, over stdio or a named pipe.
    pub fn automation_enabled(&self) -> bool {
        self.automation || self.automation_pipe.is_some()
    }
}

fn theme(value: &str) -> Result<Mode, String> {
    Mode::from_key(value).ok_or_else(|| format!("expected system, light or dark, got {value:?}"))
}

/// A window size in logical pixels, written `WIDTHxHEIGHT`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowSize {
    pub width: u32,
    pub height: u32,
}

impl fmt::Display for WindowSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}

impl FromStr for WindowSize {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (width, height) = value
            .trim()
            .split_once(['x', 'X', '×'])
            .ok_or_else(|| format!("expected WIDTHxHEIGHT, got {value:?}"))?;
        let parse = |part: &str, name: &str| {
            part.trim()
                .parse::<u32>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(|| format!("{name} must be a positive whole number, got {part:?}"))
        };
        Ok(Self {
            width: parse(width, "width")?,
            height: parse(height, "height")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("scrollback").chain(args.iter().copied()))
    }

    #[cfg(feature = "automation")]
    #[test]
    fn automation_pipe_implies_automation() {
        assert!(!parse(&[]).unwrap().automation_enabled());
        let cli = parse(&["--automation-pipe", "scrollback-e2e"]).unwrap();
        assert!(!cli.automation);
        assert!(cli.automation_enabled());
        assert_eq!(cli.automation_pipe.as_deref(), Some("scrollback-e2e"));
        assert!(parse(&["--automation-pipe"]).is_err(), "needs a name");
        assert!(parse(&["--automation"]).unwrap().automation_enabled());
    }

    #[cfg(feature = "automation")]
    #[test]
    fn the_runners_command_line_parses() {
        let cli = parse(&[
            "--automation",
            "--automation-background",
            "--data-dir",
            "target/tmp/data",
            "--window-size",
            "1440x900",
        ])
        .unwrap();
        assert_eq!(cli.data_dir, Some(PathBuf::from("target/tmp/data")));
        assert!(cli.automation);
        assert!(cli.automation_background);
        assert_eq!(
            cli.window_size,
            Some(WindowSize {
                width: 1440,
                height: 900
            })
        );
        assert_eq!(cli.theme, None);
    }

    #[cfg(feature = "automation")]
    #[test]
    fn background_needs_automation() {
        assert!(parse(&["--automation-background"]).is_err());
        assert!(parse(&["--automation", "--automation-background"]).is_ok());
        assert!(parse(&["--automation-pipe", "p", "--automation-background"]).is_ok());
    }

    #[cfg(not(feature = "automation"))]
    #[test]
    fn a_build_without_the_driver_has_no_automation_options() {
        for flag in ["--automation", "--automation-background"] {
            assert!(parse(&[flag]).is_err(), "{flag}");
        }
        assert!(parse(&["--automation-pipe", "p"]).is_err());
    }

    #[test]
    fn an_empty_data_dir_parses_as_given_but_empty() {
        // `SCROLLBACK_DATA_DIR=` reaches the parser the same way; `data_dir` reads it as unset.
        assert_eq!(
            parse(&["--data-dir", ""]).unwrap().data_dir,
            Some(PathBuf::new())
        );
        assert_eq!(parse(&[]).unwrap().data_dir, None);
    }

    #[test]
    fn window_size_syntax() {
        let size = |s: &str| s.parse::<WindowSize>();
        assert_eq!(
            size("1440x900"),
            Ok(WindowSize {
                width: 1440,
                height: 900
            })
        );
        assert_eq!(
            size(" 960 X 600 ").map(|size| size.to_string()),
            Ok("960x600".into())
        );
        assert!(size("1440").is_err());
        assert!(size("0x900").is_err());
        assert!(size("wide x tall").is_err());
    }

    #[test]
    fn a_repeated_window_size_takes_the_last() {
        let cli = parse(&["--window-size", "1440x900", "--window-size", "960x600"]).unwrap();
        assert_eq!(
            cli.window_size,
            Some(WindowSize {
                width: 960,
                height: 600
            })
        );
    }

    #[test]
    fn theme_names_a_mode() {
        assert_eq!(
            parse(&["--theme", "light"]).unwrap().theme,
            Some(Mode::Light)
        );
        assert_eq!(parse(&["--theme", "dark"]).unwrap().theme, Some(Mode::Dark));
        assert_eq!(
            parse(&["--theme", "system"]).unwrap().theme,
            Some(Mode::System)
        );
        assert!(parse(&["--theme", "sepia"]).is_err());
    }
}
