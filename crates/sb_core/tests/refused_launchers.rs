//! A process that refuses launchers (a scripted run, PLAN §12) starts none: not `ssh`, not
//! `wsl.exe`, not a command of its own. Its own test binary, since the refusal lasts for the
//! life of the process.

use sb_core::CancelToken;
use sb_core::location::LocationKind;
use sb_core::transport::{
    LAUNCH_REFUSED, Launcher, ShellTransport, Transport, TransportError, refuse_launchers, wsl,
};

/// What `result` was refused with, naming the command it would have run.
fn refused<T: std::fmt::Debug>(result: Result<T, TransportError>) -> String {
    match result {
        Err(TransportError::Failed(said)) if said.starts_with(LAUNCH_REFUSED) => said,
        other => panic!("not refused: {other:?}"),
    }
}

#[test]
fn once_launchers_are_refused_nothing_is_started() {
    /* A launcher that leaves a mark when it is started. */
    let ran = tempfile::tempdir().unwrap();
    let marker = ran.path().join("ran");
    let marking = || {
        ShellTransport::new(Launcher::Command {
            program: "sh".into(),
            args: vec![
                "-c".to_string(),
                format!("echo > '{}'; exec sh -s", marker.display()),
            ],
        })
    };
    if cfg!(unix) {
        assert!(
            marking().list("/nowhere").is_ok(),
            "the launcher runs before"
        );
        std::fs::remove_file(&marker).unwrap();
    }

    refuse_launchers();

    assert!(refused(marking().list("/nowhere")).contains(": sh -c "));
    refused(marking().probe());
    assert!(!marker.exists(), "the refused launcher ran");

    let cancel = CancelToken::new();
    let host = LocationKind::Ssh {
        host: "user@macbook.invalid".to_string(),
    };
    let said = refused(host.transport(&cancel).env(&[]));
    assert!(
        said.starts_with(&format!("{LAUNCH_REFUSED}: ssh "))
            && said.contains(" user@macbook.invalid "),
        "{said}"
    );
    let distro = LocationKind::Wsl {
        distro: "Ubuntu-24.04".to_string(),
    };
    let said = refused(distro.transport(&cancel).list("/"));
    assert!(
        said.starts_with(&format!("{LAUNCH_REFUSED}: wsl.exe -d Ubuntu-24.04 ")),
        "{said}"
    );
    /* Elsewhere there is no WSL, and nothing is run to find that out. */
    if cfg!(windows) {
        refused(wsl::distros(&cancel));
    } else {
        assert_eq!(wsl::distros(&cancel).unwrap(), None);
    }
}
