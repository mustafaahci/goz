//! Wire protocol payload types.
//!
//! Plain serde data: JSON on the wire, no I/O anywhere. Both enums are
//! internally tagged (`"t"`) so a reader can dispatch on one field, and every
//! struct tolerates unknown JSON fields (serde default), so additive protocol
//! evolution needs no version bump.

use serde::{Deserialize, Serialize};

use crate::types::{SortSpec, VolumePhase};

/// Protocol version spoken by this build.
///
/// A daemon speaks exactly this version: it answers a [`Request::Hello`] whose
/// `proto_min..=proto_max` range covers it, and otherwise refuses with
/// [`ProtoError::Unsupported`] rather than agreeing on a version one side
/// cannot actually speak.
pub const PROTO_VERSION: u16 = 1;

/// The daemon's named pipe.
pub const PIPE_NAME: &str = r"\\.\pipe\goz-v1";

/// The Windows service the daemon installs as, the SCM and registry key
/// name. Keep it stable across versions; it is independent of [`PIPE_NAME`].
pub const SERVICE_NAME: &str = "goz";

/// Server-side decoder cap for client→server frames (1 MiB). A length prefix
/// is a DoS lever; oversize frames poison the connection.
pub const MAX_CLIENT_FRAME: u32 = 1 << 20;

/// Client-side decoder cap for server→client frames (16 MiB).
pub const MAX_SERVER_FRAME: u32 = 16 << 20;

/// Maximum [`ResultItem`]s per `Results` frame; larger result sets are split
/// into multiple frames with [`QueryResults::more`] set on all but the last.
pub const PAGE_ROWS: u32 = 4096;

/// A client→server message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum Request {
    /// Version negotiation.
    ///
    /// Optional, and the v1 CLI does not send it: it ships in lockstep with the
    /// daemon, so there is no version to discover, and the daemon serves
    /// `Query`/`Status` without one. A client built separately from the daemon
    /// should send it first and honor a refusal.
    Hello {
        /// Lowest protocol version the client can speak.
        proto_min: u16,
        /// Highest protocol version the client can speak.
        proto_max: u16,
        /// Human-readable client identifier (name/version), for logs.
        client: String,
    },
    /// Run a search query.
    Query(QueryRequest),
    /// Ask for per-volume index status.
    Status,
}

/// Parameters of one search query.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueryRequest {
    /// The search string, in the query language's syntax.
    pub query: String,
    /// Optional scope folder (es `-path`), already absolutized by the client.
    pub scope: Option<String>,
    pub sort: SortSpec,
    /// Number of leading matches to skip (pagination).
    pub offset: u32,
    /// Maximum matches to return; `None` = all.
    pub limit: Option<u32>,
    /// Populate [`ResultItem::size`] (skip the lookup otherwise).
    pub want_size: bool,
    /// Populate [`ResultItem::mtime_ft`] (skip the lookup otherwise).
    pub want_mtime: bool,
    pub match_case: bool,
    /// Fill [`QueryResults::total_size`]: es's `-get-total-size`. A daemon
    /// predating the field ignores it and leaves `total_size` out.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub want_total_size: bool,
}

/// A server→client message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum Response {
    /// Reply to [`Request::Hello`].
    Hello {
        /// Negotiated protocol version.
        proto: u16,
        /// Human-readable server identifier (name/version), for logs.
        server: String,
        /// Whether the index is ready to serve queries.
        ready: bool,
        /// What the daemon supports right now, by name: `"folder_sizes"`
        /// while folder sizes are indexed and settled, and the other names
        /// the protocol defines as their features land. Empty from a daemon
        /// predating the field.
        #[serde(default)]
        features: Vec<String>,
    },
    /// One page of query results.
    Results(QueryResults),
    /// Reply to [`Request::Status`].
    Status(DaemonStatus),
    /// The request failed.
    Error {
        /// Machine-readable failure class.
        code: ProtoError,
        /// Human-readable detail for the user/log.
        message: String,
    },
}

