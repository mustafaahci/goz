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
    /// Read gozd's index options. Sent only to a daemon whose
    /// `Hello.features` lists [`feature::CONFIG`].
    GetConfig,
    /// Change gozd's index options. Accepted only from a caller whose token
    /// is an elevated administrator; anyone else gets an `Error` "needs
    /// administrator". Answered with the options as they now are.
    SetConfig(ConfigChange),
}

/// The names `Response::Hello`'s `features` lists: what the daemon supports
/// right now. A client sends a request kind, a query flag or a sort key a
/// feature names only when the daemon lists it.
pub mod feature {
    /// `Request::Watch` is understood.
    pub const WATCH: &str = "watch";
    /// `SortKey::DateCreated` is understood. A property that is not indexed
    /// sorts by reading from disk, which is slow and budgeted.
    pub const SORT_DATE_CREATED: &str = "sort:date_created";
    /// `SortKey::DateAccessed` is understood.
    pub const SORT_DATE_ACCESSED: &str = "sort:date_accessed";
    /// `SortKey::Attributes` is understood.
    pub const SORT_ATTRIBUTES: &str = "sort:attributes";
    /// `SortKey::DateRecentlyChanged` is understood.
    pub const SORT_DATE_RECENTLY_CHANGED: &str = "sort:date_recently_changed";
    /// The query flags `want_ctime`, `want_atime`, `want_attrib` and
    /// `want_rtime` are understood.
    pub const COLUMNS: &str = "columns";
    /// Folder sizes are indexed and settled.
    pub const FOLDER_SIZES: &str = "folder_sizes";
    /// Dates created are indexed and settled.
    pub const INDEXED_DATE_CREATED: &str = "indexed:date_created";
    /// Dates accessed are indexed and settled.
    pub const INDEXED_DATE_ACCESSED: &str = "indexed:date_accessed";
    /// Attributes are indexed and settled.
    pub const INDEXED_ATTRIBUTES: &str = "indexed:attributes";
    /// Recent changes are indexed and settled.
    pub const INDEXED_RECENT_CHANGES: &str = "indexed:recent_changes";
    /// `Request::GetConfig` and `Request::SetConfig` are understood.
    pub const CONFIG: &str = "config";
}

/// gozd's index options, under Everything 1.5's names (Options > Indexes)
/// and with Everything's defaults. A column that is off costs no memory: a
/// query or a column that needs it reads it from disk instead, slowly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IndexConfig {
    pub index_folder_size: bool,
    pub index_date_created: bool,
    pub index_date_accessed: bool,
    pub index_attributes: bool,
    pub index_recent_changes: bool,
    pub fast_size_sort: bool,
    pub fast_date_modified_sort: bool,
    pub fast_date_created_sort: bool,
    pub fast_date_accessed_sort: bool,
    pub fast_attributes_sort: bool,
    pub fast_extension_sort: bool,
}

impl Default for IndexConfig {
    /// Everything 1.5's defaults: folder sizes, a fast size sort and a fast
    /// date modified sort on, every other column and fast sort off.
    fn default() -> Self {
        Self {
            index_folder_size: true,
            index_date_created: false,
            index_date_accessed: false,
            index_attributes: false,
            index_recent_changes: false,
            fast_size_sort: true,
            fast_date_modified_sort: true,
            fast_date_created_sort: false,
            fast_date_accessed_sort: false,
            fast_attributes_sort: false,
            fast_extension_sort: false,
        }
    }
}

/// Every option's key, Everything's spelling, in the order the options
/// page lists them.
pub const CONFIG_KEYS: [&str; 11] = [
    "index_folder_size",
    "index_date_created",
    "fast_date_created_sort",
    "index_date_accessed",
    "fast_date_accessed_sort",
    "index_attributes",
    "fast_attributes_sort",
    "index_recent_changes",
    "fast_size_sort",
    "fast_date_modified_sort",
    "fast_extension_sort",
];

impl IndexConfig {
    /// The option named `key` (Everything's spelling).
    pub fn get(&self, key: &str) -> Option<bool> {
        let mut copy = *self;
        copy.slot(key).map(|v| *v)
    }

    /// Every option as (Everything's key, value), in [`CONFIG_KEYS`] order.
    pub fn entries(&self) -> Vec<(&'static str, bool)> {
        CONFIG_KEYS
            .iter()
            .map(|&k| (k, self.get(k).unwrap_or(false)))
            .collect()
    }

    /// The option named `key`, to set it.
    pub fn slot(&mut self, key: &str) -> Option<&mut bool> {
        Some(match key {
            "index_folder_size" => &mut self.index_folder_size,
            "index_date_created" => &mut self.index_date_created,
            "index_date_accessed" => &mut self.index_date_accessed,
            "index_attributes" => &mut self.index_attributes,
            "index_recent_changes" => &mut self.index_recent_changes,
            "fast_size_sort" => &mut self.fast_size_sort,
            "fast_date_modified_sort" => &mut self.fast_date_modified_sort,
            "fast_date_created_sort" => &mut self.fast_date_created_sort,
            "fast_date_accessed_sort" => &mut self.fast_date_accessed_sort,
            "fast_attributes_sort" => &mut self.fast_attributes_sort,
            "fast_extension_sort" => &mut self.fast_extension_sort,
            _ => return None,
        })
    }

