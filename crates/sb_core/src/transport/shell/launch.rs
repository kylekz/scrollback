//! [`Launcher`]: how the shell transport starts `sh -s` at a location (PLAN §6.2 to §6.4), and
//! what a launcher's failure means: a location that can't be reached just now, or an error.

use std::path::PathBuf;
use std::process::ExitStatus;

use toolbox_process::Command;

use crate::transport::{TransportError, wsl};

/// How long `ssh` may take to connect, in seconds.
const CONNECT_TIMEOUT: u32 = 10;

/// How often `ssh` asks a silent host whether it is still there, in seconds, and how many
/// unanswered askings end the connection: a host that went to sleep in the middle of a pass
/// is given up on within a minute instead of whenever TCP notices.
const ALIVE_INTERVAL: u32 = 15;
const ALIVE_COUNT: u32 = 3;

/// The exit status `ssh` keeps for its own failures; any other is the remote command's.
const SSH_FAILED: i32 = 255;

/// What starts a POSIX shell that reads its script on stdin. Only a fixed argument list ever
/// crosses a command line: the script, with everything a request names, goes in on stdin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Launcher {
    /// A WSL distro, as its default user: `wsl.exe -d <distro> --cd / --exec sh -s`. It starts
    /// a distro that is stopped.
    Wsl { distro: String },
    /// A host as the owner would name it to `ssh`: an alias from their `~/.ssh/config`, or
    /// `user@host`. Their config, keys and agent decide the rest; `ssh` may not prompt, so no
    /// password or passphrase is ever asked for or stored (PLAN D7).
    Ssh { host: String },
    /// A program that runs `sh -s`, with its arguments: a local `sh` for tests and for a
    /// loopback.
    Command { program: PathBuf, args: Vec<String> },
}

impl Launcher {
    pub(super) fn command(&self) -> Command {
        match self {
            /* `--cd /`: otherwise wsl.exe translates this process's working directory, and
            complains when it can't. `--exec`: no shell of the distro's reads the line. */
            Self::Wsl { distro } => {
                Command::new(wsl::WSL_EXE).args(["-d", distro, "--cd", "/", "--exec", "sh", "-s"])
            }
            /* `-T`: no terminal, whatever the config asks for, so the stream stays binary.
            No forwardings: a second connection to a host must not take the ports the
            owner's own session forwards. `--`: a host is never read as an option. */
            Self::Ssh { host } => Command::new("ssh")
                .arg("-T")
                .args(
                    [
                        "BatchMode=yes".to_string(),
                        format!("ConnectTimeout={CONNECT_TIMEOUT}"),
                        format!("ServerAliveInterval={ALIVE_INTERVAL}"),
                        format!("ServerAliveCountMax={ALIVE_COUNT}"),
                        "ClearAllForwardings=yes".to_string(),
                    ]
                    .into_iter()
                    .flat_map(|option| ["-o".to_string(), option]),
                )
                .args(["--", host, "sh", "-s"]),
            Self::Command { program, args } => Command::new(program).args(args),
        }
    }

    /**
     * Why a launcher's process ended before the shell's answer was whole: with `exit`, having
     * written `stderr`, and `stdout` before the answer began. A host that can't be reached is
     * [`TransportError::Unreachable`], which is no error (PLAN §5); the rest is one.
     */
    pub(super) fn failure(
        &self,
        exit: Option<ExitStatus>,
        stderr: &str,
        stdout: &[u8],
    ) -> TransportError {
        let ended = || match exit {
            Some(exit) => format!("it ended before it answered ({exit})"),
            None => "it ended before it answered".to_string(),
        };
        match self {
            Self::Ssh { .. } if exit.and_then(|exit| exit.code()) == Some(SSH_FAILED) => {
                ssh_failure(stderr)
            }
            /* wsl.exe says what is wrong with a distro on stdout, in UTF-16. */
            Self::Wsl { .. } => TransportError::Failed(
                [stderr.trim(), wsl::decode(stdout).trim()]
                    .into_iter()
                    .find(|said| !said.is_empty())
                    .map_or_else(ended, str::to_string),
            ),
            _ if stderr.trim().is_empty() => TransportError::Failed(ended()),
            _ => TransportError::Failed(stderr.trim().to_string()),
        }
    }
}

