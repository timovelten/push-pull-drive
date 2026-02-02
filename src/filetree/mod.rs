use std::ops::Index;

use crate::{file_repository::FileRepository, paths::RPath, PathInfo};
use anyhow::{bail, ensure};
use merge::{Merge, Side};

mod merge;

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Type {
    Dir,
    File,
}

pub trait Node: core::fmt::Debug {
    fn path(&self) -> &RPath;

    /// Indicates whether this node is allowed to have child nodes. That is,
    /// if is_terminal is false, then this node will never any children.
    ///
    /// This is also taken into consideration when diffing the two trees.
    fn is_terminal(&self) -> bool {
        false
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileTree<T> {
    /// This list must not be modified, and the only way to construct it, is to use try from below.
    ///
    /// This ensures that the list is sorted properly, and that paths cannot appear twice.
    nodes: Vec<T>,

    /// How many nodes do we need to skip to get to the next sibling of this node?
    /// 0 indicates that this node has no siblings.
    ///
    /// This is useful when we want to iterate over the children of a node.
    sibling_offset: Vec<usize>,
}

#[derive(Debug, Clone)]
enum EmitSibling {
    Nothing,
    First(usize),
    Of(usize),
}

struct SiblingIndicesIterator<'a> {
    state: EmitSibling,
    sibling_offset: &'a [usize],
}

impl Iterator for SiblingIndicesIterator<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        match self.state {
            EmitSibling::First(first) => {
                self.state = EmitSibling::Of(first);
                Some(first)
            }
            EmitSibling::Of(i) => {
                let offset = self.sibling_offset[i];
                if offset > 0 {
                    self.state = EmitSibling::Of(i + offset);
                    Some(i + offset)
                } else {
                    self.state = EmitSibling::Nothing;
                    None
                }
            }
            EmitSibling::Nothing => None,
        }
    }
}

impl<T: Node> FileTree<T> {
    pub fn empty() -> Self {
        FileTree {
            nodes: Default::default(),
            sibling_offset: Default::default(),
        }
    }

    fn get_index(&self, path: &RPath) -> Result<usize, usize> {
        self.nodes.binary_search_by_key(&path, |node| node.path())
    }

    pub fn get<'a>(&'a self, path: &RPath) -> Option<&'a T> {
        self.get_index(path)
            .ok()
            .and_then(|idx| self.nodes.get(idx))
    }

    pub fn children<'a>(&'a self, path: &'a RPath) -> Option<impl Iterator<Item = &'a T> + 'a> {
        let idx = self.get_index(path).ok()?;

        // If this node has children at all, the first child will come immediatly after it
        let state = if idx + 1 < self.nodes.len() && self.nodes[idx + 1].path().starts_with(path) {
            EmitSibling::First(idx + 1)
        } else {
            EmitSibling::Nothing
        };
        let siblings = SiblingIndicesIterator {
            state,
            sibling_offset: &self.sibling_offset,
        };

        Some(siblings.map(|i| &self.nodes[i]))
    }

    pub fn all_children<'a>(&'a self, path: &'a RPath) -> Option<impl Iterator<Item = &'a T> + 'a> {
        let idx = self.get_index(path).ok()?;
        let ret = self.nodes[idx..]
            .iter()
            .take_while(|node| node.path().starts_with(path));

        Some(ret)
    }

    pub fn root(&self) -> Option<&RPath> {
        self.nodes.first().map(|x| x.path())
    }

    pub fn as_slice(&self) -> &[T] {
        &self.nodes
    }

    pub fn into_vec(self) -> Vec<T> {
        self.nodes
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn reroot(mut self, new_root: &RPath) -> Result<Self, Self> {
        let idx = match self.get_index(new_root) {
            Ok(idx) => idx,
            Err(_) => return Err(self),
        };

        let offset = self.sibling_offset[idx];
        let end = if offset > 0 {
            idx + offset
        } else {
            self.nodes.len()
        };

        self.nodes.truncate(end);
        let _ = self.nodes.drain(0..idx);

        self.sibling_offset[idx] = 0;
        self.sibling_offset.truncate(end);
        let _ = self.sibling_offset.drain(0..idx);

        Ok(self)
    }
}

