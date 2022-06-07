/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::io::Cursor;
use std::ops::Deref;
use std::path::Path;

use anyhow::Result;

use crate::filestate::FileStateV2;
use crate::filestate::StateFlags;
use crate::filestore::FileStore;
use crate::serialization::Serializable;
use crate::store::BlockId;
use crate::store::Store;
use crate::store::StoreView;
use crate::tree::AggregatedState;
use crate::tree::Key;
use crate::tree::KeyRef;
use crate::tree::Node;
use crate::tree::Tree;
use crate::tree::VisitorResult;

const FILTER_LOWERCASE: u64 = 0x1;
/// `TreeState` uses a single tree to track an extended state of `TreeDirstate`.
/// See the comment about `FileStateV2` for the difference.
/// In short, `TreeState` combines dirstate and fsmonitor state.
pub struct TreeState {
    store: FileStore,
    tree: Tree<FileStateV2>,
    root: TreeStateRoot,
}

/// `TreeStateRoot` contains block id to the root `Tree`, and other metadata.
#[derive(Default)]
pub(crate) struct TreeStateRoot {
    pub version: u32,
    pub file_count: u32,
    pub tree_block_id: BlockId,
    pub metadata: Box<[u8]>,
}

impl TreeState {
    /// Read `TreeState` from a file, or create an empty new `TreeState` if `root_id` is None.
    pub fn open<P: AsRef<Path>>(path: P, root_id: Option<BlockId>) -> Result<Self> {
        match root_id {
            Some(root_id) => {
                let store = FileStore::open(path)?;
                let root = {
                    let mut root_buf = Cursor::new(store.read(root_id)?);
                    TreeStateRoot::deserialize(&mut root_buf)?
                };
                let tree = Tree::open(root.tree_block_id, root.file_count);
                Ok(TreeState { store, tree, root })
            }
            None => {
                let store = FileStore::create(path)?;
                let root = TreeStateRoot::default();
                let tree = Tree::new();
                Ok(TreeState { store, tree, root })
            }
        }
    }

    pub fn path(&self) -> &Path {
        self.store.path()
    }

    /// Flush dirty entries. Return new `root_id` that can be passed to `open`.
    pub fn flush(&mut self) -> Result<BlockId> {
        let tree_block_id = { self.tree.write_delta(&mut self.store)? };
        self.write_root(tree_block_id)
    }

    /// Save as a new file.
    pub fn write_as<P: AsRef<Path>>(&mut self, path: P) -> Result<BlockId> {
        let mut new_store = FileStore::create(path)?;
        let tree_block_id = self.tree.write_full(&mut new_store, &self.store)?;
        self.store = new_store;
        let root_id = self.write_root(tree_block_id)?;
        Ok(root_id)
    }

    fn write_root(&mut self, tree_block_id: BlockId) -> Result<BlockId> {
        self.root.tree_block_id = tree_block_id;
        self.root.file_count = self.len() as u32;

        let mut root_buf = Vec::new();
        self.root.serialize(&mut root_buf)?;
        let result = self.store.append(&root_buf)?;
        self.store.flush()?;
        Ok(result)
    }

    /// Create or replace the existing entry.
    pub fn insert<K: AsRef<[u8]>>(&mut self, path: K, state: &FileStateV2) -> Result<()> {
        self.tree.add(&self.store, path.as_ref(), state)
    }

    pub fn remove<K: AsRef<[u8]>>(&mut self, path: K) -> Result<bool> {
        self.tree.remove(&self.store, path.as_ref())
    }

    pub fn get<K: AsRef<[u8]>>(&mut self, path: K) -> Result<Option<&FileStateV2>> {
        self.tree.get(&self.store, path.as_ref())
    }

