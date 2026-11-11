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

use std::process::{Command, Output, Stdio};

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

/// `goz --help` with `stdout`, its stderr captured.
fn help_into(stdout: Stdio) -> Output {
    Command::new(env!("CARGO_BIN_EXE_goz"))
        .arg("--help")
        .stdout(stdout)
        .output()
        .expect("failed to spawn the goz binary")
}

/// A reader that closed early, as in `goz --help | more` quit at once, is a
/// clean stop. Its end is gone before goz starts, so the write always fails.
#[test]
fn help_into_a_closed_pipe_exits_0() {
    let (reader, writer) = std::io::pipe().expect("a pipe");
    drop(reader);
    let out = help_into(writer.into());
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(stderr(&out).is_empty(), "stderr: {}", stderr(&out));
}

/// A stdout that refuses the bytes exits 5 instead of 0 with the output
/// lost. The help shares this write with --status and --server-version,
/// which need a daemon to reach it.
#[test]
fn help_to_an_unwritable_stdout_exits_5() {
    #[cfg(windows)]
    let (unwritable, path) = {
        let path = std::env::temp_dir().join(format!("goz-read-only-{}", std::process::id()));
        std::fs::write(&path, b"").expect("a temp file");
        let file = std::fs::File::open(&path).expect("the temp file, read only");
        (file, Some(path))
    };
    #[cfg(unix)]
    let (unwritable, path) = (
        std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .expect("/dev/full"),
        None::<std::path::PathBuf>,
    );
    let out = help_into(unwritable.into());
    if let Some(path) = path {
        let _ = std::fs::remove_file(path);
    }
    assert_eq!(out.status.code(), Some(5), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).starts_with("goz: writing to stdout failed: "),
        "stderr: {}",
        stderr(&out)
    );
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

/// Exit 4 for search text that cannot be searched as written, such as an
/// unbalanced `<`, with the parser's message: refused before any daemon
/// contact, as gozd would refuse it. `|` and `!` are operators, not errors,
/// so they never get here.
#[test]
fn malformed_search_text_exits_4() {
    for (args, message) in [
        (
            &["<foo", "bar"][..],
            "goz: unbalanced '<' at column 1: the group it opens is never closed with '>'",
        ),
        (
            &["foo|bar>"][..],
            "goz: unbalanced '>' at column 8: there is no '<' for it to close",
        ),
        (&["-argv", "<a|b"][..], "goz: unbalanced '<' at column 1"),
        (&["gr<a|e>y"][..], "group expansion such as gr<a|e>y"),
        (
            &["!content:x"][..],
            "unknown search function or modifier 'content:'",
        ),
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_goz"))
            .args(args)
            .output()
            .expect("failed to spawn the goz binary");
        assert_eq!(out.status.code(), Some(4), "{args:?}: {}", stderr(&out));
        assert!(stderr(&out).contains(message), "{args:?}: {}", stderr(&out));
        assert!(stdout(&out).is_empty(), "{args:?}: {}", stdout(&out));
    }
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
        stderr(&out).contains("goz: the command line is not valid Unicode: ")
            && stderr(&out).contains("a\u{FFFD}"),
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
