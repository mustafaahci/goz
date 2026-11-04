//! End-to-end exit-code contract for the `goz` binary.
//!
//! `goz_core::escompat` already unit-tests the argv rules themselves; what no
//! unit test can observe is whether `main` actually maps an `EsFatal` onto the
//! process exit code. These spawn the real binary and check that wiring.
//!
//! Only cases that need no running daemon live here. Anything past
//! `Client::connect` depends on whether `\\.\pipe\goz-v1` happens to be live on
//! the machine running the suite, which would pass on CI and flake on a
//! developer box with `gozd` running. The cases below all end before the
//! first connect attempt, so they are hermetic on any OS.

use std::process::{Command, Output};

fn goz<const N: usize>(args: [&str; N]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_goz"))
        .args(args)
        .output()
        .expect("failed to spawn the goz binary")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// es exit code 6. Parsing rejects the switch before any daemon contact.
#[test]
fn unknown_switch_exits_6() {
    let out = goz(["-bogus-switch", "foo"]);
    assert_eq!(out.status.code(), Some(6), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("unknown switch: -bogus-switch"),
        "stderr: {}",
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("run goz --help"),
        "stderr: {}",
        stderr(&out)
    );
}

/// The help switches print to stdout and exit 0 without a daemon: where
/// none runs, as on CI, any daemon round trip would exit 8 instead.
#[test]
fn help_prints_to_stdout_and_exits_0() {
    for (arg, screen) in [
        ("-h", "Common switches"),
        ("-?", "Common switches"),
        ("/?", "Common switches"),
        ("-help", "Exit codes"),
        ("--help", "Exit codes"),
    ] {
        let out = goz([arg]);
        assert_eq!(out.status.code(), Some(0), "{arg}: {}", stderr(&out));
        assert!(stderr(&out).is_empty(), "{arg}: {}", stderr(&out));
        let text = stdout(&out);
        assert!(text.contains("Usage: goz"), "{arg}: {text}");
        assert!(text.contains(screen), "{arg}: {text}");
    }
}

/// The version switches print goz's own version and exit 0 without a daemon.
#[test]
fn version_prints_to_stdout_and_exits_0() {
    for arg in ["-v", "-version", "--version"] {
        let out = goz([arg]);
        assert_eq!(out.status.code(), Some(0), "{arg}: {}", stderr(&out));
        assert_eq!(
            stdout(&out),
            format!("goz {}\n", env!("CARGO_PKG_VERSION")),
            "{arg}"
        );
    }
}

/// es exit code 4. `-path` consumes a following value; there is none here.
#[test]
fn missing_switch_parameter_exits_4() {
    let out = goz(["-path"]);
    assert_eq!(out.status.code(), Some(4), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("expected an additional parameter after -path"),
        "stderr: {}",
        stderr(&out)
    );
}

/// es exit code 4 for an argument goz cannot read at all: one that is not
/// valid Unicode used to panic, exit 101, before parsing began.
#[test]
fn a_non_unicode_argument_exits_4() {
    #[cfg(windows)]
    let bad = {
        use std::os::windows::ffi::OsStringExt;
        std::ffi::OsString::from_wide(&[u16::from(b'a'), 0xD800])
    };
    #[cfg(unix)]
    let bad = {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(vec![b'a', 0xFF])
    };
    let out = Command::new(env!("CARGO_BIN_EXE_goz"))
        .arg(&bad)
        .output()
        .expect("failed to spawn the goz binary");
    assert_eq!(out.status.code(), Some(4), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("goz: an argument is not valid Unicode: a\u{FFFD}"),
        "stderr: {}",
        stderr(&out)
    );
    assert!(
        !stderr(&out).contains("panicked"),
        "stderr: {}",
        stderr(&out)
    );
}

/// es exit code 5. The export file is created up front, so an uncreatable path
/// fails before `Client::connect` and needs no daemon.
///
/// Windows-only: on other targets `execute` short-circuits to exit 8 without
/// ever reading `-export-csv`, so this would fail on the CI ubuntu leg.
#[cfg(windows)]
#[test]
fn unwritable_export_path_exits_5() {
    // A directory that cannot exist, made unique so a stale tree cannot make
    // this pass for the wrong reason (a creatable path would reach connect).
    let missing = std::env::temp_dir()
        .join(format!("goz-absent-{}", std::process::id()))
        .join("out.csv");
    assert!(!missing.exists());

    let out = goz(["-export-csv", &missing.to_string_lossy(), "foo"]);
    assert_eq!(out.status.code(), Some(5), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("cannot create"),
        "stderr: {}",
        stderr(&out)
    );
}
