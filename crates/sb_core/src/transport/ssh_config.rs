//! Which machine a host string is to this machine's `ssh` (PLAN §6.4, §7.7): what
//! `ssh -G -- <host>` says a connection would go to after the owner's own config has had its
//! say (`Host` and `Match` blocks, `Include`s, defaults). It opens no SSH connection, but it is
//! not inert: it runs the owner's `Match exec` commands, and with `CanonicalizeHostname` set it
//! looks host names up in DNS. So each host string's answer is kept for the app's life, until
//! the owner's `~/.ssh/config` changes or a day has passed ([`SshConfig`]).
//!
//! [`ResolveHost`] is the seam: [`SshConfig`] runs `ssh`, and tests and scripted runs answer
//! with a stand-in. `ssh -G` is a launcher like any other, refused once launchers are
//! ([`super::refuse_launchers`]).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use toolbox_process::{CancelToken, Command, run};

use super::TransportError;
use super::launching::may_start;

/// How long `ssh -G` may take. It reads files and answers at once, unless a `Match exec` or a
/// lookup is slow; one that doesn't answer in time is taken for one that can't be asked.
const TIMEOUT: Duration = Duration::from_secs(5);

/// How long an answer is kept while the owner's config doesn't change.
const KEPT: Duration = Duration::from_secs(24 * 60 * 60);

/// The port `ssh` connects to when nothing says another.
const DEFAULT_PORT: u16 = 22;

/// Whether `ssh -G` failing has been logged: once per run of the app.
static FAILURE_LOGGED: AtomicBool = AtomicBool::new(false);

/**
 * Where a host string ends up: what two host strings must share to be one machine. The way
 * there is part of it: one address behind two jump hosts is two machines.
 */
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// The host name, lowercase: `ssh` matches names without regard to case.
    pub hostname: String,
    pub user: String,
    pub port: u16,
    /// The jump hosts it is reached through (`ProxyJump`), as `ssh` prints them.
    pub proxy_jump: Option<String>,
    /// The command it is reached through (`ProxyCommand`), as `ssh` prints it.
    pub proxy_command: Option<String>,
}

impl Resolved {
    /**
     * Whether it is this machine itself: a loopback address (`127.0.0.0/8`, `::1`) or
     * `localhost`. An app's SSH into WSL reaches the distro's own sshd so (PLAN §7.7).
     */
    pub fn is_loopback(&self) -> bool {
        let name = self.hostname.trim_start_matches('[').trim_end_matches(']');
        name == "localhost"
            || name.ends_with(".localhost")
            || name
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    }
}

/// What resolving a host came to: an error when `ssh` could not be asked (missing, failing or
/// slow), which says nothing of the host; `None` when it answered without all it is asked.
pub type Resolution = Result<Option<Resolved>, String>;

/// Resolves host strings as this machine's `ssh` would.
pub trait ResolveHost: Send + Sync {
    /// Where `host` ends up ([`Resolution`]): never a guess.
    fn resolve(&self, host: &str) -> Resolution;
}

/// [`ResolveHost`] by `ssh -G`, with the owner's own config, each host's answer kept (the
/// module docs).
#[derive(Debug, Default)]
pub struct SshConfig {
    kept: Mutex<Kept>,
}

/// The answers [`SshConfig`] keeps, and the config they were given under.
#[derive(Debug, Default)]
struct Kept {
    config: Option<SystemTime>,
    answers: HashMap<String, (Instant, Option<Resolved>)>,
}

impl SshConfig {
    /**
     * What `ssh -G` makes `host`, asked now: `None` when it answers without the settings read.
     * An error when it can't be asked: launchers refused, `ssh` that can't start, fails or
     * takes too long. A host that is not one to hand `ssh` (PLAN §6.4) is never handed to it,
     * and is no machine.
     */
    pub fn run(host: &str) -> Result<Option<Resolved>, TransportError> {
        if crate::location::host_problem(host).is_some() {
            return Ok(None);
        }
        let words = ["-G", "--", host];
        may_start(&format!("ssh {}", words.join(" ")))?;
        let output = run(
            &Command::new("ssh").args(words),
            TIMEOUT,
            &CancelToken::new(),
        )
        .map_err(|error| TransportError::Failed(error.to_string()))?;
        if !output.status.success() {
            return Err(TransportError::Failed(format!(
                "ssh -G ended with {}",
                output.status
            )));
        }
        Ok(parse(&String::from_utf8_lossy(&output.stdout)))
    }

    /// When the owner's `~/.ssh/config` last changed, if there is one.
    fn config_changed() -> Option<SystemTime> {
        let config: PathBuf = std::env::home_dir()?.join(".ssh").join("config");
        std::fs::metadata(config).ok()?.modified().ok()
    }
}

impl ResolveHost for SshConfig {
    fn resolve(&self, host: &str) -> Resolution {
        let config = Self::config_changed();
        {
            let mut kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
            if kept.config != config {
                kept.answers.clear();
                kept.config = config;
            }
            if let Some((at, answer)) = kept.answers.get(host)
                && at.elapsed() < KEPT
            {
                return Ok(answer.clone());
            }
        }
        match Self::run(host) {
            Ok(answer) => {
                (self.kept.lock().unwrap_or_else(PoisonError::into_inner))
                    .answers
                    .insert(host.to_string(), (Instant::now(), answer.clone()));
                Ok(answer)
            }
            Err(error) => {
                if !FAILURE_LOGGED.swap(true, Ordering::Relaxed) {
                    log::info!("ssh -G could not be asked where a host is: {error}");
                }
                Err(error.to_string())
            }
        }
    }
}