    pub fn get_keys_ignorecase<K: AsRef<[u8]>>(&mut self, path: K) -> Result<Vec<Key>> {
        fn map_lowercase(k: KeyRef) -> Result<Key> {
            let s = std::str::from_utf8(k);
            Ok(if let Ok(s) = s {
                s.to_lowercase().into_bytes().into_boxed_slice()
            } else {
                k.to_vec().into_boxed_slice()
            })
        }
        self.get_filtered_key(
            &map_lowercase(path.as_ref())?,
            &mut map_lowercase,
            FILTER_LOWERCASE,
        )
    }

    /// Get the aggregated state of a directory. This is useful, for example, to tell if a
    /// directory only contains removed files.
    pub fn get_dir<K: AsRef<[u8]>>(&mut self, path: K) -> Result<Option<AggregatedState>> {
        self.tree.get_dir(&self.store, path.as_ref())
    }

    pub fn len(&self) -> usize {
        self.tree.file_count() as usize
    }

    pub fn set_metadata<T: AsRef<[u8]>>(&mut self, metadata: T) {
        self.root.metadata = Vec::from(metadata.as_ref()).into_boxed_slice();
    }

    pub fn get_metadata(&self) -> &[u8] {
        self.root.metadata.deref()
    }

    pub fn has_dir<P: AsRef<[u8]>>(&mut self, path: P) -> Result<bool> {
        self.tree.has_dir(&self.store, path.as_ref())
    }

    pub fn visit<F, VD, VF>(
        &mut self,
        visitor: &mut F,
        visit_dir: &VD,
        visit_file: &VF,
    ) -> Result<()>
    where
        F: FnMut(&Vec<&[u8]>, &mut FileStateV2) -> Result<VisitorResult>,
        VD: Fn(&Vec<KeyRef>, &Node<FileStateV2>) -> bool,
        VF: Fn(&Vec<KeyRef>, &FileStateV2) -> bool,
    {
        self.tree
            .visit_advanced(&self.store, visitor, visit_dir, visit_file)
    }

    pub fn visit_by_state(&mut self, state_required_any: StateFlags) -> Result<Vec<Vec<u8>>> {
        let mut result = Vec::new();
        self.visit(
            &mut |path_components, _| {
                result.push(path_components.concat());
                Ok(VisitorResult::NotChanged)
            },
            &|_, dir| match dir.get_aggregated_state() {
                None => true,
                Some(aggregated_state) => aggregated_state.union.intersects(state_required_any),
            },
            &|_, file| file.state.intersects(state_required_any),
        )?;

        Ok(result)
    }

    pub fn get_filtered_key<F>(
        &mut self,
        name: KeyRef,
        filter: &mut F,
        filter_id: u64,
    ) -> Result<Vec<Key>>
    where
        F: FnMut(KeyRef) -> Result<Key>,
    {
        self.tree
            .get_filtered_key(&self.store, name, filter, filter_id)
    }

    pub fn path_complete<FA, FV>(
        &mut self,
        prefix: KeyRef,
        full_paths: bool,
        acceptable: &FA,
        visitor: &mut FV,
    ) -> Result<()>
    where
        FA: Fn(&FileStateV2) -> bool,
        FV: FnMut(&Vec<KeyRef>) -> Result<()>,
    {
        self.tree
            .path_complete(&self.store, prefix, full_paths, acceptable, visitor)
    }

