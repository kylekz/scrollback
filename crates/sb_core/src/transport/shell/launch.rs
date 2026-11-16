//! [`Launcher`]: how the shell transport starts `sh -s` at a location (PLAN §6.2 to §6.4), and
//! what a launcher's failure means: a location that can't be reached just now, or an error.

use std::path::{Path, PathBuf};
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

/// What the far side's shell searches for its tools when [`Launcher::reading_home`] has
/// cleared its environment.
const CLEAN_PATH: &str = "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

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
    /// The program the launcher runs and its arguments, the program first.
    pub fn words(&self) -> Vec<String> {
        let words = |words: &[&str]| words.iter().map(|word| (*word).to_string()).collect();
        match self {
            /* `--cd /`: otherwise wsl.exe translates this process's working directory, and
            complains when it can't. `--exec`: no shell of the distro's reads the line. */
            Self::Wsl { distro } => words(&[
                wsl::WSL_EXE,
                "-d",
                distro,
                "--cd",
                "/",
                "--exec",
                "sh",
                "-s",
            ]),
            /*
             * `-T`: no terminal, whatever the config asks for, so the stream stays binary.
             * `LogLevel=INFO`, ssh's own default: a config that silences ssh would leave a
             * failure without a reason, and one that makes it chatty would bury the reason.
             * Not `ERROR`: OpenSSH for Windows says that a connection was refused at the
             * level below it, and with nothing said the host would count as failed where it
             * can't be reached. No forwardings: a second connection to a host must not take
             * the ports the owner's own session forwards. `--`: a host is never read as an
             * option.
             *
             * `ControlMaster=no`: with `auto` in the owner's config, a call that found no
             * master would become it, and every call's `ssh` is killed once its answer is
             * read, which would take down whatever else had come to share the connection.
             * A master that is already there is still used.
             */
            Self::Ssh { host } => words(&[
                "ssh",
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                &format!("ConnectTimeout={CONNECT_TIMEOUT}"),
                "-o",
                &format!("ServerAliveInterval={ALIVE_INTERVAL}"),
                "-o",
                &format!("ServerAliveCountMax={ALIVE_COUNT}"),
                "-o",
                "ClearAllForwardings=yes",
                "-o",
                "ControlMaster=no",
                "-o",
                "LogLevel=INFO",
                "--",
                host,
                "sh",
                "-s",
            ]),
            Self::Command { program, args } => std::iter::once(program.display().to_string())
                .chain(args.iter().cloned())
                .collect(),
        }
    }

    /// The command the launcher runs, as one would type it: to show what was run (DESIGN
    /// §4.12). A word a shell would take apart is in single quotes.
    pub fn command_line(&self) -> String {
        self.words()
            .iter()
            .map(|word| quoted(word))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /**
     * This launcher with the far side's shell started in an environment of nothing but a
     * plain `PATH` and `home` for its `HOME`: `… env -i PATH=… HOME=<home> sh -s`. For trying
     * a real distro or host on a generated tree in place of its user's own data (the app's
     * development override, the dev example's `--home`); never for the owner's own syncs.
     * `ssh` hands its command to the far side's shell as one line, so there the home is
     * quoted for that shell. A failure reads as the launcher's own would.
     */
    pub fn reading_home(&self, home: &str) -> Launcher {
        let mut words = self.words();
        let shell = words.split_off(words.len().saturating_sub(2));
        let home = match self {
            Self::Ssh { .. } => quoted(home),
            Self::Wsl { .. } | Self::Command { .. } => home.to_string(),
        };
        words.extend([
            "env".to_string(),
            "-i".to_string(),
            format!("PATH={CLEAN_PATH}"),
            format!("HOME={home}"),
        ]);
        words.extend(shell);
        let mut words = words.into_iter();
        Launcher::Command {
            program: words.next().unwrap_or_default().into(),
            args: words.collect(),
        }
    }

    /// The host the launcher hands `ssh`: its own, or the word after `--` of a command that
    /// runs `ssh` ([`Self::reading_home`]).
    fn host(&self) -> Option<&str> {
        match self {
            Self::Ssh { host } => Some(host),
            Self::Wsl { .. } => None,
            Self::Command { args, .. } if self.is_ssh() => args
                .iter()
                .position(|arg| arg == "--")
                .and_then(|at| args.get(at + 1))
                .map(String::as_str),
            Self::Command { .. } => None,
        }
    }

    /**
     * Whether the launcher may run: a host that is not one ssh should be handed
     * ([`crate::location::host_problem`]) is refused before anything starts, whichever way it
     * came: typed, or from a catalogue that holds it from before the check.
     */
    pub(super) fn check(&self) -> Result<(), TransportError> {
        match self.host().and_then(crate::location::host_problem) {
            Some(problem) => Err(TransportError::Failed(problem)),
            None => Ok(()),
        }
    }

    /// Whether the launcher's program is `ssh`, whose failures say what they are.
    fn is_ssh(&self) -> bool {
        match self {
            Self::Ssh { .. } => true,
            Self::Wsl { .. } => false,
            Self::Command { program, .. } => Path::new(program)
                .file_stem()
                .is_some_and(|name| name.eq_ignore_ascii_case("ssh")),
        }
    }

    /// Whether the launcher's program is `wsl.exe`, whose own words need decoding.
    fn is_wsl(&self) -> bool {
        match self {
            Self::Wsl { .. } => true,
            Self::Ssh { .. } => false,
            Self::Command { program, .. } => Path::new(program)
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case(wsl::WSL_EXE)),
        }
    }

    pub(super) fn command(&self) -> Command {
        let mut words = self.words().into_iter();
        let command = Command::new(words.next().unwrap_or_default()).args(words);
        if self.is_wsl() {
            /* Its own messages in UTF-8, where it knows the variable. */
            command.env(wsl::UTF8, "1")
        } else {
            command
        }
    }

    /**
     * Why a launcher's process ended before the shell's answer was whole: with `exit`, having
     * written `stderr`, and `stdout` before the answer began. A host that can't be reached is
     * [`TransportError::Unreachable`], which is no error (PLAN §5); the rest is one. The
     * first line of what the error carries is the launcher's verdict, the lines after it
     * whatever else it said.
     */
    pub(super) fn failure(
        &self,
        exit: Option<ExitStatus>,
        stderr: &[u8],
        stdout: &[u8],
    ) -> TransportError {
        let ended = || match exit {
            Some(exit) => format!("it ended before it answered ({exit})"),
            None => "it ended before it answered".to_string(),
        };
        if self.is_wsl() {
            /* wsl.exe says what is wrong with a distro first, and on either stream. */
            let said = format!("{}\n{}", wsl::decode(stdout), wsl::decode(stderr));
            let said = lines(&said);
            return TransportError::Failed(if said.is_empty() {
                ended()
            } else {
                said.join("\n")
            });
        }
        let stderr = String::from_utf8_lossy(stderr);
        let mut said = lines(&stderr);
        /* ssh, as most tools, says what is wrong last. */
        if let Some(verdict) = said.pop() {
            said.insert(0, verdict);
        }
        let said = said.join("\n");
        match self {
            _ if self.is_ssh() && exit.and_then(|exit| exit.code()) == Some(SSH_FAILED) => {
                ssh_failure(said)
            }
            _ if said.is_empty() => TransportError::Failed(ended()),
            _ => TransportError::Failed(said),
        }
    }
}