impl<T: Node> Index<&RPath> for &FileTree<T> {
    type Output = T;

    fn index(&self, path: &RPath) -> &Self::Output {
        self.get(path).unwrap()
    }
}

impl<T: Node> TryFrom<Vec<T>> for FileTree<T> {
    type Error = anyhow::Error;

    fn try_from(mut nodes: Vec<T>) -> Result<Self, Self::Error> {
        nodes.sort_by(|a, b| a.path().cmp(b.path()));

        if nodes.is_empty() {
            return Ok(Self::empty());
        }

        // Ensure the paths of the nodes are unique. Also make sure that all files are leafs (and have no children)
        // and that the tree is connected.
        let mut sibling_offset = vec![0; nodes.len()];
        let root = nodes[0].path();

        // This contains the (unique) path up to the root node at all times
        let mut path: Vec<usize> = Vec::new();
        for i in 0..nodes.len() {
            // Is this path equal to the next path? (This is sufficient to check if there
            // are any duplicates because the list is sorted)
            if i + 1 < nodes.len() && nodes[i].path() == nodes[i + 1].path() {
                bail!(
                    "paths must be unique but the path {:?} appears twice",
                    nodes[i].path()
                )
            }

            // Is this a terminal node that has children? If so, we do not like that.
            if i + 1 < nodes.len()
                && nodes[i].is_terminal()
                && nodes[i + 1].path().starts_with(nodes[i].path())
            {
                bail!(
                    "the node {:?} has children but is terminal and therefore may not have children",
                    nodes[i].path()
                )
            }

            if !nodes[i].path().starts_with(root) {
                bail!(
                    "the node {:?} is not a child of the root node {:?}",
                    nodes[i].path(),
                    root
                )
            }

            // At this point, path is still the unique path to the nodes we peviously visited.
            // But we can use that to find our sibling (if we have one) and also our parent.
            let current_depth = nodes[i].path().len() - nodes[0].path().len();

            if let Some(&j) = path.get(current_depth) {
                debug_assert!(i > j, "Sibling is a different node");
                sibling_offset[j] = i - j;
            }

            if current_depth > 0 {
                // This node needs to have a parent
                let has_parent = path
                    .get(current_depth - 1)
                    .map(|&j| nodes[i].path().starts_with(nodes[j].path()))
                    .unwrap_or(false);

                ensure!(
                    has_parent,
                    "tree is not connected, the node {:?} has no parent",
                    nodes[i].path()
                );

                // We have found an edge! Note that if this for loop runs through once,
                // then we will have been here exactly n - 1 times, which will ensure that
                // tree is connected by maths
            }

            // Only now, actually update the path to the root (the order is a bit weird, I know)
            path.truncate(current_depth);
            path.insert(current_depth, i);

            // Now: path is the unique path from nodes[i] to the root node, and path.len() == current_depth + 1.
        }

        Ok(FileTree {
            nodes,
            sibling_offset,
        })
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Debug, Clone, Copy)]
pub struct Patch<'a> {
    pub path: &'a RPath,
    pub kind: PatchKind,
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Debug, Clone, Copy)]
pub enum PatchKind {
    /// A file is present in both trees at the path
    PresentFile,
    /// A directory is present in both trees at the path
    PresentDirectory,
    /// The left tree had a file, and the path does not exist in the right tree
    DeleteFile,
    /// The left tree had a directory (possibly with children), and the path did not exist in the right tree
    DeleteDirectory,
    /// The left tree has a directory, and the path does not exist in the right tree
    CreateDirectory,
    /// The left tree has a file and the path does not exist in the right tree
    CreateFile,
    /// The left tree has a directory and the right tree has a file at that
    ReplaceDirectoryByFile,
    /// The left tree has a file and the right tree has a directory there
    ReplaceFileByDirectory,
}

