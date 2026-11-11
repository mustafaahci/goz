//! `Request::Watch`: change notification pushed to a client, so it need not
//! poll.
//!
//! A watch holds its own connection. The daemon answers `Watching`, then a
//! `Changed` frame listing the watched volumes whose generation moved since
//! the last frame: a journal batch, the live enricher's sizes and dates, a
//! hard-link reconcile, folder-size deltas, a rebuild (each moves the
//! generation and publishes it, see `VolumeState::write_index`), or the
//! volume being found or going away. Changes that come closer together
//! than `max(min_interval_ms, 100)` ms share a frame. Closing the pipe ends
//! the watch; so does any request sent on it.
//!
//! A watch is cheap, but each holds a pipe instance and a task, so a client
//! process may hold [`PER_PROCESS`] and the daemon [`PER_DAEMON`]. A watch
//! frees the slot it took among the daemon's general connections, so
//! watchers never starve queries. The pipe's security descriptor and the
//! clients' owner check are the same as for any other request.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use goz_core::proto::{ProtoError, Response, VolumeGen, encode_response_frame};
use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

use crate::volume_state::{VolumeRegistry, VolumeState, subscribe_changes};

/// The most watches one client process may hold.
pub(crate) const PER_PROCESS: usize = 16;
/// The most watches the daemon holds at once.
pub(crate) const PER_DAEMON: usize = 64;
/// The least time between two `Changed` frames, whatever the client asks.
pub(crate) const MIN_INTERVAL: Duration = Duration::from_millis(100);
/// How long a `Changed` frame may wait for a client that stopped reading.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// The watches held, by client process, and the daemon's slots for them.
pub(crate) struct Watchers {
    slots: Arc<Semaphore>,
    by_process: Mutex<HashMap<u32, usize>>,
}

/// One watch's slots, given back as it drops.
pub(crate) struct WatchSlot {
    watchers: Arc<Watchers>,
    process: Option<u32>,
    _slot: OwnedSemaphorePermit,
}

impl Drop for WatchSlot {
    fn drop(&mut self) {
        if let Some(pid) = self.process {
            let mut by = self.watchers.by_process.lock();
            if let Some(n) = by.get_mut(&pid) {
                *n -= 1;
                if *n == 0 {
                    by.remove(&pid);
                }
            }
        }
    }
}

impl Watchers {
    pub(crate) fn new() -> Arc<Watchers> {
        Arc::new(Watchers {
            slots: Arc::new(Semaphore::new(PER_DAEMON)),
            by_process: Mutex::new(HashMap::new()),
        })
    }

    /// A slot for a watch of client `process` (`None` when the pipe could
    /// not say which), or why there is none.
    pub(crate) fn claim(self: &Arc<Self>, process: Option<u32>) -> Result<WatchSlot, String> {
        let Ok(slot) = Arc::clone(&self.slots).try_acquire_owned() else {
            return Err(format!(
                "gozd holds {PER_DAEMON} watches, the most it holds; close one and watch again"
            ));
        };
        if let Some(pid) = process {
            let mut by = self.by_process.lock();
            let n = by.entry(pid).or_insert(0);
            if *n >= PER_PROCESS {
                return Err(format!(
                    "a process may hold {PER_PROCESS} watches; close one and watch again"
                ));
            }
            *n += 1;
        }
        Ok(WatchSlot {
            watchers: Arc::clone(self),
            process,
            _slot: slot,
        })
    }
}

/// Whether volume `v` is one the client names in `names` (`None`: every
/// volume): a mount, with or without its closing `\`, or its GUID path,
/// case-insensitively.
fn watched(v: &VolumeState, names: Option<&[String]>) -> bool {
    let Some(names) = names else {
        return true;
    };
    let trim = |s: &str| s.trim_end_matches('\\').to_ascii_lowercase();
    names.iter().any(|name| {
        let name = trim(name);
        trim(&v.guid) == name || v.mounts.iter().any(|m| trim(m) == name)
    })
}

/// The watched volumes now: GUID path → (mount, generation published).
fn snapshot(registry: &VolumeRegistry, names: Option<&[String]>) -> HashMap<String, (String, u64)> {
    registry
        .current()
        .iter()
        .filter(|v| watched(v, names))
        .map(|v| {
            (
                v.guid.clone(),
                (v.mount_prefix().to_string(), v.published()),
            )
        })
        .collect()
}