/**
 * What `ssh -G` printed, `said`: one setting a line, its key and then its value. Only
 * `hostname`, `user`, `port`, `proxyjump` and `proxycommand` are read, their keys in any case;
 * every other line is ignored. An answer without the first three, or with a port that isn't
 * one, resolves nothing. `none` for a proxy is none.
 */
pub fn parse(said: &str) -> Option<Resolved> {
    let (mut hostname, mut user, mut port) = (None, None, None);
    let (mut proxy_jump, mut proxy_command) = (None, None);
    for line in said.lines() {
        let Some((key, value)) = line.trim().split_once(char::is_whitespace) else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        let proxy = || (!value.eq_ignore_ascii_case("none")).then(|| value.to_string());
        match key.to_ascii_lowercase().as_str() {
            "hostname" => hostname = Some(value.to_ascii_lowercase()),
            "user" => user = Some(value.to_string()),
            "port" => port = Some(value.parse::<u16>().ok().filter(|port| *port > 0)?),
            "proxyjump" => proxy_jump = proxy(),
            "proxycommand" => proxy_command = proxy(),
            _ => {}
        }
    }
    Some(Resolved {
        hostname: hostname?,
        user: user?,
        port: port?,
        proxy_jump,
        proxy_command,
    })
}

/// [`ResolveHost`] by [`plain`] with `user` for a host that names none: `ssh` with no config,
/// for tests and scripted runs.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoConfig;

impl ResolveHost for NoConfig {
    fn resolve(&self, host: &str) -> Resolution {
        Ok(plain(host, "user"))
    }
}

/**
 * A stand-in for `ssh -G` with no config at all, as tests and scripted runs use: `user@host`
 * and `host` both end at `host` (lowercase), with the user given or `default_user`, at port
 * 22, directly; `host:port` is not taken apart, as `ssh` doesn't. A host that is no host to
 * hand `ssh` resolves nothing, as with the real one.
 */
pub fn plain(host: &str, default_user: &str) -> Option<Resolved> {
    if crate::location::host_problem(host).is_some() {
        return None;
    }
    let (user, hostname) = match host.rsplit_once('@') {
        Some((user, hostname)) => (user, hostname),
        None => (default_user, host),
    };
    Some(Resolved {
        hostname: hostname.to_ascii_lowercase(),
        user: user.to_string(),
        port: DEFAULT_PORT,
        proxy_jump: None,
        proxy_command: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved(hostname: &str, user: &str, port: u16) -> Option<Resolved> {
        Some(Resolved {
            hostname: hostname.to_string(),
            user: user.to_string(),
            port,
            proxy_jump: None,
            proxy_command: None,
        })
    }

    #[test]
    fn only_the_host_name_user_port_and_proxies_are_read() {
        let said = "user someone\nHostName Build.Example\nport 2222\nidentityfile ~/.ssh/id\n\
                    sendenv LANG\nproxyjump none\nproxycommand none\n";
        assert_eq!(parse(said), resolved("build.example", "someone", 2222));
        assert_eq!(
            parse("HOSTNAME a\r\nUSER b\r\nPORT 22\r\n"),
            resolved("a", "b", 22)
        );
        let behind = |jump: &str| {
            parse(&format!(
                "hostname 10.0.0.5\nuser u\nport 22\nproxyjump {jump}\n"
            ))
        };
        assert_ne!(behind("bastion-a"), behind("bastion-b"));
        assert_eq!(
            behind("bastion-a").and_then(|it| it.proxy_jump).as_deref(),
            Some("bastion-a")
        );
        let command = parse("hostname h\nuser u\nport 22\nproxycommand nc -X 5 %h %p\n");
        assert_eq!(
            command.and_then(|it| it.proxy_command).as_deref(),
            Some("nc -X 5 %h %p")
        );
    }

    #[test]
    fn loopback_is_this_machine() {
        for name in ["localhost", "127.0.0.1", "127.8.9.10", "::1", "[::1]"] {
            assert!(resolved(name, "u", 22).unwrap().is_loopback(), "{name}");
        }
        for name in ["10.0.0.5", "macbook", "128.0.0.1", "localhost.example"] {
            assert!(!resolved(name, "u", 22).unwrap().is_loopback(), "{name}");
        }
    }

    #[test]
    fn an_answer_without_all_three_resolves_nothing() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("hostname a\nuser b\n"), None);
        assert_eq!(parse("hostname a\nport 22\n"), None);
        assert_eq!(parse("hostname a\nuser b\nport twenty\n"), None);
        assert_eq!(parse("hostname a\nuser b\nport 0\n"), None);
        assert_eq!(parse("hostname a\nuser b\nport 70000\n"), None);
        assert_eq!(parse("hostname\nuser b\nport 22\n"), None);
    }

    #[test]
    fn the_plain_stand_in_ends_a_host_at_its_name_and_user() {
        assert_eq!(plain("user@MacBook", "me"), resolved("macbook", "user", 22));
        assert_eq!(plain("macbook", "user"), resolved("macbook", "user", 22));
        assert_eq!(plain("-oProxyCommand=x", "user"), None);
    }

    #[test]
    fn a_host_ssh_should_not_be_handed_is_no_machine_and_runs_nothing() {
        assert!(matches!(SshConfig::run("-oProxyCommand=touch x"), Ok(None)));
        assert!(matches!(SshConfig::run("host name"), Ok(None)));
    }
}
