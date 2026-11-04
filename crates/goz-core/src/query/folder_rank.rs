//! Folder ranks for a path sort: each folder's position in the order of
//! folded full paths, found by walking the folder tree in name order instead
//! of building and sorting the paths.
//!
//! That order is not a plain walk of the tree. `a` sorts before `a b`, which
//! sorts before `a\x`, because a space sorts before `\`. So among the folders
//! of one parent each sorts by two keys: its folded name followed by an end
//! that sorts before every byte, for the folder itself, and its folded name
//! followed by `\`, for everything below it. A name holds no `\`, so no key
//! is the start of another, and walking the keys in order visits the folders
//! exactly as their paths compare. Siblings are put in name order by their
//! name ranks (see `NameStore::rank_key`), so the walk compares integers and
//! reads a name's bytes only to break a tie or to test a prefix.

use crate::index::volume::MAX_PATH_HOPS;
use crate::index::{NameId, VolumeIndex};
use crate::types::{EntryIdx, NIL};
use rustc_hash::FxHashMap;

/// The rank of a folder with no full path: one under lost+found, in a cycle,
/// or deeper than [`MAX_PATH_HOPS`]. It sorts after every real folder.
const UNPRESENTABLE: u32 = u32::MAX;

/// The parent of a node whose folder has none: the root, or lost+found.
const TOP: u32 = u32::MAX;

/// The root's node, made first.
const ROOT: u32 = 0;

/// The folders a path sort needs, gathered one at a time with
/// [`Self::node`] and ranked together by [`Self::ranks`].
///
/// Each folder gets a node, and so does every ancestor up to the root, since
/// the walk descends through them. Folders that share a folded path, such as
/// `A` and `a` in a case-sensitive folder, keep separate nodes; the walk
/// visits them as one group and gives them one rank.
pub(super) struct FolderRanks {
    node_of: FxHashMap<EntryIdx, u32>,
    /// Per node: the parent's node, or [`TOP`].
    parent: Vec<u32>,
    /// Per node: the folder's name.
    name: Vec<NameId>,
    /// The last folder asked for and its node: siblings often arrive in a row.
    last: (EntryIdx, u32),
}

/// One step of the walk over [`FolderRanks::ranks`]'s `members`.
enum Step {
    /// Give the group at `members[start..end]` the next rank.
    Folder { start: usize, end: usize },
    /// Walk the children of the group at `members[start..end]`, which is
    /// `depth` entries from the root, counting both ends.
    Below {
        start: usize,
        end: usize,
        depth: usize,
    },
}

impl FolderRanks {
    pub(super) fn new(index: &VolumeIndex) -> Self {
        let mut this = Self {
            node_of: FxHashMap::default(),
            parent: Vec::new(),
            name: Vec::new(),
            last: (NIL, TOP),
        };
        this.push(index, index.root());
        this
    }

    /// The node of folder `dir`, made along with any ancestor not seen yet.
    pub(super) fn node(&mut self, index: &VolumeIndex, dir: EntryIdx) -> u32 {
        if self.last.0 == dir {
            return self.last.1;
        }
        let node = match self.node_of.get(&dir) {
            Some(&n) => n,
            None => self.push_chain(index, dir),
        };
        self.last = (dir, node);
        node
    }

    /// Makes nodes for `dir` and its ancestors up to the first one already
    /// known, or the top of the chain. Every node is in `node_of` before the
    /// climb goes on, so a cycle ends where it closes.
    fn push_chain(&mut self, index: &VolumeIndex, dir: EntryIdx) -> u32 {
        let first = self.push(index, dir);
        let (mut child, mut cur) = (first, dir);
        loop {
            let up = index.entry(cur).parent();
            if up == NIL {
                break;
            }
            if let Some(&n) = self.node_of.get(&up) {
                self.parent[child as usize] = n;
                break;
            }
            let n = self.push(index, up);
            self.parent[child as usize] = n;
            (child, cur) = (n, up);
        }
        first
    }

    fn push(&mut self, index: &VolumeIndex, dir: EntryIdx) -> u32 {
        let n = self.parent.len() as u32;
        self.node_of.insert(dir, n);
        self.parent.push(TOP);
        self.name.push(index.name_id(dir));
        n
    }