/// One page of matches for a [`QueryRequest`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueryResults {
    /// Full match count regardless of paging (es `totitems` semantics).
    pub total: u64,
    /// The matches in this page, at most [`PAGE_ROWS`].
    pub items: Vec<ResultItem>,
    /// `true` → another `Results` frame follows for the same query.
    pub more: bool,
    /// `true` → at least one volume is bootstrapping/rescanning/failed, so
    /// results may be incomplete. The honesty bit: never presented silently.
    pub volumes_incomplete: bool,
    /// `true` → size/mtime enrichment has not finished; requested metadata
    /// columns may still be `None`.
    pub metadata_pending: bool,
    /// Always 0, kept so the wire and `--json` keep their shape. A query can
    /// span volumes that each have their own generation, so no one number
    /// could say whether its results are still current.
    pub generation: u64,
    /// The summed size in bytes of every matching file, regardless of
    /// paging, when [`QueryRequest::want_total_size`] asked for it; `None`
    /// otherwise, or from a daemon predating the field. A folder adds
    /// nothing, nor a file whose size is not read yet, and each name of a
    /// hard-linked file adds its size, as each is its own row. A sum past
    /// `u64::MAX`, which only the names of huge sparse files reach, stops
    /// there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_size: Option<u64>,
}

/// One matched file or directory.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResultItem {
    /// Full path, WTF-8 lossy-decoded to UTF-8 (lone surrogates → U+FFFD).
    pub path: String,
    /// Exact UTF-16 code units of the path, present ONLY when [`Self::path`]
    /// was lossy (the name contained unpaired surrogates). Clients that need
    /// exact fidelity reconstruct the `OsString` from this.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub path_u16: Option<Vec<u16>>,
    /// Size in bytes: a file's, or a folder's total while the daemon indexes
    /// folder sizes (`Hello.features` lists `"folder_sizes"`); `None` =
    /// unknown or not requested, and a folder's from a daemon without them.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub size: Option<u64>,
    /// Raw FILETIME modification stamp; `None` = unknown or not requested.
    /// The client formats it (locale/timezone stays client-side).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub mtime_ft: Option<i64>,
    pub is_dir: bool,
}

/// Daemon-wide index status.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DaemonStatus {
    /// Per-volume status, one entry per indexed volume.
    pub volumes: Vec<VolumeStatus>,
    /// `true` → at least one volume is not fully live.
    pub volumes_incomplete: bool,
    /// Daemon working-set bytes (what Task Manager's Memory column tracks),
    /// self-reported at the moment of the status request. 0 = unavailable.
    #[serde(default)]
    pub process_working_set: u64,
    /// Daemon commit charge (private bytes). This is the real footprint the
    /// process holds regardless of what is currently resident. 0 = unavailable.
    #[serde(default)]
    pub process_private_bytes: u64,
}

/// Heap bytes of one index component on the wire: live bytes and allocated
/// (capacity) bytes. See `IndexMemory` in goz-core for what each component is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MemPair {
    pub used: u64,
    pub alloc: u64,
}

/// Per-component memory breakdown of one volume's index, for `--status`.
/// All-zero when the daemon predates this field (serde default).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VolumeMemory {
    pub entries: MemPair,
    pub arena_raw: MemPair,
    pub arena_folded: MemPair,
    pub frn_map: MemPair,
    /// Per-unique-name tables (pairs, refcounts, chain heads, intern table).
    pub name_tables: MemPair,
    pub dir_children: MemPair,
    /// Which FRN-map backing is in use ("dense" / "sparse").
    pub frn_map_kind: String,
}