/// `word` as a POSIX shell takes it whole: as it is when it holds nothing a shell would take
/// apart, and in single quotes otherwise. For a command the owner is shown and may paste.
pub fn quoted(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|letter| letter.is_ascii_alphanumeric() || "-_=.,:/@%+".contains(letter));
    if plain {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

/// The lines of `text` that say something, without the space around them.
fn lines(text: &str) -> Vec<&str> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect()
}

/// What `ssh` meant when it failed having `said` this. Its messages are fixed English text.
fn ssh_failure(said: String) -> TransportError {
    /* Said when the host's key is new or has changed; under BatchMode `ssh` can't ask. */
    const HOST_KEY: [&str; 3] = [
        "Host key verification failed",
        "HOST IDENTIFICATION HAS CHANGED",
        "host key is known for",
    ];
    /* As ssh words a sign-in turned down: `Permission denied (publickey).` A tool at the
    host that was denied something says it without the bracket. */
    const REFUSED: [&str; 3] = [
        "Permission denied (",
        "Too many authentication failures",
        "Authentication failed",
    ];
    /* The host is asleep, off, or not on this network; or the connection broke. */
    const UNREACHABLE: [&str; 14] = [
        "Could not resolve hostname",
        "Connection timed out",
        "Operation timed out",
        "Connection refused",
        "No route to host",
        "Network is unreachable",
        "Network is down",
        "Host is down",
        "Connection reset",
        "Connection closed",
        "closed by remote host",
        "Broken pipe",
        "not responding",
        "Connection aborted",
    ];
    let says = |any: &[&str]| any.iter().any(|words| said.contains(words));
    if says(&HOST_KEY) {
        TransportError::HostKey(said)
    } else if says(&REFUSED) {
        TransportError::Refused(said)
    } else if says(&UNREACHABLE) {
        TransportError::Unreachable(said)
    } else if said.is_empty() {
        /* A connection that stalls or drops where ssh has no word for it. Its own
        mistakes (a bad option, a bad config) it always names. */
        TransportError::Unreachable("ssh gave up without saying why".to_string())
    } else {
        TransportError::Failed(said)
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn a_launcher_reading_another_home_clears_the_far_sides_environment() {
        let wsl = Launcher::Wsl {
            distro: "Ubuntu-24.04".to_string(),
        };
        assert_eq!(
            wsl.reading_home("/home/user/fixture").command_line(),
            format!(
                "wsl.exe -d Ubuntu-24.04 --cd / --exec env -i PATH={CLEAN_PATH} \
                 HOME=/home/user/fixture sh -s"
            )
        );
        /* ssh's far side reads its command as one line: a home with a space stays whole. */
        let ssh = Launcher::Ssh {
            host: "user@macbook".to_string(),
        };
        let words = ssh.reading_home("/Users/user/a fixture").words();
        assert_eq!(
            words[words.len() - 6..],
            [
                "env",
                "-i",
                &format!("PATH={CLEAN_PATH}"),
                "HOME='/Users/user/a fixture'",
                "sh",
                "-s"
            ]
        );
        assert!(
            ssh.reading_home("/Users/user/fixture")
                .command_line()
                .starts_with("ssh -T -o BatchMode=yes")
        );
        /* Its failures still read as ssh's and wsl.exe's own. */
        assert!(ssh.reading_home("/h").is_ssh() && wsl.reading_home("/h").is_wsl());
        assert!(!wsl.reading_home("/h").is_ssh());
    }

    /// What `launcher` runs: the program, its arguments, and what it adds to the environment.
    fn run(launcher: &Launcher) -> (Vec<String>, Vec<(String, String)>) {
        let command = launcher.command();
        let text = |text: &std::ffi::OsStr| text.to_str().unwrap().to_string();
        (
            std::iter::once(text(command.get_program().as_os_str()))
                .chain(command.get_args().iter().map(|arg| text(arg)))
                .collect(),
            command
                .get_envs()
                .map(|(name, value)| (text(name), text(value.unwrap())))
                .collect(),
        )
    }

    #[test]
    fn a_distro_and_a_host_are_arguments_and_never_options_or_shell_text() {
        let wsl = Launcher::Wsl {
            distro: "Ubuntu-24.04".to_string(),
        };
        assert_eq!(run(&wsl).0, wsl.words());
        /* wsl.exe is asked to speak UTF-8, however it is launched. */
        let utf8 = [("WSL_UTF8".to_string(), "1".to_string())];
        assert_eq!(run(&wsl).1, utf8);
        let wrapped = Launcher::Command {
            program: "tools/WSL.EXE".into(),
            args: vec!["--exec".to_string(), "sh".to_string(), "-s".to_string()],
        };
        assert_eq!(run(&wrapped).1, utf8);
        assert_eq!(
            wsl.command_line(),
            "wsl.exe -d Ubuntu-24.04 --cd / --exec sh -s"
        );
        assert_eq!(
            wsl.words(),
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
        let ssh = Launcher::Ssh {
            host: "-oProxyCommand=touch /tmp/x".to_string(),
        };
        assert_eq!(run(&ssh), (ssh.words(), Vec::new()));
        assert!(
            ssh.command_line()
                .ends_with("-o LogLevel=INFO -- '-oProxyCommand=touch /tmp/x' sh -s")
        );
        assert_eq!(
            ssh.words(),
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
                "-o",
                "ControlMaster=no",
                "-o",
                "LogLevel=INFO",
                "--",
                "-oProxyCommand=touch /tmp/x",
                "sh",
                "-s"
            ]
        );
    }

    /// A process's end with the exit code `code`.
    fn exit(code: u8) -> Option<ExitStatus> {
        #[cfg(unix)]
        let status = {
            use std::os::unix::process::ExitStatusExt as _;
            ExitStatus::from_raw(i32::from(code) << 8)
        };
        #[cfg(windows)]
        let status = {
            use std::os::windows::process::ExitStatusExt as _;
            ExitStatus::from_raw(u32::from(code))
        };
        Some(status)
    }

    #[test]
    fn a_launchers_end_is_read_by_which_launcher_it_is() {
        let said = |failure| match failure {
            TransportError::Failed(said) => said,
            other => panic!("{other}"),
        };
        let ssh = Launcher::Ssh {
            host: "user@macbook".to_string(),
        };
        let refused = b"ssh: connect to host macbook port 22: Connection refused\r\n";
        assert!(matches!(
            ssh.failure(exit(255), refused, b""),
            TransportError::Unreachable(_)
        ));
        /* Any other exit is the remote command's, whatever it wrote: the host was reached. */
        assert_eq!(
            said(ssh.failure(exit(127), refused, b"")),
            "ssh: connect to host macbook port 22: Connection refused"
        );
        /* What ssh says last is its verdict, and comes first; the rest follows. */
        let chatty = b"Warning: Permanently added 'macbook' to the list of known hosts.\r\n\
                       user@macbook: Permission denied (publickey).\r\n";
        match ssh.failure(exit(255), chatty, b"") {
            TransportError::Refused(said) => assert_eq!(
                said,
                "user@macbook: Permission denied (publickey).\n\
                 Warning: Permanently added 'macbook' to the list of known hosts."
            ),
            other => panic!("{other}"),
        }
        /* With nothing said, its own exit status still tells of a connection that failed. */
        for nothing in [&b""[..], b" \r\n"] {
            assert!(matches!(
                ssh.failure(exit(255), nothing, b"not an answer"),
                TransportError::Unreachable(said) if said == "ssh gave up without saying why"
            ));
        }
        assert_eq!(
            said(ssh.failure(exit(1), b"", b"")),
            format!("it ended before it answered ({})", exit(1).unwrap())
        );

        /* wsl.exe speaks in UTF-16, and in UTF-8 with WSL_UTF8 set, on either stream: what
        it says of a distro that is not installed, with its lines ended as it ends them. The
        first line is the reason, the next the rest of what it said. */
        let wsl = Launcher::Wsl {
            distro: "Ubuntu-24.04".to_string(),
        };
        let no_distro = "There is no distribution with the supplied name.\r\n\
                         Error code: Wsl/Service/WSL_E_DISTRO_NOT_FOUND\r\n";
        let utf16: Vec<u8> = no_distro
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let in_lines = "There is no distribution with the supplied name.\n\
                        Error code: Wsl/Service/WSL_E_DISTRO_NOT_FOUND";
        for words in [&utf16[..], no_distro.as_bytes()] {
            assert_eq!(said(wsl.failure(exit(255), b"", words)), in_lines);
            assert_eq!(said(wsl.failure(exit(255), words, b"")), in_lines);
        }

        let silent = Launcher::Command {
            program: "sh".into(),
            args: Vec::new(),
        };
        /* "exit status: 3", or "exit code: 3" on Windows. */
        assert_eq!(
            said(silent.failure(exit(3), b" \n", b"not an answer")),
            format!("it ended before it answered ({})", exit(3).unwrap())
        );
        assert_eq!(
            said(silent.failure(None, b"", b"")),
            "it ended before it answered"
        );
    }

    /// What OpenSSH prints for each kind of failure, with made-up hosts.
    #[test]
    fn what_ssh_says_tells_an_unreachable_host_from_a_failure() {
        let kind = |stderr: &str| match ssh_failure(stderr.to_string()) {
            TransportError::Unreachable(_) => "unreachable",
            TransportError::Refused(_) => "refused",
            TransportError::HostKey(_) => "host key",
            TransportError::Failed(_) => "failed",
            other => panic!("{other}"),
        };
        /* What ssh says only at its level INFO, and not at ERROR: of a host that stopped
        answering, and on Windows of a connection that was refused. They are heard because
        the launcher asks for that level. */
        let ssh = Launcher::Ssh {
            host: "user@macbook".to_string(),
        };
        assert!(ssh.words().contains(&"LogLevel=INFO".to_string()));
        for at_info in [
            "Timeout, server macbook.local not responding.",
            "banner exchange: Connection to UNKNOWN port -1: Connection refused",
        ] {
            assert!(
                matches!(
                    ssh.failure(exit(255), at_info.as_bytes(), b""),
                    TransportError::Unreachable(_)
                ),
                "{at_info}"
            );
        }
        for unreachable in [
            "ssh: Could not resolve hostname macbook.local: nodename nor servname provided, or not known",
            "ssh: Could not resolve hostname macbook: Name or service not known",
            /* OpenSSH for Windows. */
            "ssh: Could not resolve hostname macbook.local: No such host is known. ",
            "ssh: connect to host macbook.local port 22: Operation timed out",
            "ssh: connect to host 192.0.2.7 port 22: Connection timed out",
            "ssh: connect to host macbook.local port 22: Connection refused",
            "ssh: connect to host 192.0.2.7 port 22: No route to host",
            "ssh: connect to host 192.0.2.7 port 22: Network is unreachable",
            "ssh: connect to host 192.0.2.7 port 22: Network is down",
            /* A tool at the host that was denied something, and then the connection went. */
            "cat: /etc/motd.d/private: Permission denied\nclient_loop: send disconnect: Broken pipe",
            "ssh: connect to host macbook.local port 22: Host is down",
            "Connection timed out during banner exchange\r\nConnection to 192.0.2.7 port 22 timed out",
            "kex_exchange_identification: read: Connection reset by peer\r\nConnection reset by 192.0.2.7 port 22",
            "kex_exchange_identification: Connection closed by remote host\r\nConnection closed by 192.0.2.7 port 22",
            "client_loop: send disconnect: Broken pipe",
            "Connection to macbook.local closed by remote host.",
            /* Nothing at all. */
            "",
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
        ] {
            assert_eq!(kind(failed), "failed", "{failed}");
        }
    }
}
