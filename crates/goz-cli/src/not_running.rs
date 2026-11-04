//! Why gozd is not running, read from the state of its Windows service, and
//! the commands that fix it.
//!
//! Each command stands alone on its line, so it can be copied into cmd or
//! PowerShell as it is: `sc.exe` because PowerShell reads `sc` as
//! `Set-Content`, one command per line because Windows PowerShell 5.1 has no
//! `&&`, and `pushd` because an administrator terminal opens in System32.

use std::path::{Path, PathBuf};

use goz_core::proto::SERVICE_NAME;
use goz_winfs::ServiceState;

/// Where both binaries are published.
const RELEASES: &str = "https://github.com/mustafaahci/goz/releases/latest";

/// What to print, after es's line, when the daemon's pipe does not exist:
/// the goz service's state and the commands that fix it.
pub(crate) fn advice() -> String {
    let state = goz_winfs::service_state(SERVICE_NAME).map_err(|e| e.to_string());
    advice_for(state, gozd_folder().as_deref())
}

/// The folder of the running goz.exe, if gozd.exe is there too.
fn gozd_folder() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let folder = exe.parent()?;
    folder
        .join("gozd.exe")
        .is_file()
        .then(|| folder.to_path_buf())
}

/// [`advice`] for one service state; `gozd_folder` is where gozd.exe sits
/// beside goz.exe, if it does.
fn advice_for(state: Result<ServiceState, String>, gozd_folder: Option<&Path>) -> String {
    const ADMIN: &str = "open a terminal as administrator and run:";
    let name = SERVICE_NAME;
    match state {
        Ok(ServiceState::NotInstalled) => match gozd_folder {
            Some(folder) => format!(
                "goz: the {name} service is not installed. To install and start it, {ADMIN}\n  \
                 pushd \"{}\"\n  \
                 .\\gozd.exe install\n",
                folder.display()
            ),
            None => format!(
                "goz: the {name} service is not installed, and gozd.exe is not next to \
                 goz.exe. Get both from:\n  {RELEASES}\n"
            ),
        },
        Ok(ServiceState::Disabled) => format!(
            "goz: the {name} service is disabled. To turn it on, {ADMIN}\n  \
             sc.exe config {name} start= auto\n  \
             net start {name}\n"
        ),
        Ok(ServiceState::Stopped) => {
            format!(
                "goz: the {name} service is stopped. To start it, {ADMIN}\n  net start {name}\n"
            )
        }
        // Running without its pipe is the moment between the two at start.
        Ok(ServiceState::Starting | ServiceState::Running) => format!(
            "goz: the {name} service is starting and answers searches within a few \
             seconds, while it builds its index; try again shortly\n"
        ),
        Ok(ServiceState::Stopping) => format!(
            "goz: the {name} service is stopping. To start it again once it has stopped, \
             {ADMIN}\n  net start {name}\n"
        ),
        Ok(ServiceState::Paused) => format!(
            "goz: the {name} service is paused. To resume it, {ADMIN}\n  net continue {name}\n"
        ),
        Err(error) => format!(
            "goz: gozd is not running, and the state of the {name} service could not be \
             read: {error}\n"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The command lines of an advice: the lines indented two spaces.
    fn commands(advice: &str) -> Vec<&str> {
        advice
            .lines()
            .filter_map(|line| line.strip_prefix("  "))
            .collect()
    }

    #[test]
    fn each_state_prints_the_commands_that_fix_it() {
        let folder = Path::new(r"C:\Program Files\goz");
        let cases: [(ServiceState, Option<&Path>, &[&str]); 8] = [
            (
                ServiceState::NotInstalled,
                Some(folder),
                &[r#"pushd "C:\Program Files\goz""#, r".\gozd.exe install"],
            ),
            (ServiceState::NotInstalled, None, &[RELEASES]),
            (
                ServiceState::Disabled,
                None,
                &["sc.exe config goz start= auto", "net start goz"],
            ),
            (ServiceState::Stopped, None, &["net start goz"]),
            (ServiceState::Starting, None, &[]),
            (ServiceState::Running, None, &[]),
            (ServiceState::Stopping, None, &["net start goz"]),
            (ServiceState::Paused, None, &["net continue goz"]),
        ];
        for (state, folder, expected) in cases {
            let advice = advice_for(Ok(state), folder);
            assert_eq!(commands(&advice), expected, "{state:?}: {advice}");
            assert!(
                advice.starts_with("goz: ") && advice.ends_with('\n'),
                "{advice}"
            );
            // No dash as punctuation.
            assert!(!advice.contains(" - ") && !advice.contains(['\u{2013}', '\u{2014}']));
        }
        let unknown = advice_for(Err("OpenSCManagerW failed: Win32 error 5".into()), None);
        assert!(unknown.contains("Win32 error 5"), "{unknown}");
        assert!(commands(&unknown).is_empty(), "{unknown}");
    }

    /// The real query answers an unelevated user with a state, not with the
    /// error branch.
    #[test]
    fn the_service_query_answers_an_unelevated_user() {
        let advice = advice();
        assert!(!advice.contains("could not be read"), "{advice}");
    }
}
