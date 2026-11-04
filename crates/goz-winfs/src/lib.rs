//! goz-winfs: the thin unsafe Win32 layer.
//!
//! Contract: this crate produces and consumes raw byte buffers, plain typed
//! structs, and opaque handles only. It never parses ioctl record payloads
//! (USN records, MFT enum output, file-layout entries): all interpretation
//! lives in `goz-core`, where it is testable from fixtures on any OS. This
//! crate deliberately does NOT depend on `goz-core` (enforced in CI), which
//! keeps the boundary honest in both directions.
//!
//! On non-Windows targets the crate compiles to nothing, so `cargo test
//! --workspace` stays green everywhere.

#[cfg(windows)]
mod error;
#[cfg(windows)]
mod ioctl;
#[cfg(windows)]
mod util;

#[cfg(windows)]
mod cancel_io;
#[cfg(windows)]
mod clock;
#[cfg(windows)]
mod elevation;
#[cfg(windows)]
mod file_layout;
#[cfg(windows)]
mod file_stat;
#[cfg(windows)]
mod journal;
#[cfg(windows)]
mod links;
#[cfg(windows)]
mod pipe_sd;
#[cfg(windows)]
mod pipe_trust;
#[cfg(windows)]
mod volume;
#[cfg(windows)]
mod working_set;

#[cfg(all(windows, test))]
mod test_support;

#[cfg(windows)]
pub use error::WinError;

#[cfg(windows)]
pub use cancel_io::{ThreadIoHandle, cancel_synchronous_io, current_thread_io_handle};
#[cfg(windows)]
pub use clock::awake_time;
#[cfg(windows)]
pub use working_set::{SelfMemory, pin_working_set, self_memory, trim_working_set};

#[cfg(windows)]
pub use elevation::{enable_volume_privileges, is_elevated};

#[cfg(windows)]
pub use volume::{VolumeHandle, VolumeInfo, enumerate_volumes, open_volume, volume_total_bytes};

#[cfg(windows)]
pub use journal::{
    ERROR_JOURNAL_DELETE_IN_PROGRESS, ERROR_JOURNAL_ENTRY_DELETED, ERROR_JOURNAL_NOT_ACTIVE,
    JournalInfo, JournalQuery, create_usn_journal, enum_usn_data, query_usn_journal,
    read_usn_journal,
};

#[cfg(windows)]
pub use file_layout::{query_file_layout, query_file_layout_raw};

#[cfg(windows)]
pub use links::link_paths;

#[cfg(windows)]
pub use file_stat::{FileStat, stat_file};

#[cfg(windows)]
pub use pipe_sd::{PipeSecurity, build_pipe_security};

#[cfg(windows)]
pub use pipe_trust::{pipe_bytes_available, pipe_server_is_trusted};
