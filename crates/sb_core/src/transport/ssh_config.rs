//! Which machine a host string is to this machine's `ssh` (PLAN §6.4, §7.7): what
//! `ssh -G -- <host>` says a connection would go to after the owner's own config has had its
//! say (`Host` and `Match` blocks, `Include`s, defaults). It connects to nothing.
//!
//! [`ResolveHost`] is the seam: [`SshConfig`] runs `ssh`, and tests and scripted runs answer
//! with a stand-in. `ssh -G` is a launcher like any other, refused once launchers are
//! ([`super::refuse_launchers`]).

use std::time::Duration;

use toolbox_process::{CancelToken, Command, run};

use super::TransportError;
use super::launching::may_start;

/// How long `ssh -G` may take. It reads files and answers at once; one that doesn't is taken
/// for one that can't answer.
const TIMEOUT: Duration = Duration::from_secs(5);

/// The port `ssh` connects to when nothing says another.
const DEFAULT_PORT: u16 = 22;

/// Where a host string ends up: what two host strings must share to be one machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// The host name, lowercase: `ssh` matches names without regard to case.
    pub hostname: String,
    pub user: String,
    pub port: u16,
}

/// Resolves host strings as this machine's `ssh` would.
pub trait ResolveHost: Send + Sync {
    /// Where `host` ends up, or `None` when that can't be told: never a guess.
    fn resolve(&self, host: &str) -> Option<Resolved>;
}

/// [`ResolveHost`] by `ssh -G`, with the owner's own config.
#[derive(Clone, Copy, Debug, Default)]
pub struct SshConfig;

impl SshConfig {
    /**
     * What `ssh -G` makes `host`: `None` when it answers without the three settings. An error
     * when it can't be asked: a host that is not one to hand `ssh` (PLAN §6.4), launchers
     * refused, `ssh` that can't start, fails or takes too long.
     */
    pub fn run(host: &str) -> Result<Option<Resolved>, TransportError> {
        if let Some(problem) = crate::location::host_problem(host) {
            return Err(TransportError::Failed(problem));
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
}

impl ResolveHost for SshConfig {
    fn resolve(&self, host: &str) -> Option<Resolved> {
        Self::run(host)
            .inspect_err(|error| log::info!("a host could not be resolved: {error}"))
            .ok()
            .flatten()
    }
}

/**
 * What `ssh -G` printed, `said`: one setting a line, its key and then its value. Only
 * `hostname`, `user` and `port` are read, their keys in any case; every other line is ignored.
 * An answer without all three, or with a port that isn't one, resolves nothing.
 */
pub fn parse(said: &str) -> Option<Resolved> {
    let (mut hostname, mut user, mut port) = (None, None, None);
    for line in said.lines() {
        let Some((key, value)) = line.trim().split_once(char::is_whitespace) else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match key.to_ascii_lowercase().as_str() {
            "hostname" => hostname = Some(value.to_ascii_lowercase()),
            "user" => user = Some(value.to_string()),
            "port" => port = Some(value.parse::<u16>().ok().filter(|port| *port > 0)?),
            _ => {}
        }
    }
    Some(Resolved {
        hostname: hostname?,
        user: user?,
        port: port?,
    })
}

/// [`ResolveHost`] by [`plain`] with `user` for a host that names none: `ssh` with no config,
/// for tests and scripted runs.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoConfig;

impl ResolveHost for NoConfig {
    fn resolve(&self, host: &str) -> Option<Resolved> {
        plain(host, "user")
    }
}

/**
 * A stand-in for `ssh -G` with no config at all, as tests and scripted runs use: `user@host`
 * and `host` both end at `host` (lowercase), with the user given or `default_user`, at port
 * 22; `host:port` is not taken apart, as `ssh` doesn't. A host that is no host to hand `ssh`
 * resolves nothing, as with the real one.
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
        })
    }

    #[test]
    fn only_the_host_name_user_and_port_are_read() {
        let said = "user someone\nHostName Build.Example\nport 2222\nidentityfile ~/.ssh/id\n\
                    sendenv LANG\nproxyjump none\n";
        assert_eq!(parse(said), resolved("build.example", "someone", 2222));
        assert_eq!(
            parse("HOSTNAME a\r\nUSER b\r\nPORT 22\r\n"),
            resolved("a", "b", 22)
        );
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
    fn a_host_ssh_should_not_be_handed_is_not_resolved() {
        assert_eq!(SshConfig.resolve("-oProxyCommand=touch x"), None);
        assert_eq!(SshConfig.resolve("host name"), None);
    }
}