    /// `self` with `change` applied.
    #[must_use]
    pub fn with(mut self, change: &ConfigChange) -> Self {
        for key in CONFIG_KEYS {
            if let (Some(v), Some(slot)) = (change.get(key), self.slot(key)) {
                *slot = v;
            }
        }
        self
    }
}

/// A [`Request::SetConfig`]: the options to change; `None` leaves one as it
/// is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConfigChange {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_folder_size: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_date_created: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_date_accessed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_attributes: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_recent_changes: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fast_size_sort: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fast_date_modified_sort: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fast_date_created_sort: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fast_date_accessed_sort: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fast_attributes_sort: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fast_extension_sort: Option<bool>,
}

impl ConfigChange {
    /// The change to the option named `key`; `None` for none, or for a name
    /// that is not an option.
    pub fn get(&self, key: &str) -> Option<bool> {
        let mut copy = *self;
        copy.slot(key).and_then(|v| *v)
    }

    /// The change to the option named `key`, to set it; `None` for a name
    /// that is not an option.
    pub fn slot(&mut self, key: &str) -> Option<&mut Option<bool>> {
        Some(match key {
            "index_folder_size" => &mut self.index_folder_size,
            "index_date_created" => &mut self.index_date_created,
            "index_date_accessed" => &mut self.index_date_accessed,
            "index_attributes" => &mut self.index_attributes,
            "index_recent_changes" => &mut self.index_recent_changes,
            "fast_size_sort" => &mut self.fast_size_sort,
            "fast_date_modified_sort" => &mut self.fast_date_modified_sort,
            "fast_date_created_sort" => &mut self.fast_date_created_sort,
            "fast_date_accessed_sort" => &mut self.fast_date_accessed_sort,
            "fast_attributes_sort" => &mut self.fast_attributes_sort,
            "fast_extension_sort" => &mut self.fast_extension_sort,
            _ => return None,
        })
    }
}

/// Parameters of one search query.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
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
    /// Populate [`ResultItem::ctime_ft`] (`"columns"` in `Hello.features`;
    /// an older daemon ignores the flag). Read from disk for the page's
    /// rows while dates created are not indexed.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub want_ctime: bool,
    /// Populate [`ResultItem::atime_ft`], as `want_ctime`.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub want_atime: bool,
    /// Populate [`ResultItem::attrib`], as `want_ctime`.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub want_attrib: bool,
    /// Populate [`ResultItem::rtime_ft`] while recent changes are indexed.
    /// The disk keeps no such date, so it is `None` while they are not.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub want_rtime: bool,
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
        /// What the daemon supports right now (see [`feature`]); empty from
        /// a daemon predating the field.
        #[serde(default)]
        features: Vec<String>,
    },
    /// One page of query results.
    Results(QueryResults),
    /// Reply to [`Request::Status`].
    Status(DaemonStatus),
    /// Reply to [`Request::GetConfig`] and [`Request::SetConfig`]: the
    /// options as they are.
    Config(IndexConfig),
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
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ResultItem {
    /// Full path, WTF-8 lossy-decoded to UTF-8 (lone surrogates → U+FFFD).
    pub path: String,
    /// Exact UTF-16 code units of the path, present ONLY when [`Self::path`]
    /// was lossy (the name contained unpaired surrogates). Clients that need
    /// exact fidelity reconstruct the `OsString` from this.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub path_u16: Option<Vec<u16>>,
    /// File size in bytes; `None` = unknown, not requested, or a directory.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub size: Option<u64>,
    /// Raw FILETIME modification stamp; `None` = unknown or not requested.
    /// The client formats it (locale/timezone stays client-side).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub mtime_ft: Option<i64>,
    pub is_dir: bool,
    /// Raw FILETIME creation stamp, when [`QueryRequest::want_ctime`] asked
    /// for it; `None` when it could not be read.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ctime_ft: Option<i64>,
    /// Raw FILETIME last-access stamp, as `ctime_ft`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub atime_ft: Option<i64>,
    /// Raw FILETIME of the last change to the entry gozd saw in the journal
    /// (Everything's "date recently changed"), while recent changes are
    /// indexed.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub rtime_ft: Option<i64>,
    /// The Windows `FILE_ATTRIBUTE_*` bits, as `ctime_ft`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub attrib: Option<u32>,
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
    /// The optional columns switched on for this volume, and how far each
    /// one's background build has come. Empty while none is on, or from a
    /// daemon predating the field.
    #[serde(default)]
    pub columns: Vec<ColumnStatus>,
}