    // Distrust changed files with a mtime of `fsnow`. Rewrite their mtime to -1.
    // See mercurial/pure/parsers.py:pack_dirstate in core Mercurial for motivation.
    // Basically, this is required for the following case:
    //
    // $ hg update rev; write foo; hg commit -m update-foo
    //
    //   Time (second) | 0   | 1           |
    //   hg update       ...----|
    //   write foo               |--|
    //   hg commit                   |---...
    //
    // If "write foo" changes a file without changing its mtime and size, the file
    // change won't be detected. Therefore if mtime is `fsnow`, reset it to a different
    // value and mark it as NEED_CHECK, at the end of update to workaround the issue.
    // Here, hg assumes nobody else is touching the working copy when it holds wlock
    // (ex. during second 0).
    //
    // This is used before "flush" or "saveas".
    //
    // Note: In TreeState's case, NEED_CHECK might mean "perform a quick mtime check",
    // or "perform a content check" depending on the caller. Be careful when removing
    // "mtime = -1" statement.
    pub fn invalidate_mtime(&mut self, now: i32) -> Result<()> {
        self.visit(
            &mut |_, state| {
                if state.mtime >= now {
                    state.mtime = -1;
                    state.state |= StateFlags::NEED_CHECK;
                    Ok(VisitorResult::Changed)
                } else {
                    Ok(VisitorResult::NotChanged)
                }
            },
            &|_, dir| {
                if !dir.is_changed() {
                    false
                } else {
                    match dir.get_aggregated_state() {
                        Some(x) => x
                            .union
                            .intersects(StateFlags::EXIST_P1 | StateFlags::EXIST_P2),
                        None => true,
                    }
                }
            },
            &|_, file| {
                file.state
                    .intersects(StateFlags::EXIST_P1 | StateFlags::EXIST_P2)
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use rand::Rng;
    use rand::SeedableRng;
    use rand_chacha::ChaChaRng;
    use tempdir::TempDir;

    use super::*;
    use crate::filestate::StateFlags;

    #[test]
    fn test_new() {
        let dir = TempDir::new("treestate").expect("tempdir");
        let state = TreeState::open(dir.path().join("empty"), None).expect("open");
        assert!(state.get_metadata().is_empty());
        assert_eq!(state.len(), 0);
    }

    #[test]
    fn test_empty_flush() {
        let dir = TempDir::new("treestate").expect("tempdir");
        let mut state = TreeState::open(dir.path().join("empty"), None).expect("open");
        let block_id = state.flush().expect("flush");
        let state = TreeState::open(dir.path().join("empty"), block_id.into()).expect("open");
        assert!(state.get_metadata().is_empty());
        assert_eq!(state.len(), 0);
    }

    #[test]
    fn test_empty_write_as() {
        let dir = TempDir::new("treestate").expect("tempdir");
        let mut state = TreeState::open(dir.path().join("empty"), None).expect("open");
        let block_id = state.write_as(dir.path().join("as")).expect("write_as");
        let state = TreeState::open(dir.path().join("as"), block_id.into()).expect("open");
        assert!(state.get_metadata().is_empty());
        assert_eq!(state.len(), 0);
    }

    #[test]
    fn test_set_metadata() {
        let dir = TempDir::new("treestate").expect("tempdir");
        let mut state = TreeState::open(dir.path().join("1"), None).expect("open");
        state.set_metadata(b"foobar");
        let block_id1 = state.flush().expect("flush");
        let block_id2 = state.write_as(dir.path().join("2")).expect("write_as");
        let state = TreeState::open(dir.path().join("1"), block_id1.into()).expect("open");
        assert_eq!(state.get_metadata()[..], b"foobar"[..]);
        let state = TreeState::open(dir.path().join("2"), block_id2.into()).expect("open");
        assert_eq!(state.get_metadata()[..], b"foobar"[..]);
    }

    // Some random paths extracted from fb-hgext, plus some manually added entries, shuffled.
    const SAMPLE_PATHS: [&[u8]; 21] = [
        b".fbarcanist",
        b"phabricator/phabricator_graphql_client_urllib.pyc",
        b"hgext3rd/__init__.py",
        b"hgext3rd/.git/objects/14/8f179e7e702ddedb54c53f2726e7f81b14a33f",
        b"rust/radixbuf/.git/objects/pack/pack-c0bc37a255e59f5563de9a76013303d8df46a659.idx",
        b".hg/shelved/default-106.patch",
        b"rust/radixbuf/.git/objects/20/94e0274ba1ef2ec30de884e3ca4d7093838064",
        b"rust/radixbuf/.git/hooks/prepare-commit-msg.sample",
        b"rust/radixbuf/.git/objects/b3/9acb828290b77704cc44e748d6e7d4a528d6ae",
        b"scripts/lint.py",
        b".fbarcanist/unit/MercurialTestEngine.php",
        b".hg/shelved/default-37.patch",
        b"rust/radixbuf/.git/objects/01/d8e75b3bae0819c4095ae96ebdc889e9e5d806",
        b"hgext3rd/fastannotate/error.py",
        b"rust/radixbuf/.git/objects/pack/pack-c0bc37a255e59f5563de9a76013303d8df46a659.pack",
        b"distutils_rust/__init__.py",
        b".editorconfig",
        b"rust/radixbuf/.git/objects/01/89a583d7e9aff802cdfed3ff3cc3a473253281",
        b"hgext3rd/fastannotate/commands.py",
        b"distutils_rust/__init__.pyc",
        b"rust/radixbuf/.git/objects/b3/9b2824f47b66462e92ffa4f978bc95f5fdad2e",
    ];

    fn new_treestate<P: AsRef<Path>>(path: P) -> TreeState {
        let mut state = TreeState::open(path, None).expect("open");
        let mut rng = ChaChaRng::from_seed([0; 32]);
        for path in &SAMPLE_PATHS {
            let file = rng.gen();
            state.insert(path, &file).expect("insert");
        }
        state
    }

    #[test]
    fn test_insert() {
        let dir = TempDir::new("treestate").expect("tempdir");
        let mut state = new_treestate(dir.path().join("1"));
        let mut rng = ChaChaRng::from_seed([0; 32]);
        for path in &SAMPLE_PATHS {
            let file: FileStateV2 = rng.gen();
            assert_eq!(state.get(path).unwrap().unwrap(), &file);
        }
        assert_eq!(state.len(), SAMPLE_PATHS.len());
    }

    #[test]
    fn test_remove() {
        let dir = TempDir::new("treestate").expect("tempdir");
        let mut state = new_treestate(dir.path().join("1"));
        for path in &SAMPLE_PATHS {
            assert!(state.remove(path).unwrap())
        }
        for path in &SAMPLE_PATHS {
            assert!(!state.remove(path).unwrap())
        }
        assert_eq!(state.len(), 0);
    }

    #[test]
    fn test_non_empty_flush() {
        let dir = TempDir::new("treestate").expect("tempdir");
        let mut state = new_treestate(dir.path().join("1"));
        let block_id = state.flush().expect("flush");
        let mut state = TreeState::open(dir.path().join("1"), block_id.into()).expect("open");
        let mut rng = ChaChaRng::from_seed([0; 32]);
        for path in &SAMPLE_PATHS {
            let file: FileStateV2 = rng.gen();
            assert_eq!(state.get(path).unwrap().unwrap(), &file);
        }
        assert_eq!(state.len(), SAMPLE_PATHS.len());
    }

    #[test]
    fn test_non_empty_write_as() {
        let dir = TempDir::new("treestate").expect("tempdir");
        let mut state = new_treestate(dir.path().join("1"));
        let block_id = state.write_as(dir.path().join("as")).expect("write_as");
        let mut state = TreeState::open(dir.path().join("as"), block_id.into()).expect("open");
        let mut rng = ChaChaRng::from_seed([0; 32]);
        for path in &SAMPLE_PATHS {
            let file: FileStateV2 = rng.gen();
            assert_eq!(state.get(path).unwrap().unwrap(), &file);
        }
        assert_eq!(state.len(), SAMPLE_PATHS.len());
    }

    #[test]
    fn test_has_dir() {
        let dir = TempDir::new("treestate").expect("tempdir");
        let mut state = new_treestate(dir.path().join("1"));
        assert!(state.has_dir(b"/").unwrap());
        assert!(state.has_dir(b"hgext3rd/").unwrap());
        assert!(!state.has_dir(b"hgext4th/").unwrap());
        assert!(state.has_dir(b"rust/radixbuf/.git/objects/").unwrap());
        assert!(!state.has_dir(b"rust/radixbuf/.git2/objects/").unwrap());
    }

    #[test]
    fn test_visit_query_by_flags() {
        let dir = TempDir::new("treestate").expect("tempdir");
        let mut state = TreeState::open(dir.path().join("1"), None).expect("open");
        let mut rng = ChaChaRng::from_seed([0; 32]);
        let mut file: FileStateV2 = rng.gen();
        file.state = StateFlags::IGNORED | StateFlags::NEED_CHECK;
        state.insert(b"a/b/1", &file).expect("insert");
        file.state = StateFlags::IGNORED | StateFlags::EXIST_P2;
        state.insert(b"a/b/2", &file).expect("insert");
        file.state = StateFlags::COPIED | StateFlags::EXIST_P2;
        state.insert(b"a/c/3", &file).expect("insert");

        let files = state.visit_by_state(StateFlags::IGNORED).unwrap();
        assert_eq!(files, vec![b"a/b/1", b"a/b/2"]);

        let files = state.visit_by_state(StateFlags::EXIST_P2).unwrap();
        assert_eq!(files, vec![b"a/b/2", b"a/c/3"]);
    }

    #[test]
    fn test_visit_state_change_propagation() {
        let paths: [&[u8]; 5] = [b"a/b/1", b"a/b/2", b"a/c/d/3", b"b/5", b"c"];

        // Only care about 1 bit (IGNORED), since other bits will propagate to parent trees in a
        // same way.
        //
        // Enumerate transition from all possible start states to end states. Make sure `visit`
        // querying that bit would return the expected result.
        //
        // 2 states for each file - IGNORED is set, or not set. With 5 files, that's (1 << 5 = 32)
        // start states, and 32 end states. 32 ** 2 = 1024 transitions to test.
        let bit = StateFlags::IGNORED;
        for start_bits in 0..(1 << paths.len()) {
            let dir = TempDir::new("treestate").expect("tempdir");
            // First, write the start state.
            let mut state = TreeState::open(dir.path().join("1"), None).expect("open");
            let mut rng = ChaChaRng::from_seed([0; 32]);
            for (i, path) in paths.iter().enumerate() {
                let mut file: FileStateV2 = rng.gen();
                if ((1 << i) & start_bits) == 0 {
                    file.state -= bit;
                } else {
                    file.state |= bit;
                }
                state.insert(path, &file).expect("insert");
            }
            let block_id = state.flush().expect("flush");

            // Then test end states.
            for end_bits in 0..(1 << paths.len()) {
                let mut state =
                    TreeState::open(dir.path().join("1"), Some(block_id)).expect("open");
                let mut i: usize = 0;
                let mut expected = vec![];
                state
                    .visit(
                        &mut |ref _path, ref mut file| {
                            let old_state = file.state;
                            if ((1 << i) & end_bits) == 0 {
                                file.state -= bit;
                            } else {
                                file.state |= bit;
                                expected.push(paths[i]);
                            }
                            i += 1;
                            if old_state == file.state {
                                Ok(VisitorResult::NotChanged)
                            } else {
                                Ok(VisitorResult::Changed)
                            }
                        },
                        &|_, _| true,
                        &|_, _| true,
                    )
                    .expect("visit");
                let files = state.visit_by_state(bit).unwrap();
                assert_eq!(files, expected);
            }
        }
    }

    #[test]
    fn test_get_keys_ignorecase() {
        let dir = TempDir::new("treestate").expect("tempdir");
        let mut state = new_treestate(dir.path().join("1"));
        let expected = vec![b"hgext3rd/__init__.py".to_vec().into_boxed_slice()];
        assert_eq!(
            state.get_keys_ignorecase(b"hgext3rd/__init__.py").unwrap(),
            expected
        );
        assert_eq!(
            state.get_keys_ignorecase(b"hGext3rd/__init__.py").unwrap(),
            expected
        );
        assert_eq!(
            state.get_keys_ignorecase(b"hgext3rd/__Init__.py").unwrap(),
            expected
        );
    }
}