/// Status of one indexed volume.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VolumeStatus {
    /// Stable volume GUID path identifier.
    pub guid: String,
    /// Mount points (drive letters / folder mounts) of the volume.
    pub mounts: Vec<String>,
    pub phase: VolumePhase,
    /// Number of indexed entries on the volume.
    pub entries: u64,
    /// Index generation for the volume.
    pub generation: u64,
    /// `true` → size/mtime enrichment still running for this volume.
    pub metadata_pending: bool,
    /// Maximum size of the volume's USN journal in bytes; `None` for a volume
    /// without one (or from daemons predating the field). It bounds how long
    /// the daemon can be down and still replay changes instead of rescanning.
    #[serde(default)]
    pub journal_max_bytes: Option<u64>,
    /// `true` while entries sit under lost+found because their parent never
    /// arrived. Every query then pays a presentability walk per candidate, so
    /// a healthy volume reports `false`.
    #[serde(default)]
    pub has_orphans: bool,
    /// `true` while a record has closed a parent cycle that no record has
    /// broken yet: the entries in it have no full path, and every query pays
    /// the presentability walk, as with [`Self::has_orphans`].
    #[serde(default)]
    pub has_parent_cycle: bool,
    /// Distinct names waiting to join the volume's name order (the sorted
    /// list a name-sorted page walks). They join on the tail's next merge;
    /// until then each costs a name-sorted query a binary search.
    #[serde(default)]
    pub names_waiting: u64,
    /// Index drift counters. Non-zero values are not errors on their own (a
    /// missed delete or a slot reuse is normal on a busy volume), but they are
    /// the only outward sign that the index and the volume have diverged.
    #[serde(default)]
    pub placeholders_created: u64,
    #[serde(default)]
    pub delete_of_unknown: u64,
    #[serde(default)]
    pub stale_slots: u64,
    /// Hard-link changes whose Win32 link-set walk could not complete (file
    /// gone, locked, or a parent not yet indexed). See
    /// `ApplyOutcome::needs_link_reconcile`: successful walks are reconciled
    /// live; these are skipped and counted. Non-zero means a few names on this
    /// volume may be stale until the next rescan.
    #[serde(default)]
    pub link_reconciles_dropped: u64,
    /// Per-component index memory breakdown. `None` from daemons predating it.
    #[serde(default)]
    pub memory: Option<VolumeMemory>,
}