impl Node for Patch<'_> {
    fn path(&self) -> &RPath {
        self.path
    }

    /// Note that this is not really semantic; this is implemented to satisfy the trait requirements
    /// for filetree. We could just as well return false here and pretend that every node can have children.
    /// It is convenient to to make this reflect reality though, because the builer can
    /// then - at least sometimes - report if we violate our own assumptions.
    fn is_terminal(&self) -> bool {
        // Note that this is not
        match self.kind {
            // Note that a directory deletion is not supposed to have children in the patch tree (we explicitly
            // do not want every single file deletion to be there as a child)
            PatchKind::DeleteDirectory | PatchKind::ReplaceDirectoryByFile => true,
            PatchKind::CreateFile | PatchKind::DeleteFile | PatchKind::PresentFile => true,
            PatchKind::PresentDirectory
            | PatchKind::CreateDirectory
            | PatchKind::ReplaceFileByDirectory => false,
        }
    }
}

/// What operations do we have to do to the left tree so that it equals the right tree?
///
/// Nodes that appear in both trees are output as [`Patch::Update`], regardless of whether
/// they were actually updated or not (there is no way for this function to determine that).
pub fn diff<'a, A: Node, B: Node>(
    left: &'a FileTree<A>,
    right: &'a FileTree<B>,
) -> FileTree<Patch<'a>> {
    let mut patches = Vec::new();

    let merged = Merge::new(left.nodes.iter(), right.nodes.iter(), |x, y| {
        x.path().cmp(y.path())
    });

    let mut skip_over: Option<&RPath> = None;
    for side in merged {
        let path = match side {
            Side::Left(x) => x.path(),
            Side::Right(y) => y.path(),
            Side::Both { left, .. } => {
                // Note that both left and right compare equal according to the above
                // cmp function. Therefore, left and right have the same path
                // (unless Node has a very pathological path() impl that does not return the same
                // value when called again)
                left.path()
            }
        };

        // Maybe skip over some paths
        match skip_over {
            Some(skip_over) if path.starts_with(skip_over) => continue,
            Some(_) => {
                skip_over = None;
            }
            None => {}
        }

        let kind = match side {
            Side::Left(x) => {
                // Skip over all children of this node (we do not care about the children that
                // may have also been deleted)
                skip_over = Some(path);

                if x.is_terminal() {
                    PatchKind::DeleteFile
                } else {
                    PatchKind::DeleteDirectory
                }
            }
            Side::Right(y) => {
                if y.is_terminal() {
                    PatchKind::CreateFile
                } else {
                    PatchKind::CreateDirectory
                }
            }
            Side::Both { left, right } => match (left.is_terminal(), right.is_terminal()) {
                (true, true) => PatchKind::PresentFile,
                (false, false) => PatchKind::PresentDirectory,
                (false, true) => {
                    skip_over = Some(path);

                    PatchKind::ReplaceDirectoryByFile
                }
                (true, false) => PatchKind::ReplaceFileByDirectory,
            },
        };

        patches.push(Patch { path, kind });
    }

    patches.try_into().unwrap()
}

pub async fn file_tree_of_repository<A: FileRepository>(
    repo: &A,
    path: &RPath,
) -> anyhow::Result<FileTree<PathInfo>> {
    // Start out by walking the components of the path, and checking which component is
    // the first to not exist
    let mut ret = Vec::new();
    let mut done = false;
    for path in path.subpaths() {
        match repo.maybe_metadata(path).await? {
            Some(info @ PathInfo::Dir(_)) => ret.push(info),
            Some(info @ PathInfo::File(_)) => {
                ret.push(info);
                done = true;
            }
            None => {
                done = true;
            }
        }
    }

    if done {
        return Ok(ret.try_into().unwrap());
    }

    // Last element will be added in a second. Remove it here to avoid duplicates.
    let _ = ret.pop();

    // Now actually read the directory (we already know it exists)
    let files = repo.list_files(path, usize::MAX).await?;
    ret.extend(files);

    Ok(ret.try_into().expect("is a tree"))
}

