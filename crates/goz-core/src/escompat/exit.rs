//! goz's process exit codes, with what each means.

/// How a goz run ends, as its process exit code.
///
/// Codes 4 to 9 keep es.exe's numbers and meanings, so a script written
/// against es reads them the same way; 10 is goz's own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// 0: the run did what it was asked.
    Success,
    /// 4: a switch is missing its parameter or got one goz cannot use, an
    /// argument is not valid Unicode, or gozd refused the search.
    BadParam,
    /// 5: the export file could not be created, or writing the output failed.
    WriteFailed,
    /// 6: an unknown switch.
    UnknownSwitch,
    /// 7: the pipe could not be opened, or gozd sent a malformed reply or
    /// none in time.
    Protocol,
    /// 8: gozd is not running.
    NotRunning,
    /// 9: `-no-result-error` was given and nothing matched.
    NoResults,
    /// 10: the pipe server is not the trusted gozd.
    Untrusted,
}

impl Exit {
    /// Every exit code, in the order the help lists them.
    pub const ALL: [Exit; 8] = [
        Exit::Success,
        Exit::BadParam,
        Exit::WriteFailed,
        Exit::UnknownSwitch,
        Exit::Protocol,
        Exit::NotRunning,
        Exit::NoResults,
        Exit::Untrusted,
    ];

    /// The process exit code.
    pub const fn code(self) -> u8 {
        match self {
            Exit::Success => 0,
            Exit::BadParam => 4,
            Exit::WriteFailed => 5,
            Exit::UnknownSwitch => 6,
            Exit::Protocol => 7,
            Exit::NotRunning => 8,
            Exit::NoResults => 9,
            Exit::Untrusted => 10,
        }
    }

    /// What the code means, as the help prints it.
    pub const fn meaning(self) -> &'static str {
        match self {
            Exit::Success => "Success",
            Exit::BadParam => {
                "A switch is missing its parameter or got one goz cannot use, \
                 an argument is not valid Unicode, or gozd refused the search"
            }
            Exit::WriteFailed => {
                "The export file could not be created, or writing the output failed"
            }
            Exit::UnknownSwitch => "Unknown switch",
            Exit::Protocol => {
                "The pipe to gozd could not be opened, or gozd sent a malformed reply \
                 or none in time"
            }
            Exit::NotRunning => "gozd is not running",
            Exit::NoResults => "-no-result-error was given and nothing matched",
            Exit::Untrusted => "The pipe server is not the trusted gozd",
        }
    }
}

impl From<Exit> for std::process::ExitCode {
    fn from(exit: Exit) -> Self {
        Self::from(exit.code())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The numbers are a contract with scripts, es's among them: pin them.
    #[test]
    fn codes_keep_their_numbers() {
        let codes: Vec<u8> = Exit::ALL.iter().map(|exit| exit.code()).collect();
        assert_eq!(codes, [0, 4, 5, 6, 7, 8, 9, 10]);
    }
}