/// Machine-readable failure class carried by [`Response::Error`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProtoError {
    /// The query string failed to parse, or the query needed longer than
    /// gozd lets one query search a volume.
    BadQuery,
    /// Unexpected server-side failure.
    Internal,
    /// A frame exceeded the receiver's size cap.
    TooLarge,
    /// The client requested an unsupported protocol version or feature.
    Unsupported,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{SortDir, SortKey};
    use crate::wtf8;

    fn rt_request(req: &Request) {
        let json = serde_json::to_string(req).expect("serialize");
        let back: Request = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(&back, req, "json: {json}");
    }

    fn rt_response(resp: &Response) {
        let json = serde_json::to_string(resp).expect("serialize");
        let back: Response = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(&back, resp, "json: {json}");
    }

    fn sample_query() -> QueryRequest {
        QueryRequest {
            query: "*.rs size:>1kb".into(),
            scope: Some(r"C:\src".into()),
            sort: SortSpec {
                key: SortKey::Size,
                dir: SortDir::Desc,
            },
            offset: 128,
            limit: Some(50),
            want_size: true,
            want_mtime: false,
            match_case: true,
            want_total_size: true,
        }
    }

    fn sample_items() -> Vec<ResultItem> {
        vec![
            ResultItem {
                path: r"C:\src\main.rs".into(),
                path_u16: None,
                size: Some(1234),
                mtime_ft: Some(133_500_000_000_000_000),
                is_dir: false,
            },
            ResultItem {
                path: r"C:\src".into(),
                path_u16: None,
                size: None,
                mtime_ft: None,
                is_dir: true,
            },
        ]
    }

    #[test]
    fn request_hello_round_trips() {
        rt_request(&Request::Hello {
            proto_min: 1,
            proto_max: 3,
            client: "goz-cli 0.1.0".into(),
        });
    }

    #[test]
    fn request_query_round_trips() {
        rt_request(&Request::Query(sample_query()));
        // Minimal form too: no scope, no limit.
        rt_request(&Request::Query(QueryRequest {
            query: String::new(),
            scope: None,
            sort: SortSpec::default(),
            offset: 0,
            limit: None,
            want_size: false,
            want_mtime: false,
            match_case: false,
            want_total_size: false,
        }));
    }

    #[test]
    fn request_status_round_trips() {
        rt_request(&Request::Status);
    }

    #[test]
    fn response_hello_round_trips() {
        rt_response(&Response::Hello {
            proto: PROTO_VERSION,
            server: "gozd 0.1.0".into(),
            ready: false,
            features: vec!["folder_sizes".into()],
        });
    }

    #[test]
    fn response_results_round_trips() {
        rt_response(&Response::Results(QueryResults {
            total: 987_654,
            items: sample_items(),
            more: true,
            volumes_incomplete: true,
            metadata_pending: true,
            generation: 42,
            total_size: Some(u64::MAX),
        }));
        // Empty page.
        rt_response(&Response::Results(QueryResults {
            total: 0,
            items: vec![],
            more: false,
            volumes_incomplete: false,
            metadata_pending: false,
            generation: 0,
            total_size: None,
        }));
    }

    #[test]
    fn response_status_round_trips_every_phase() {
        let phases = [
            VolumePhase::Bootstrapping,
            VolumePhase::Live,
            VolumePhase::Rescanning,
            VolumePhase::Offline,
            VolumePhase::Failed {
                reason: "journal wrapped".into(),
            },
        ];
        let volumes: Vec<VolumeStatus> = phases
            .into_iter()
            .enumerate()
            .map(|(i, phase)| VolumeStatus {
                guid: format!(r"\\?\Volume{{0000000{i}-aaaa-bbbb-cccc-ddddeeeeffff}}\"),
                mounts: vec![format!(r"{}:\", char::from(b'C' + i as u8))],
                phase,
                entries: 1_000_000 + i as u64,
                generation: i as u64,
                metadata_pending: i % 2 == 0,
                journal_max_bytes: (i % 2 == 1).then_some(64 << 20),
                has_orphans: i % 3 == 0,
                has_parent_cycle: i % 2 == 1,
                names_waiting: 7 * i as u64,
                placeholders_created: i as u64,
                delete_of_unknown: 2 * i as u64,
                stale_slots: 3 * i as u64,
                link_reconciles_dropped: 4 * i as u64,
                memory: Some(VolumeMemory {
                    entries: MemPair {
                        used: 10 * i as u64,
                        alloc: 20 * i as u64,
                    },
                    frn_map_kind: "sparse".into(),
                    ..VolumeMemory::default()
                }),
            })
            .collect();
        rt_response(&Response::Status(DaemonStatus {
            volumes,
            volumes_incomplete: true,
            process_working_set: 123_456_789,
            process_private_bytes: 987_654_321,
        }));
    }

    #[test]
    fn response_error_round_trips_every_code() {
        for code in [
            ProtoError::BadQuery,
            ProtoError::Internal,
            ProtoError::TooLarge,
            ProtoError::Unsupported,
        ] {
            rt_response(&Response::Error {
                code,
                message: format!("{code:?} happened"),
            });
        }
    }

    #[test]
    fn clean_result_item_omits_optional_fields() {
        let item = ResultItem {
            path: r"C:\clean.txt".into(),
            path_u16: None,
            size: None,
            mtime_ft: None,
            is_dir: false,
        };
        let json = serde_json::to_string(&item).expect("serialize");
        assert!(!json.contains("path_u16"), "json: {json}");
        assert!(!json.contains("size"), "json: {json}");
        assert!(!json.contains("mtime_ft"), "json: {json}");
    }

    #[test]
    fn absent_optional_fields_deserialize_to_none() {
        let item: ResultItem =
            serde_json::from_str(r#"{"path":"C:\\a.txt","is_dir":false}"#).expect("deserialize");
        assert_eq!(item.path_u16, None);
        assert_eq!(item.size, None);
        assert_eq!(item.mtime_ft, None);
    }

    #[test]
    fn lossy_path_carries_exact_code_units() {
        // A name with an unpaired high surrogate: WTF-8 flags it lossy, so
        // the item ships path_u16 with the exact original units.
        let units: Vec<u16> = vec![b'C' as u16, b':' as u16, b'\\' as u16, 0xD800, b'x' as u16];
        let mut wtf8_bytes = Vec::new();
        let lossy = wtf8::from_utf16(&units, &mut wtf8_bytes);
        assert!(lossy);
        let item = ResultItem {
            path: wtf8::to_string_lossy(&wtf8_bytes),
            path_u16: Some(units.clone()),
            size: Some(7),
            mtime_ft: Some(-1),
            is_dir: false,
        };
        assert!(item.path.contains('\u{FFFD}'));
        let json = serde_json::to_string(&item).expect("serialize");
        assert!(json.contains("path_u16"), "json: {json}");
        let back: ResultItem = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, item);
        assert_eq!(back.path_u16.as_deref(), Some(units.as_slice()));
    }

    /// A v0.5.4 client sends no `want_total_size`, and its query must still
    /// parse; a new client that does not ask sends none either.
    #[test]
    fn want_total_size_is_optional_both_ways() {
        let req: Request = serde_json::from_str(
            r#"{"t":"Query","query":"a","scope":null,"sort":{"key":"Name","dir":"Asc"},
                "offset":0,"limit":0,"want_size":true,"want_mtime":true,"match_case":false}"#,
        )
        .expect("a query without the field");
        assert!(matches!(req, Request::Query(q) if !q.want_total_size));
        let mut q = sample_query();
        q.want_total_size = false;
        let json = serde_json::to_string(&Request::Query(q)).expect("serialize");
        assert!(!json.contains("want_total_size"), "json: {json}");
    }

    /// `--json` prints [`QueryResults`]: a page that was not asked for a
    /// total size must print as it did before the field.
    #[test]
    fn results_without_total_size_omit_it() {
        let page = QueryResults {
            total: 1,
            items: vec![],
            more: false,
            volumes_incomplete: false,
            metadata_pending: false,
            generation: 0,
            total_size: None,
        };
        let json = serde_json::to_string(&page).expect("serialize");
        assert!(!json.contains("total_size"), "json: {json}");
    }

    /// A daemon predating the field reports no parent cycle.
    #[test]
    fn status_without_has_parent_cycle_reads_false() {
        let v: VolumeStatus = serde_json::from_str(
            r#"{"guid":"g","mounts":[],"phase":{"phase":"Live"},"entries":1,"generation":0,
                "metadata_pending":false,"has_orphans":true}"#,
        )
        .expect("an older status");
        assert!(v.has_orphans);
        assert!(!v.has_parent_cycle);
    }

    #[test]
    fn unknown_json_fields_are_ignored() {
        // Additive evolution: a newer peer may send fields we don't know.
        let resp: Response = serde_json::from_str(
            r#"{"t":"Hello","proto":1,"server":"gozd","ready":true,"new_field":[1,2,3]}"#,
        )
        .expect("top-level extra field");
        assert_eq!(
            resp,
            Response::Hello {
                proto: 1,
                server: "gozd".into(),
                ready: true,
                features: Vec::new(),
            },
            "a daemon predating the features lists none"
        );

        let req: Request = serde_json::from_str(
            r#"{"t":"Query","query":"a","scope":null,"sort":{"key":"Name","dir":"Asc"},
                "offset":0,"limit":null,"want_size":false,"want_mtime":false,
                "match_case":false,"future_flag":true}"#,
        )
        .expect("nested extra field");
        assert!(matches!(req, Request::Query(q) if q.query == "a"));
    }
}