    /// Every node's rank, indexed by node: ranks order the folders as their
    /// folded paths compare (the root, whose path is empty, first), folders
    /// with equal paths share one, and a folder with no full path gets
    /// [`UNPRESENTABLE`].
    ///
    /// The walk starts at the root, so the nodes under lost+found and in a
    /// cycle are never reached and keep [`UNPRESENTABLE`]; so do folders past
    /// the depth where `VolumeIndex::path_of` gives up.
    pub(super) fn ranks(self, index: &VolumeIndex) -> Vec<u32> {
        let Self {
            node_of,
            parent,
            name,
            ..
        } = self;
        drop(node_of);
        let n = parent.len();
        // Each node's children, stored together: those of node `p` are
        // `children[first[p]..first[p + 1]]`.
        let mut first = vec![0u32; n + 1];
        for &p in &parent {
            if p != TOP {
                first[p as usize + 1] += 1;
            }
        }
        for i in 1..=n {
            first[i] += first[i - 1];
        }
        let mut children = vec![0u32; first[n] as usize];
        let mut fill = first.clone();
        for (child, &p) in parent.iter().enumerate() {
            if p != TOP {
                children[fill[p as usize] as usize] = child as u32;
                fill[p as usize] += 1;
            }
        }
        drop((fill, parent));

        let key: Vec<u64> = name.iter().map(|&id| index.name_rank_key(id)).collect();
        let folded = |node: u32| index.name_folded(name[node as usize]);
        let mut rank = vec![UNPRESENTABLE; n];
        rank[ROOT as usize] = 0;
        let mut next_rank = 1u32;
        // Groups of nodes with equal folded paths, as ranges of `members`.
        let mut members: Vec<u32> = vec![ROOT];
        let mut steps = vec![Step::Below {
            start: 0,
            end: 1,
            depth: 1,
        }];
        let mut level: Vec<Step> = Vec::new();
        // Sibling groups whose `\` key is still to come, innermost last.
        let mut open: Vec<(usize, usize)> = Vec::new();
        while let Some(step) = steps.pop() {
            let (start, end, depth) = match step {
                Step::Folder { start, end } => {
                    for &m in &members[start..end] {
                        rank[m as usize] = next_rank;
                    }
                    next_rank += 1;
                    continue;
                }
                Step::Below { start, end, depth } => (start, end, depth),
            };
            // A child would be deeper than any full path.
            if depth >= MAX_PATH_HOPS {
                continue;
            }
            let from = members.len();
            for i in start..end {
                let m = members[i] as usize;
                members.extend_from_slice(&children[first[m] as usize..first[m + 1] as usize]);
            }
            members[from..].sort_unstable_by(|&a, &b| {
                key[a as usize]
                    .cmp(&key[b as usize])
                    .then_with(|| folded(a).cmp(folded(b)))
            });
            let mut i = from;
            while i < members.len() {
                let name = folded(members[i]);
                let mut j = i + 1;
                while j < members.len()
                    && key[members[j] as usize] == key[members[i] as usize]
                    && folded(members[j]) == name
                {
                    j += 1;
                }
                // An open group's `\` key comes before this name's keys
                // unless this name extends it with a byte below `\`.
                while let Some(&(s, e)) = open.last() {
                    if extends_below_separator(folded(members[s]), name) {
                        break;
                    }
                    open.pop();
                    level.push(Step::Below {
                        start: s,
                        end: e,
                        depth: depth + 1,
                    });
                }
                level.push(Step::Folder { start: i, end: j });
                open.push((i, j));
                i = j;
            }
            while let Some((s, e)) = open.pop() {
                level.push(Step::Below {
                    start: s,
                    end: e,
                    depth: depth + 1,
                });
            }
            steps.extend(level.drain(..).rev());
        }
        rank
    }
}

/// Whether `name` sorts before `prefix` followed by `\`: it starts with
/// `prefix` and goes on with a byte below `\`. Only a later sibling is asked,
/// so `name` already sorts after `prefix`.
fn extends_below_separator(prefix: &[u8], name: &[u8]) -> bool {
    name.len() > prefix.len() && name.starts_with(prefix) && name[prefix.len()] < b'\\'
}