/// The volumes that changed from `before` to `now`: moved, found, or gone
/// (with the generation it had), in mount order.
fn changes(
    before: &HashMap<String, (String, u64)>,
    now: &HashMap<String, (String, u64)>,
) -> Vec<VolumeGen> {
    let mut out: Vec<VolumeGen> = now
        .iter()
        .filter(|(guid, (_, generation))| {
            before.get(*guid).is_none_or(|(_, was)| was != generation)
        })
        .chain(before.iter().filter(|(guid, _)| !now.contains_key(*guid)))
        .map(|(_, (mount, generation))| VolumeGen {
            mount: mount.clone(),
            generation: *generation,
        })
        .collect();
    out.sort_by(|a, b| a.mount.cmp(&b.mount));
    out
}

/// Writes one response frame, failing when the client has not taken it
/// within [`WRITE_TIMEOUT`].
async fn send<S: AsyncWrite + Unpin>(io: &mut S, response: &Response) -> std::io::Result<()> {
    let mut frame = Vec::new();
    encode_response_frame(response, &mut frame);
    tokio::time::timeout(WRITE_TIMEOUT, io.write_all(&frame))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "client stopped reading"))?
}

/// Runs a watch on `io` until the client closes it or sends anything: the
/// `Watching` reply, then a `Changed` frame for each change to a volume
/// `names` picks, no two closer than `max(min_interval_ms, 100)` ms.
/// `slot` is the watch's place under the caps, `None` when there is none,
/// which is answered with an error and ends the connection.
pub(crate) async fn run<S: AsyncRead + AsyncWrite + Unpin>(
    io: &mut S,
    registry: &VolumeRegistry,
    names: Option<Vec<String>>,
    min_interval_ms: u32,
    slot: Result<WatchSlot, String>,
) -> std::io::Result<()> {
    let _slot = match slot {
        Ok(slot) => slot,
        Err(message) => {
            return send(
                io,
                &Response::Error {
                    code: ProtoError::Unsupported,
                    message,
                },
            )
            .await;
        }
    };
    let names = names.as_deref();
    let interval = Duration::from_millis(u64::from(min_interval_ms)).max(MIN_INTERVAL);
    let mut changed = subscribe_changes();
    changed.mark_unchanged();
    let mut last = snapshot(registry, names);
    send(io, &Response::Watching).await?;
    let mut next_frame = Instant::now();
    let mut scratch = [0u8; 64];
    loop {
        tokio::select! {
            r = changed.changed() => {
                if r.is_err() {
                    return Ok(());
                }
            }
            // A close, or a request a watch connection does not carry.
            _ = io.read(&mut scratch) => return Ok(()),
        }
        // Let the changes that come within the interval share one frame.
        if Instant::now() < next_frame {
            tokio::select! {
                () = tokio::time::sleep_until(next_frame) => {}
                _ = io.read(&mut scratch) => return Ok(()),
            }
        }
        changed.mark_unchanged();
        let now = snapshot(registry, names);
        let volumes = changes(&last, &now);
        last = now;
        if volumes.is_empty() {
            continue;
        }
        send(io, &Response::Changed { volumes }).await?;
        next_frame = Instant::now() + interval;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use goz_core::proto::{FrameDecoder, MAX_SERVER_FRAME, decode_response_frame};
    use goz_core::types::VolumePhase;

    fn found(guid: &str, mount: &str) -> Arc<VolumeState> {
        let v = Arc::new(VolumeState::found(guid.into(), vec![mount.into()]));
        *v.phase.write() = VolumePhase::Live;
        v
    }

    /// The next frame the client reads, waiting at most `within`.
    async fn next(
        client: &mut tokio::io::DuplexStream,
        decoder: &mut FrameDecoder,
        within: Duration,
    ) -> Option<Response> {
        let deadline = Instant::now() + within;
        let mut buf = [0u8; 4096];
        loop {
            if let Ok(Some(frame)) = decoder.next_frame() {
                return Some(decode_response_frame(&frame).unwrap());
            }
            let n = tokio::time::timeout_at(deadline, client.read(&mut buf))
                .await
                .ok()?
                .unwrap();
            if n == 0 {
                return None;
            }
            decoder.feed(&buf[..n]);
        }
    }

    fn gens(r: Response) -> Vec<(String, u64)> {
        match r {
            Response::Changed { volumes } => volumes
                .into_iter()
                .map(|v| (v.mount, v.generation))
                .collect(),
            other => panic!("{other:?}"),
        }
    }

    /// A watch answers `Watching`, tells of each change to a watched
    /// volume (a journal batch, the enricher's sizes and dates, folder-size
    /// deltas, a volume found or gone), coalesces changes closer than the
    /// interval into one frame, ignores volumes it does not watch, and ends
    /// when the client closes the pipe.
    #[tokio::test(start_paused = true)]
    async fn a_watch_tells_of_changes_coalesced_until_the_pipe_closes() {
        let registry = Arc::new(VolumeRegistry::default());
        let c = found("guid-c", r"C:\");
        let d = found("guid-d", r"D:\");
        registry.add(Arc::clone(&c));
        registry.add(Arc::clone(&d));
        let (mut client, mut server) = tokio::io::duplex(1 << 16);
        let watchers = Watchers::new();
        let slot = watchers.claim(Some(1));
        let task = tokio::spawn({
            let registry = Arc::clone(&registry);
            async move {
                run(
                    &mut server,
                    &registry,
                    Some(vec!["c:".into(), "E:\\".into()]),
                    250,
                    slot,
                )
                .await
            }
        });
        let mut decoder = FrameDecoder::new(MAX_SERVER_FRAME);
        let wait = Duration::from_secs(5);
        assert_eq!(
            next(&mut client, &mut decoder, wait).await,
            Some(Response::Watching)
        );

        // The enricher's sizes and dates.
        c.write_index().note_changed();
        assert_eq!(
            gens(next(&mut client, &mut decoder, wait).await.unwrap()),
            [(r"C:\".to_string(), 1)]
        );
        // Three changes within the interval: one frame, the last generation.
        c.write_index().note_changed();
        c.write_index().note_folder_sizes_changed();
        c.write_index().note_changed();
        let started = Instant::now();
        assert_eq!(
            gens(next(&mut client, &mut decoder, wait).await.unwrap()),
            [(r"C:\".to_string(), 4)]
        );
        assert!(started.elapsed() >= Duration::from_millis(200));
        // A volume not watched moves: nothing.
        d.write_index().note_changed();
        assert_eq!(
            next(&mut client, &mut decoder, Duration::from_secs(2)).await,
            None
        );
        // A write that changes nothing is not told.
        drop(c.write_index());
        assert_eq!(
            next(&mut client, &mut decoder, Duration::from_secs(2)).await,
            None
        );
        // A watched name found later, and a watched volume gone.
        let e = found("guid-e", r"E:\");
        registry.add(e);
        assert_eq!(
            gens(next(&mut client, &mut decoder, wait).await.unwrap()),
            [(r"E:\".to_string(), 0)]
        );
        registry.remove("guid-c");
        assert_eq!(
            gens(next(&mut client, &mut decoder, wait).await.unwrap()),
            [(r"C:\".to_string(), 4)]
        );
        drop(client);
        task.await.unwrap().unwrap();
        assert_eq!(watchers.by_process.lock().len(), 0, "the slot is freed");
        assert_eq!(watchers.slots.available_permits(), PER_DAEMON);
    }

    /// 16 watches a process, 64 the daemon; a slot freed is a slot again.
    #[test]
    fn watches_are_capped_per_process_and_per_daemon() {
        let watchers = Watchers::new();
        let mine: Vec<WatchSlot> = (0..PER_PROCESS)
            .map(|_| watchers.claim(Some(7)).unwrap())
            .collect();
        assert!(watchers.claim(Some(7)).is_err());
        let mut others: Vec<WatchSlot> = (0..PER_DAEMON - PER_PROCESS)
            .map(|i| watchers.claim(Some(100 + i as u32 / 8)).unwrap())
            .collect();
        assert!(watchers.claim(Some(9999)).is_err());
        assert!(watchers.claim(None).is_err());
        others.pop();
        assert!(watchers.claim(Some(9999)).is_ok());
        drop(mine);
        assert!(watchers.claim(Some(7)).is_ok());
    }

    /// Over a cap, the watch is refused with why, and the connection ends.
    #[tokio::test]
    async fn a_watch_over_the_cap_is_refused() {
        let registry = VolumeRegistry::default();
        let (mut client, mut server) = tokio::io::duplex(1 << 12);
        run(&mut server, &registry, None, 0, Err("too many".to_string()))
            .await
            .unwrap();
        drop(server);
        let mut decoder = FrameDecoder::new(MAX_SERVER_FRAME);
        let r = next(&mut client, &mut decoder, Duration::from_secs(1)).await;
        assert!(matches!(r, Some(Response::Error { message, .. }) if message == "too many"));
    }

    /// Mounts match with or without their `\`, in any case, and GUID paths.
    #[test]
    fn watched_names() {
        let v = VolumeState::found(
            r"\\?\Volume{1}\".into(),
            vec![r"C:\".into(), r"D:\mnt\x\".into()],
        );
        for name in ["C:", r"c:\", r"d:\MNT\x", r"\\?\volume{1}"] {
            assert!(watched(&v, Some(&[name.to_string()])), "{name}");
        }
        assert!(!watched(&v, Some(&["E:".to_string()])));
        assert!(!watched(&v, Some(&[])));
        assert!(watched(&v, None));
    }
}
