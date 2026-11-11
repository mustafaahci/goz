//! `content:` in the CLI: the files are read here, under the user's own
//! account, never by gozd.
//!
//! goz-core splits the query (`goz_core::query::content`): gozd runs the 2^n
//! queries around the n content terms (at most 3), each with every match and
//! no paging, and the client keeps which of them each entry matched. An
//! entry that matched all of them matches and one that matched none does
//! not, unread; the rest are read, a file at a time on a few threads, a term
//! at a time and only while its answer can change the verdict. The matches
//! are then sorted as gozd sorts (`sort_items`) and paged here, so offsets,
//! limits, counts and total sizes are those of the whole search, as in
//! Everything.
//!
//! A file the user cannot read, or one whose data is not on the disk (a
//! cloud file not downloaded, whose read would download it), has no
//! content. stderr shows how many files are read while a console shows it,
//! and Ctrl+C stops the search: goz then exits as a program Ctrl+C ends
//! does (0xC000013A), with no results written.

use std::collections::HashMap;
use std::fs::File;
use std::io::{IsTerminal, Write};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, mpsc};
use std::time::Duration;

use goz_core::escompat::EsPlan;
use goz_core::proto::{PAGE_ROWS, QueryRequest, QueryResults, ResultItem};
use goz_core::query::{ContentEnv, ContentQuery, TextFilter, sort_items};
use goz_core::types::{SortKey, SortSpec};

use crate::client::{Client, ClientError};
use crate::timing::Timing;
use crate::win::{IncompleteWarning, Stop, emit, nothing_matched, number_only, request_failed};

/// The process exit code of a console program Ctrl+C ends
/// (`STATUS_CONTROL_C_EXIT`).
const CTRL_C_EXIT: u32 = 0xC000_013A;
/// The null filter's class id: Windows reads no text from such a file.
const NULL_FILTER: &str = "{098F2470-BAE0-11CD-B579-08002B30BFEB}";
/// `FILE_ATTRIBUTE_OFFLINE`, `FILE_ATTRIBUTE_RECALL_ON_OPEN` and
/// `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`: the data is not on the disk.
const NOT_LOCAL: u32 = 0x1000 | 0x4_0000 | 0x40_0000;
/// `FILE_FLAG_SEQUENTIAL_SCAN`.
const SEQUENTIAL_SCAN: u32 = 0x0800_0000;
/// How often the progress line is redrawn.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

/// Why the candidates stopped coming.
enum Halt {
    Client(ClientError),
    Cancelled,
}

impl From<ClientError> for Halt {
    fn from(e: ClientError) -> Self {
        Halt::Client(e)
    }
}

/// An entry some query matched, and which ones.
struct Candidate {
    item: ResultItem,
    matched: u8,
}

/// What reading needs from Windows, shared by the reading threads.
struct Env<'a> {
    cancel: &'a AtomicBool,
    filters: Mutex<HashMap<String, TextFilter>>,
}