/// One optional column of a volume's index.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnStatus {
    /// Everything's option name: `index_date_created`, `index_date_accessed`,
    /// `index_attributes` or `index_recent_changes`.
    pub column: String,
    /// Every entry has its value: queries and pages read it from the index.
    pub settled: bool,
    /// While it builds: the files filled in so far, of `of`, the volume's
    /// entries.
    pub built: u64,
    pub of: u64,
    /// The column's bytes.
    pub bytes: u64,
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
            ..Default::default()
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
                ..Default::default()
            },
            ResultItem {
                path: r"C:\src".into(),
                path_u16: None,
                size: None,
                mtime_ft: None,
                is_dir: true,
                ..Default::default()
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
            ..Default::default()
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
            features: vec![feature::COLUMNS.into(), feature::CONFIG.into()],
        });
    }

    /// A Hello from a daemon predating `features` reads as none; a client
    /// predating it ignores the field (see `unknown_json_fields_are_ignored`).
    #[test]
    fn hello_without_features_reads_as_none() {
        let resp: Response =
            serde_json::from_str(r#"{"t":"Hello","proto":1,"server":"gozd","ready":true}"#)
                .expect("an older Hello");
        assert!(matches!(resp, Response::Hello { features, .. } if features.is_empty()));
    }

    /// The feature names are exactly the protocol's.
    #[test]
    fn feature_names_are_the_specs() {
        assert_eq!(
            [
                feature::WATCH,
                feature::SORT_DATE_CREATED,
                feature::SORT_DATE_ACCESSED,
                feature::SORT_ATTRIBUTES,
                feature::SORT_DATE_RECENTLY_CHANGED,
                feature::COLUMNS,
                feature::FOLDER_SIZES,
                feature::INDEXED_DATE_CREATED,
                feature::INDEXED_DATE_ACCESSED,
                feature::INDEXED_ATTRIBUTES,
                feature::INDEXED_RECENT_CHANGES,
                feature::CONFIG,
            ],
            [
                "watch",
                "sort:date_created",
                "sort:date_accessed",
                "sort:attributes",
                "sort:date_recently_changed",
                "columns",
                "folder_sizes",
                "indexed:date_created",
                "indexed:date_accessed",
                "indexed:attributes",
                "indexed:recent_changes",
                "config",
            ]
        );
    }

    /// GetConfig, SetConfig and Config round-trip; a SetConfig names only
    /// the options it changes, and the defaults are Everything's.
    #[test]
    fn config_requests_round_trip_with_everythings_names() {
        rt_request(&Request::GetConfig);
        let change = ConfigChange {
            index_attributes: Some(true),
            fast_size_sort: Some(false),
            ..ConfigChange::default()
        };
        rt_request(&Request::SetConfig(change));
        let json = serde_json::to_string(&Request::SetConfig(change)).expect("serialize");
        assert_eq!(
            json,
            r#"{"t":"SetConfig","index_attributes":true,"fast_size_sort":false}"#
        );
        let defaults = IndexConfig::default();
        rt_response(&Response::Config(defaults));
        let on: Vec<&str> = defaults
            .entries()
            .into_iter()
            .filter(|(_, v)| *v)
            .map(|(k, _)| k)
            .collect();
        assert_eq!(
            on,
            [
                "index_folder_size",
                "fast_size_sort",
                "fast_date_modified_sort"
            ]
        );
        let changed = defaults.with(&change);
        assert!(changed.index_attributes && !changed.fast_size_sort);
        assert_eq!(changed.with(&ConfigChange::default()), changed);
        for key in CONFIG_KEYS {
            let mut c = ConfigChange::default();
            *c.slot(key).expect("every key has a slot") = Some(!defaults.get(key).unwrap());
            assert_eq!(
                defaults.with(&c).get(key),
                Some(!defaults.get(key).unwrap())
            );
        }
        // A config file missing an option keeps Everything's default.
        let partial: IndexConfig =
            serde_json::from_str(r#"{"index_date_created":true}"#).expect("partial");
        assert!(partial.index_date_created && partial.index_folder_size);
    }

    /// A query from a client predating the columns carries no `want_*`,
    /// and one that asks for none sends none.
    #[test]
    fn column_flags_are_optional_both_ways() {
        let req: Request = serde_json::from_str(
            r#"{"t":"Query","query":"a","scope":null,"sort":{"key":"Name","dir":"Asc"},
                "offset":0,"limit":0,"want_size":true,"want_mtime":true,"match_case":false}"#,
        )
        .expect("an older query");
        assert!(matches!(req, Request::Query(q)
            if !q.want_ctime && !q.want_atime && !q.want_attrib && !q.want_rtime));
        let json = serde_json::to_string(&Request::Query(sample_query())).expect("serialize");
        for flag in ["want_ctime", "want_atime", "want_attrib", "want_rtime"] {
            assert!(!json.contains(flag), "json: {json}");
        }
        let mut q = sample_query();
        q.want_attrib = true;
        q.sort.key = SortKey::DateRecentlyChanged;
        rt_request(&Request::Query(q));
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
                columns: vec![ColumnStatus {
                    column: "index_attributes".into(),
                    settled: i % 2 == 0,
                    built: i as u64,
                    of: 9,
                    bytes: 18,
                }],
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
            ..Default::default()
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
            ..Default::default()
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
            }
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