#[cfg(test)]
mod test {
    use anyhow::anyhow;
    use futures::io::Cursor;
    use time::OffsetDateTime;

    use crate::{
        file_repository::{AsyncReader, Error},
        filetree::{diff, Patch},
        paths::{RPath, RPathBuf},
        FileInfo,
    };

    use super::*;

    #[derive(Debug)]
    enum Nd {
        File(RPathBuf),
        Dir(RPathBuf),
    }

    impl Node for Nd {
        fn path(&self) -> &crate::paths::RPath {
            match self {
                Self::Dir(path) => path,
                Self::File(path) => path,
            }
        }

        fn is_terminal(&self) -> bool {
            match self {
                Self::Dir(_) => false,
                Self::File(_) => true,
            }
        }
    }

    fn filetree(nodes: &[&'static str]) -> anyhow::Result<FileTree<Nd>> {
        let mut tree = Vec::new();
        for node in nodes {
            let nd = if let Some(folder) = node.strip_suffix('/') {
                Nd::Dir(RPathBuf::from(folder))
            } else {
                Nd::File(RPathBuf::from(*node))
            };

            tree.push(nd);
        }

        tree.try_into()
    }

    fn patchtree(nodes: &[(&'static str, PatchKind)]) -> FileTree<Patch<'static>> {
        let mut tree = Vec::new();
        for &(path, kind) in nodes {
            tree.push(Patch {
                path: path.into(),
                kind,
            });
        }

        tree.try_into().unwrap()
    }

    #[test]
    fn tree_must_be_connected_1() {
        let err = filetree(&[
            "/",
            "/dir/",
            "/dir/file",
            "/dir/file2",
            "/dir2/file1",
            "/dir2/file2",
        ])
        .unwrap_err();

        let msg = err.to_string();
        assert!(err.to_string().contains("connected"), "Wrong error: {msg}");
    }

    #[test]
    fn tree_must_be_connected_2() {
        let err = filetree(&[
            "/",
            "/dir/",
            "/dir/file",
            "/dir/file2",
            "/dir2/a/b/c/file1",
            "/dir2/a/b/c/file2",
        ])
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("connected"), "Wrong error: {msg}");
    }

    #[test]
    fn tree_must_be_rooted() {
        let err = filetree(&["/dir/", "/dir/file", "/dir/file2", "/dir2/"]).unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("root"), "Wrong error: {msg}");
    }

    #[test]
    fn patch_tree_present_in_both() {
        let left = filetree(&["/", "some document", "abc/", "abc/x"]).unwrap();
        let patches = diff(&left, &left);
        let expected = patchtree(&[
            ("", PatchKind::PresentDirectory),
            ("some document", PatchKind::PresentFile),
            ("abc", PatchKind::PresentDirectory),
            ("abc/x", PatchKind::PresentFile),
        ]);
        assert_eq!(patches, expected);
    }

    #[test]
    fn patch_tree_delete_file() {
        let left = filetree(&["/", "abc"]).unwrap();
        let right = filetree(&["/"]).unwrap();
        let patches = diff(&left, &right);
        let expected = patchtree(&[
            ("", PatchKind::PresentDirectory),
            ("abc", PatchKind::DeleteFile),
        ]);
        assert_eq!(patches, expected);
    }

    #[test]
    fn patch_tree_delete_directory() {
        let left = filetree(&["/", "abc/", "abc/f1", "abc/f2"]).unwrap();
        let right = filetree(&["/"]).unwrap();
        let patches = diff(&left, &right);
        let expected = patchtree(&[
            ("", PatchKind::PresentDirectory),
            ("abc", PatchKind::DeleteDirectory),
            // We do not expect the deleted files to appear here (we only record that the parent was deleted!)
        ]);
        assert_eq!(patches, expected);
    }

    #[test]
    fn patch_tree_create_file() {
        let left = filetree(&["/"]).unwrap();
        let right = filetree(&["/", "abc"]).unwrap();
        let patches = diff(&left, &right);
        let expected = patchtree(&[
            ("", PatchKind::PresentDirectory),
            ("abc", PatchKind::CreateFile),
        ]);

        assert_eq!(patches, expected);
    }

    #[test]
    fn patch_tree_create_directory() {
        let left = filetree(&["/"]).unwrap();
        let right = filetree(&["/", "abc/"]).unwrap();
        let patches = diff(&left, &right);
        let expected = patchtree(&[
            ("", PatchKind::PresentDirectory),
            ("abc", PatchKind::CreateDirectory),
        ]);

        assert_eq!(patches, expected);
    }

    #[test]
    fn patch_tree_replace_directory_by_file() {
        let left = filetree(&["/", "abc/", "abc/f1", "abc/f2"]).unwrap();
        let right = filetree(&["/", "abc"]).unwrap();
        let patches = diff(&left, &right);
        let expected = patchtree(&[
            ("", PatchKind::PresentDirectory),
            ("abc", PatchKind::ReplaceDirectoryByFile),
            // We do not expect the deleted files to appear here (we only record that the parent was deleted!)
        ]);
        assert_eq!(patches, expected);
    }

    fn dummy_tree() -> FileTree<Nd> {
        filetree(&[
            "a/",
            "a/2011/",
            "a/2012/",
            "a/2012/invoices/",
            "a/2012/invoices/invoice 1",
            "a/2012/invoices/invoice 2",
            "a/2012/misc/",
            "a/2012/misc/1",
            "a/2013/",
            "a/2013/file 1",
            "a/2013/file 2",
            "a/2014/",
        ])
        .unwrap()
    }

    #[test]
    fn tree_works() {
        let tree = dummy_tree();

        let expected_root: &RPath = "a".into();
        assert_eq!(tree.root().unwrap(), expected_root)
    }

    #[test]
    fn children_works_1() {
        let tree = dummy_tree();
        let children = tree
            .children("a".into())
            .unwrap()
            .map(|x| x.path().to_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            children,
            vec![
                "a/2011".into(),
                "a/2012".into(),
                "a/2013".into(),
                "a/2014".into()
            ]
        );
    }

    #[test]
    fn children_works_2() {
        let tree = dummy_tree();
        let children = tree
            .children("a/2011".into())
            .unwrap()
            .map(|x| x.path().to_owned())
            .collect::<Vec<_>>();

        assert_eq!(children, []);
    }

    #[test]
    fn tree_can_be_rerooted() {
        let tree = dummy_tree();
        let new_root: &RPath = "a/2012/invoices".into();
        let tree = tree.reroot(new_root).unwrap();
        assert_eq!(tree.root().unwrap(), new_root);

        let children = tree
            .children(tree.root().unwrap())
            .unwrap()
            .map(|x| x.path().to_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            children,
            vec![
                "a/2012/invoices/invoice 1".into(),
                "a/2012/invoices/invoice 2".into()
            ]
        );
    }

    #[derive(Clone, Debug)]
    pub struct TreeRepository {
        tree: FileTree<PathInfo>,
    }

    impl TreeRepository {
        pub fn new(tree: FileTree<PathInfo>) -> TreeRepository {
            Self { tree }
        }
    }

    impl FileRepository for TreeRepository {
        type Reader = Cursor<Vec<u8>>;

        const CONCURRENCY_LIMIT: usize = 4;

        async fn list_files(
            &self,
            path: &crate::paths::RPath,
            depth: usize,
        ) -> anyhow::Result<Vec<PathInfo>> {
            let ret = self
                .tree
                .all_children(path)
                .ok_or(Error::NotFound)?
                .filter(|other| other.path().len() - path.len() <= depth)
                .cloned()
                .collect();

            Ok(ret)
        }

        /// Get metadata of the file at the specified path.
        async fn metadata(&self, path: &RPath) -> anyhow::Result<PathInfo> {
            match self.tree.get(path) {
                Some(info) => Ok(info.clone()),
                None => return Err(anyhow!(Error::NotFound)),
            }
        }

        async fn read_file(
            &self,
            _path: &crate::paths::RPath,
        ) -> anyhow::Result<(crate::FileInfo, Self::Reader)> {
            unimplemented!()
        }

        async fn create_file<R: AsyncReader>(
            &self,
            _path: &RPath,
            _src: R,
            _len: u64,
        ) -> crate::file_repository::Result<FileInfo> {
            unimplemented!()
        }

        async fn update_file<R: AsyncReader>(
            &self,
            _path: &RPath,
            _src: R,
            _len: u64,
        ) -> crate::file_repository::Result<FileInfo> {
            unimplemented!()
        }

        async fn set_modified_time(
            &self,
            _path: &RPath,
            _time: time::OffsetDateTime,
        ) -> crate::file_repository::Result<()> {
            unimplemented!()
        }

        async fn mkdir(&self, _path: &crate::paths::RPath) -> anyhow::Result<()> {
            unimplemented!()
        }

        async fn delete(&self, _path: &crate::paths::RPath) -> anyhow::Result<()> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn file_tree_is_equivalent_to_list_files_root() {
        let tree: FileTree<PathInfo> = vec![
            PathInfo::Dir(RPathBuf::empty()),
            PathInfo::Dir("some".into()),
            PathInfo::Dir("some/dir".into()),
            PathInfo::Dir("some/dir/that".into()),
            PathInfo::File(FileInfo {
                path: "some/dir/that/file".into(),
                size: 42,
                modified: OffsetDateTime::now_utc(),
            }),
            PathInfo::File(FileInfo {
                path: "file".into(),
                size: 42,
                modified: OffsetDateTime::now_utc(),
            }),
        ]
        .try_into()
        .unwrap();

        let repo = TreeRepository::new(tree.clone());

        let output = file_tree_of_repository(&repo, RPath::empty())
            .await
            .unwrap();

        assert_eq!(output.len(), tree.len());
    }

    #[tokio::test]
    async fn file_tree_returns_partial_trees() {
        let tree: FileTree<PathInfo> = vec![
            PathInfo::Dir(RPathBuf::empty()),
            PathInfo::Dir("some".into()),
            PathInfo::Dir("some/dir".into()),
            PathInfo::Dir("some/dir/that".into()),
        ]
        .try_into()
        .unwrap();

        let repo = TreeRepository::new(tree.clone());

        let output = file_tree_of_repository(&repo, "some/dir/that/does/not/exist".into())
            .await
            .unwrap();

        assert_eq!(output, tree);
    }

    #[tokio::test]
    async fn file_tree_returns_partial_trees_when_called_on_files() {
        let tree: FileTree<PathInfo> = vec![
            PathInfo::Dir(RPathBuf::empty()),
            PathInfo::Dir("some".into()),
            PathInfo::Dir("some/dir".into()),
            PathInfo::File(FileInfo {
                path: "some/dir/that".into(),
                size: 42,
                modified: OffsetDateTime::now_utc(),
            }),
        ]
        .try_into()
        .unwrap();

        let repo = TreeRepository::new(tree.clone());

        let output = file_tree_of_repository(&repo, "some/dir/that".into())
            .await
            .unwrap();

        assert_eq!(output, tree);
    }

    #[tokio::test]
    async fn file_might_be_empty() {
        let tree: FileTree<PathInfo> = vec![
            PathInfo::Dir(RPathBuf::empty()),
            PathInfo::Dir("some".into()),
            PathInfo::Dir("some/dir".into()),
            PathInfo::Dir("some/dir/that".into()),
        ]
        .try_into()
        .unwrap();

        let repo = TreeRepository::new(tree.clone());

        let output = file_tree_of_repository(&repo, "something/else/entirely".into())
            .await
            .unwrap()
            .into_vec();

        assert_eq!(output, vec![PathInfo::Dir(RPathBuf::empty())]);
    }
}