impl ContentEnv for Env<'_> {
    fn ansi(&self, bytes: &[u8], out: &mut String) {
        goz_winfs::ansi_to_string(bytes, out);
    }

    fn filter(&self, ext: &str) -> TextFilter {
        let mut filters = self.filters.lock().unwrap_or_else(|e| e.into_inner());
        *filters.entry(ext.to_string()).or_insert_with(|| {
            match goz_winfs::persistent_handler(ext) {
                None => TextFilter::None,
                Some(id) if id == NULL_FILTER => TextFilter::Null,
                Some(_) => TextFilter::Other,
            }
        })
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// The file an entry names, as Windows spells it.
fn os_path(item: &ResultItem) -> std::ffi::OsString {
    use std::os::windows::ffi::OsStringExt;
    match &item.path_u16 {
        Some(units) => std::ffi::OsString::from_wide(units),
        None => item.path.clone().into(),
    }
}

/// Whether term `t` of `split` matches the content of the file `item`.
fn answer(split: &ContentQuery, t: usize, item: &ResultItem, env: &Env<'_>) -> bool {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    let path = os_path(item);
    let Ok(meta) = std::fs::metadata(&path) else {
        return false;
    };
    if meta.file_attributes() & NOT_LOCAL != 0 {
        return false;
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(SEQUENTIAL_SCAN)
        .open(&path)
    else {
        return false;
    };
    let name = item.path.rsplit('\\').next().unwrap_or(&item.path);
    split.terms()[t]
        .search(&mut file, name, env)
        .unwrap_or(false)
}

/// Runs a search with content terms: the candidates from gozd, the files
/// read, and the matches written as the plan asks.
pub(crate) fn run(
    plan: &EsPlan,
    connection: &mut Client,
    query: QueryRequest,
    split: &ContentQuery,
    export_file: Option<File>,
    timing: &mut Option<Timing>,
) -> Result<ExitCode, ExitCode> {
    let local = AtomicBool::new(false);
    let cancel: &AtomicBool = goz_winfs::ctrl_c_flag().unwrap_or(&local);
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut incomplete = false;
    let mut pending = false;
    for (a, text) in split.queries().iter().enumerate() {
        let request = QueryRequest {
            query: text.clone(),
            sort: SortSpec::default(),
            offset: 0,
            limit: None,
            want_size: true,
            want_mtime: true,
            want_total_size: false,
            // The rows are sorted here, by the value of the sort's column.
            want_ctime: query.want_ctime || query.sort.key == SortKey::DateCreated,
            want_atime: query.want_atime || query.sort.key == SortKey::DateAccessed,
            want_attrib: query.want_attrib || query.sort.key == SortKey::Attributes,
            want_rtime: query.want_rtime || query.sort.key == SortKey::DateRecentlyChanged,
            ..query.clone()
        };
        let fetched = connection.query(request, |page: QueryResults| {
            if cancel.load(Ordering::Relaxed) {
                return Err(Halt::Cancelled);
            }
            incomplete |= page.volumes_incomplete;
            pending |= page.metadata_pending;
            for item in page.items {
                // A lossy path is told apart by its exact units.
                let key = match &item.path_u16 {
                    Some(units) => format!("{units:?}"),
                    None => item.path.clone(),
                };
                let at = *seen.entry(key).or_insert_with(|| {
                    candidates.push(Candidate { item, matched: 0 });
                    candidates.len() - 1
                });
                candidates[at].matched |= 1 << a;
            }
            Ok(())
        });
        match fetched {
            Ok(()) => {}
            Err(Halt::Cancelled) => cancelled(),
            Err(Halt::Client(e)) => return Err(request_failed(e)),
        }
    }
    drop(seen);

    // The verdicts that need no reading, and the files that do.
    let mut verdicts: Vec<Option<bool>> = candidates
        .iter()
        .map(|c| match split.decided(c.matched) {
            Some(v) => Some(v),
            // A folder has no content: every answer is no.
            None if c.item.is_dir => Some(split.verdict(c.matched, |_| false)),
            None => None,
        })
        .collect();
    let to_read: Vec<usize> = (0..candidates.len())
        .filter(|&i| verdicts[i].is_none())
        .collect();
    let read = read_all(split, &candidates, &to_read, cancel);
    if cancel.load(Ordering::Relaxed) {
        cancelled();
    }
    for (&i, v) in to_read.iter().zip(read) {
        verdicts[i] = Some(v);
    }
    let mut matches: Vec<ResultItem> = candidates
        .into_iter()
        .zip(verdicts)
        .filter(|(_, v)| *v == Some(true))
        .map(|(c, _)| c.item)
        .collect();

    if incomplete {
        IncompleteWarning::default().check(&QueryResults {
            total: 0,
            items: Vec::new(),
            more: false,
            volumes_incomplete: true,
            metadata_pending: false,
            generation: 0,
            total_size: None,
        });
    }
    let total = matches.len() as u64;
    let no_results = plan.no_result_error && total == 0;
    let finish = |code: ExitCode| {
        if no_results {
            Err(nothing_matched())
        } else {
            Ok(code)
        }
    };
    if plan.result_count {
        return number_only(plan, total, export_file).and_then(finish);
    }
    if plan.total_size {
        if pending {
            eprintln!("goz: warning: some file sizes are not read yet; the total leaves them out");
        }
        let size = matches
            .iter()
            .filter(|m| !m.is_dir)
            .filter_map(|m| m.size)
            .fold(0u64, u64::saturating_add);
        return number_only(plan, size, export_file).and_then(finish);
    }
    sort_items(&mut matches, query.sort);
    let start = (query.offset as usize).min(matches.len());
    let end = query
        .limit
        .map_or(matches.len(), |l| (start + l as usize).min(matches.len()));
    let mut page: Vec<ResultItem> = matches.drain(start..end).collect();
    drop(matches);
    let code = emit(plan, export_file, timing, |write| {
        if page.is_empty() {
            return write(results(total, Vec::new(), false, pending));
        }
        while !page.is_empty() {
            let rest = page.split_off(page.len().min(PAGE_ROWS as usize));
            let items = std::mem::replace(&mut page, rest);
            write(results(total, items, !page.is_empty(), pending))?;
        }
        Ok::<(), Stop>(())
    })?;
    finish(code)
}

/// One page of a content search's matches.
fn results(total: u64, items: Vec<ResultItem>, more: bool, pending: bool) -> QueryResults {
    QueryResults {
        total,
        items,
        more,
        // Said once, before the pages, by `run`.
        volumes_incomplete: false,
        metadata_pending: pending,
        generation: 0,
        total_size: None,
    }
}

/// Reads the files of `candidates` at `to_read` on a few threads, giving
/// each its verdict, in order; the progress on stderr while a console shows
/// it.
fn read_all(
    split: &ContentQuery,
    candidates: &[Candidate],
    to_read: &[usize],
    cancel: &AtomicBool,
) -> Vec<bool> {
    let env = Env {
        cancel,
        filters: Mutex::new(HashMap::new()),
    };
    let verdicts: Vec<AtomicBool> = to_read.iter().map(|_| AtomicBool::new(false)).collect();
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .clamp(1, 8)
        .min(to_read.len().max(1));
    let show = std::io::stderr().is_terminal() && !to_read.is_empty();
    let (finished, ended) = mpsc::channel::<()>();
    std::thread::scope(|s| {
        for _ in 0..threads {
            let finished = finished.clone();
            let (env, verdicts, next, done) = (&env, &verdicts, &next, &done);
            s.spawn(move || {
                loop {
                    let k = next.fetch_add(1, Ordering::Relaxed);
                    if k >= to_read.len() || cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    let c = &candidates[to_read[k]];
                    let v = split.verdict(c.matched, |t| answer(split, t, &c.item, env));
                    verdicts[k].store(v, Ordering::Relaxed);
                    done.fetch_add(1, Ordering::Relaxed);
                }
                let _ = finished.send(());
            });
        }
        drop(finished);
        let mut left = threads;
        let mut drawn = false;
        while left > 0 {
            match ended.recv_timeout(PROGRESS_EVERY) {
                Ok(()) => left -= 1,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if show {
                let line = format!(
                    "\rgoz: searching content: {} of {} files",
                    done.load(Ordering::Relaxed),
                    to_read.len()
                );
                let _ = std::io::stderr().write_all(line.as_bytes());
                drawn = true;
            }
        }
        if drawn {
            let _ = std::io::stderr().write_all(format!("\r{:60}\r", "").as_bytes());
        }
    });
    verdicts.into_iter().map(AtomicBool::into_inner).collect()
}

/// Ends the run as Ctrl+C asked, with nothing written.
fn cancelled() -> ! {
    eprintln!("goz: content search cancelled");
    let _ = std::io::stderr().flush();
    std::process::exit(CTRL_C_EXIT as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16le(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    /// Files written to disk and read back through Windows: the null filter
    /// `.bin` has (read from the registry) gives no text, a file of no
    /// filter is searched as bytes, the ANSI code page reads an accent
    /// (é is 0xE9 in every Western and Turkish code page), and a cloud file
    /// not on the disk, here an offline one, is not read.
    #[test]
    fn files_are_read_through_windows_as_everything_reads_them() {
        let dir = std::env::temp_dir().join(format!("goz-content-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let blob = [&b"\x00\x01\x02needle\x00\x00"[..], &utf16le("wneedle")].concat();
        let files: [(&str, Vec<u8>); 5] = [
            ("b.bin", blob.clone()),
            ("b.dat", blob),
            ("ansi.txt", b"r\xe9sum\xe9 here\n".to_vec()),
            ("w16.txt", utf16le("widechar text\n")),
            ("off.txt", b"needle\n".to_vec()),
        ];
        for (name, bytes) in &files {
            std::fs::write(dir.join(name), bytes).unwrap();
        }
        let item = |name: &str| ResultItem {
            path: dir.join(name).to_str().unwrap().to_string(),
            path_u16: None,
            size: None,
            mtime_ft: None,
            is_dir: false,
            ..ResultItem::default()
        };
        let cancel = AtomicBool::new(false);
        let env = Env {
            cancel: &cancel,
            filters: Mutex::new(HashMap::new()),
        };
        let hit = |query: &str, name: &str| {
            let split = ContentQuery::new(query, false).unwrap();
            answer(&split, 0, &item(name), &env)
        };
        assert!(!hit("content:needle", "b.bin"), "the null filter");
        assert!(hit("ansicontent:needle", "b.bin"));
        assert!(hit("content:needle", "b.dat") && hit("content:wneedle", "b.dat"));
        assert!(hit("content:resume", "ansi.txt") && hit("content:r\u{e9}sum\u{e9}", "ansi.txt"));
        assert!(hit("content:widechar", "w16.txt"));
        assert!(hit("content:needle", "off.txt"));
        assert!(!hit("content:needle", "missing.txt"));
        // Offline: the data is elsewhere, so it is not read.
        let off = dir.join("off.txt");
        let status = std::process::Command::new("attrib")
            .arg("+O")
            .arg(&off)
            .status();
        if status.is_ok_and(|s| s.success()) {
            assert!(!hit("content:needle", "off.txt"));
            let _ = std::process::Command::new("attrib")
                .arg("-O")
                .arg(&off)
                .status();
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