/// What `ssh` meant when it failed with `stderr`. Its messages are fixed English text.
fn ssh_failure(stderr: &str) -> TransportError {
    /* Said when the host's key is new or has changed; under BatchMode `ssh` can't ask. */
    const HOST_KEY: [&str; 3] = [
        "Host key verification failed",
        "HOST IDENTIFICATION HAS CHANGED",
        "host key is known for",
    ];
    const REFUSED: [&str; 3] = [
        "Permission denied",
        "Too many authentication failures",
        "Authentication failed",
    ];
    /* The host is asleep, off, or not on this network; or the connection broke. */
    const UNREACHABLE: [&str; 13] = [
        "Could not resolve hostname",
        "Connection timed out",
        "Operation timed out",
        "Connection refused",
        "No route to host",
        "Network is unreachable",
        "Host is down",
        "Connection reset",
        "Connection closed",
        "closed by remote host",
        "Broken pipe",
        "not responding",
        "Connection aborted",
    ];
    let said = stderr.trim().to_string();
    let says = |any: &[&str]| any.iter().any(|words| said.contains(words));
    if says(&HOST_KEY) {
        TransportError::HostKey(said)
    } else if says(&REFUSED) {
        TransportError::Refused(said)
    } else if says(&UNREACHABLE) {
        TransportError::Unreachable(said)
    } else if said.is_empty() {
        TransportError::Failed("ssh failed without saying why".to_string())
    } else {
        TransportError::Failed(said)
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn args(launcher: &Launcher) -> Vec<String> {
        let command = launcher.command();
        std::iter::once(command.get_program().to_str().unwrap().to_string())
            .chain(
                command
                    .get_args()
                    .iter()
                    .map(|arg| arg.to_str().unwrap().to_string()),
            )
            .collect()
    }

    #[test]
    fn a_distro_and_a_host_are_arguments_and_never_options_or_shell_text() {
        assert_eq!(
            args(&Launcher::Wsl {
                distro: "Ubuntu-24.04".to_string()
            }),
            [
                "wsl.exe",
                "-d",
                "Ubuntu-24.04",
                "--cd",
                "/",
                "--exec",
                "sh",
                "-s"
            ]
        );
        /* A host that looks like an option comes after `--`, where ssh takes it for a host. */
        assert_eq!(
            args(&Launcher::Ssh {
                host: "-oProxyCommand=touch /tmp/x".to_string()
            }),
            [
                "ssh",
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "-o",
                "ClearAllForwardings=yes",
                "--",
                "-oProxyCommand=touch /tmp/x",
                "sh",
                "-s"
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_launchers_end_is_read_by_which_launcher_it_is() {
        use std::os::unix::process::ExitStatusExt as _;

        let exit = |code: i32| Some(ExitStatus::from_raw(code << 8));
        let said = |failure| match failure {
            TransportError::Failed(said) => said,
            other => panic!("{other}"),
        };
        let ssh = Launcher::Ssh {
            host: "user@macbook".to_string(),
        };
        let refused = "ssh: connect to host macbook port 22: Connection refused\r\n";
        assert!(matches!(
            ssh.failure(exit(255), refused, b""),
            TransportError::Unreachable(_)
        ));
        /* Any other exit is the remote command's, whatever it wrote: the host was reached. */
        assert_eq!(said(ssh.failure(exit(127), refused, b"")), refused.trim());

        /* wsl.exe speaks on stdout, in UTF-16. */
        let wsl = Launcher::Wsl {
            distro: "Ubuntu-24.04".to_string(),
        };
        let no_distro: Vec<u8> = "There is no distribution with the supplied name.\r\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(
            said(wsl.failure(exit(255), "", &no_distro)),
            "There is no distribution with the supplied name."
        );

        let silent = Launcher::Command {
            program: "sh".into(),
            args: Vec::new(),
        };
        assert_eq!(
            said(silent.failure(exit(3), " \n", b"not an answer")),
            "it ended before it answered (exit status: 3)"
        );
        assert_eq!(
            said(silent.failure(None, "", b"")),
            "it ended before it answered"
        );
    }

    /// What OpenSSH prints for each kind of failure, with made-up hosts.
    #[test]
    fn what_ssh_says_tells_an_unreachable_host_from_a_failure() {
        let kind = |stderr: &str| match ssh_failure(stderr) {
            TransportError::Unreachable(_) => "unreachable",
            TransportError::Refused(_) => "refused",
            TransportError::HostKey(_) => "host key",
            TransportError::Failed(_) => "failed",
            other => panic!("{other}"),
        };
        for unreachable in [
            "ssh: Could not resolve hostname macbook.local: nodename nor servname provided, or not known",
            "ssh: Could not resolve hostname macbook: Name or service not known",
            "ssh: connect to host macbook.local port 22: Operation timed out",
            "ssh: connect to host 192.0.2.7 port 22: Connection timed out",
            "ssh: connect to host macbook.local port 22: Connection refused",
            "ssh: connect to host 192.0.2.7 port 22: No route to host",
            "ssh: connect to host 192.0.2.7 port 22: Network is unreachable",
            "ssh: connect to host macbook.local port 22: Host is down",
            "Connection timed out during banner exchange\r\nConnection to 192.0.2.7 port 22 timed out",
            "kex_exchange_identification: read: Connection reset by peer\r\nConnection reset by 192.0.2.7 port 22",
            "kex_exchange_identification: Connection closed by remote host\r\nConnection closed by 192.0.2.7 port 22",
            "Timeout, server macbook.local not responding.",
            "client_loop: send disconnect: Broken pipe",
            "Connection to macbook.local closed by remote host.",
        ] {
            assert_eq!(kind(unreachable), "unreachable", "{unreachable}");
        }
        for refused in [
            "user@macbook.local: Permission denied (publickey,password,keyboard-interactive).",
            "Received disconnect from 192.0.2.7 port 22:2: Too many authentication failures\r\nDisconnected from 192.0.2.7 port 22",
        ] {
            assert_eq!(kind(refused), "refused", "{refused}");
        }
        for host_key in [
            "Host key verification failed.",
            "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n\
             @    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\n\
             @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n\
             IT IS POSSIBLE THAT SOMEONE IS DOING SOMETHING NASTY!\n\
             Host key for macbook.local has changed and you have requested strict checking.\n\
             Host key verification failed.",
            "No ED25519 host key is known for macbook.local and you have requested strict checking.\n\
             Host key verification failed.",
        ] {
            assert_eq!(kind(host_key), "host key", "{host_key}");
        }
        for failed in [
            "/home/user/.ssh/config: line 7: Bad configuration option: hostnam\n\
             /home/user/.ssh/config: terminating, 1 bad configuration options",
            "Bad owner or permissions on /home/user/.ssh/config",
            "",
        ] {
            assert_eq!(kind(failed), "failed", "{failed}");
        }
    }
}
