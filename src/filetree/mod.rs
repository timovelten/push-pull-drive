use crate::paths::RPath;
use anyhow::{bail, ensure, Context as _};
use merge::{Merge, Side};

mod merge;

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Type {
    Dir,
    File,
}

impl Type {
    fn is_dir(&self) -> bool {
        matches!(self, Self::Dir)
    }
}

/// We implement this trait for a bunch of types
pub trait Node: core::fmt::Debug {
    fn path(&self) -> &RPath;

    fn typ(&self) -> Type;

    fn as_tuple(&self) -> (&RPath, Type) {
        (self.path(), self.typ())
    }
}

impl<T: Node> Node for &T {
    fn path(&self) -> &RPath {
        T::path(self)
    }

    fn typ(&self) -> Type {
        T::typ(self)
    }
}

/// Generally speaking, all of these operations should happen in the order defined by [Ord]
/// and they may not be reordered arbitrarily.
///
/// Specifically, it is vital that all deletions happen before all creations, and that the creation
/// of directories happens before the creation of files.
///
/// It is okay to reorder [`Self::Update`] with other patches of type [`Self::Update`].
#[derive(PartialEq, Eq, PartialOrd, Ord, Debug, Clone, Copy)]
pub enum Patch<'a> {
    Delete(Type, &'a RPath),
    Update(Type, &'a RPath),
    Create(Type, &'a RPath),
}

// Constructing these in tests is annoying because of the lifetime
#[cfg(test)]
macro_rules! patch {
    ($variant:ident; $ty:ident; $($elem:expr),*) => {
        {
            let rp: &'static RPathBuf = Box::leak(RPathBuf::from([$($elem),*]).into());
            Patch::$variant(Type::$ty, rp)
        }
    };
}

pub struct FileTreeBuilder<T> {
    nodes: Vec<T>,
}

impl<T: Node> FileTreeBuilder<T> {
    pub fn new() -> Self {
        Self {
            nodes: Default::default(),
        }
    }

    pub fn push(&mut self, node: T) {
        self.nodes.push(node);
    }

    pub fn extend<I: IntoIterator<Item = T>>(&mut self, nodes: I) {
        self.nodes.extend(nodes)
    }

    pub fn extended_with<I: IntoIterator<Item = T>>(mut self, nodes: I) -> Self {
        self.nodes.extend(nodes);
        self
    }

    pub fn build(self) -> anyhow::Result<FileTree<T>> {
        let mut nodes = self.nodes;
        nodes.sort_by(|a, b| a.path().cmp(&b.path()));

        if nodes.is_empty() {
            return Ok(FileTree { nodes });
        }

        // Ensure the paths of the nodes are unique. Also make sure that all files are leafs (and have no children)
        // and that the tree is connected.
        for i in 0..(nodes.len() - 1) {
            if nodes[i].path() == nodes[i + 1].path() {
                bail!(
                    "paths must be unique but the path {:?} appears twice",
                    nodes[i].path()
                )
            }

            let is_file_with_children =
                nodes[i].typ() == Type::File && nodes[i + 1].path().starts_with(&nodes[i].path());
            if is_file_with_children {
                bail!(
                    "the file {:?} has children, but files may never have children",
                    nodes[i].path()
                )
            }
        }

        // Note that we do not guarantee that the root of the tree (i.e. nodes[0]) is a directory

        // Constructing a non-connected FileTree is allowed, but only by us.
        let ret = FileTree { nodes };
        
        if ret.count_reachable_nodes() != ret.len() {
            bail!("Tree is not connected");
        }

        Ok(ret)
    }
}

#[derive(Clone, Debug)]
pub struct FileTree<T> {
    /// This list must not be modified, and the only way to construct it, is to use try from below.
    ///
    /// This ensures that the list is sorted properly, and that paths cannot appear twice.
    nodes: Vec<T>,
}

impl<T: Node> FileTree<T> {
    fn get_index(&self, path: &RPath) -> Result<usize, usize> {
        self.nodes.binary_search_by_key(&path, |node| &node.path())
    }

    pub fn get(&self, path: &RPath) -> Option<&T> {
        self.get_index(path)
            .ok()
            .and_then(|idx| self.nodes.get(idx))
    }

    pub fn all_children<'a>(&'a self, path: &'a RPath) -> impl Iterator<Item = &'a T> + 'a {
        self.get_index(path)
            .map(|idx| {
                self.nodes[idx + 1..]
                    .iter()
                    .take_while(move |node| node.path().starts_with(path))
            })
            .into_iter()
            .flatten()
    }

    pub fn children<'a>(&'a self, path: &'a RPath) -> impl Iterator<Item = &'a T> + 'a {
        self.all_children(path)
            .filter(|node| node.path().len() == path.len() + 1)
    }

    pub fn root(&self) -> Option<&RPath> {
        self.nodes.get(0).map(|x| x.path())
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    fn count_reachable_nodes(&self) -> usize {
        fn f<D: Node>(tree: &FileTree<D>, path: &RPath) -> usize {
            // Count the node at path itself
            let mut ret = 1;

            for child in tree.children(path) {
                ret += match child.typ() {
                    Type::Dir => f(tree, &child.path()),
                    Type::File => 1,
                };
            }

            ret
        }

        if let Some(root) = self.root() {
            f(self, root)
        } else {
            0
        }
    }
}

/// What operations do we have to do to the old tree so that it equals the new tree?
/// 
/// Nodes that appear in both trees are output as [`Patch::Update`], regardless of whether
/// they were actually updated or not (there is no way for this function to determine that).
pub fn diff<'a, A: Node, B: Node>(
    new: &'a FileTree<A>,
    old: &'a FileTree<B>,
) -> anyhow::Result<Vec<Patch<'a>>> {
    ensure!(
        new.count_reachable_nodes() == new.len(),
        "left tree is not connected"
    );
    ensure!(
        old.count_reachable_nodes() == old.len(),
        "right tree is not connected"
    );
    ensure!(!new.is_empty(), "new tree must not be empty");

    fn f<'a, A: Node, B: Node>(
        path: &'a RPath,
        new_tree: &'a FileTree<A>,
        old_tree: &'a FileTree<B>,
        patches: &mut Vec<Patch<'a>>,
    ) {
        let xs = new_tree.children(path);
        let ys = old_tree.children(path);
        let merged = Merge::new(xs, ys, |x, y| x.path().cmp(y.path()));

        for state in merged {
            let descend = match state {
                Side::Left(x) => {
                    // x is only present in the new tree, so we must create it in the old one
                    patches.push(Patch::Create(x.typ(), x.path()));

                    x.typ().is_dir().then(|| x.path())
                }
                Side::Right(y) => {
                    // y is only present in the old tree, so it must have been deleted
                    patches.push(Patch::Delete(y.typ(), y.path()));

                    // Do NOT descend into paths that are deleted in the old tree. We already have
                    // deleted the parent, we do not need to know about every single deleted child.
                    // Those changes are implied.
                    None
                }
                Side::Both { left: x, right: y } => {
                    let the_path = x.path();

                    match (x.typ(), y.typ()) {
                        (Type::Dir, Type::Dir) => {
                            patches.push(Patch::Update(Type::Dir, the_path));
                            Some(the_path)
                        }
                        (Type::File, Type::File) => {
                            patches.push(Patch::Update(Type::File, the_path));

                            // Do not need to traverse files (they wont have any children anyway)
                            None
                        }
                        (Type::Dir, Type::File) => {
                            patches.push(Patch::Delete(Type::File, the_path));
                            patches.push(Patch::Create(Type::Dir, the_path));

                            Some(the_path)
                        }
                        (Type::File, Type::Dir) => {
                            patches.push(Patch::Delete(Type::Dir, the_path));
                            patches.push(Patch::Create(Type::File, the_path));

                            // Do NOT descend into a deleted directory in the old tree
                            None
                        }
                    }
                }
            };

            // Descend into possible child directories
            if let Some(path) = descend {
                f(path, new_tree, old_tree, patches)
            }
        }
    }

    // The two trees might not have the same root, or may even be empty (?)
    let start = match (new.root(), old.root()) {
        (None, None) => return Ok(Default::default()),
        (Some(root), None) | (None, Some(root)) => root,
        (Some(new_root), Some(old_root)) if new_root == old_root => new_root,
        (Some(_), Some(_)) => {
            todo!("Should we allow this case and determine a common starting point?");
        }
    };

    let mut patches = Vec::new();
    f(start, new, old, &mut patches);
    patches.sort();

    Ok(patches)
}

#[cfg(test)]
mod test {
    use crate::{
        assert_equal_when_sorted,
        filetree::{diff, FileTreeBuilder, Patch},
        paths::RPathBuf,
    };

    use super::{Node, Type};

    #[derive(Debug)]
    enum Nd {
        File(RPathBuf),
        Dir(RPathBuf),
    }

    impl Nd {
        fn file(p: impl Into<RPathBuf>) -> Self {
            Self::File(p.into())
        }

        fn dir(p: impl Into<RPathBuf>) -> Self {
            Self::Dir(p.into())
        }

        fn root() -> Self {
            Self::Dir(RPathBuf::empty())
        }
    }

    impl Node for Nd {
        fn path(&self) -> &crate::paths::RPath {
            match self {
                Self::Dir(path) => path,
                Self::File(path) => path,
            }
        }

        fn typ(&self) -> Type {
            match self {
                Self::Dir(_) => Type::Dir,
                Self::File(_) => Type::File,
            }
        }
    }

    #[test]
    fn test_sort_order() {
        let update_file = patch!(Update; File; "test");
        let delete_dir = patch!(Delete; Dir; "test");
        let create_dir = patch!(Create; Dir; "dir");
        let create_file = patch!(Create; File; "dir", "file");

        let patches = &[update_file, delete_dir, create_dir, create_file];
        assert_equal_when_sorted(patches, &[delete_dir, update_file, create_dir, create_file]);
    }

    #[test]
    fn trees_must_be_connected() {
        let _ = FileTreeBuilder::new()
            .extended_with([
                Nd::root(),
                Nd::dir(["dir"]),
                Nd::file(["dir", "file"]),
                Nd::file(["dir", "file2"]),
                Nd::file(["dir2", "file1"]),
                Nd::file(["dir2", "file2"]),
            ])
            .build()
            .unwrap_err();
    }

    #[test]
    fn basic_test() {
        let a = FileTreeBuilder::new()
            .extended_with([
                Nd::root(),
                Nd::file(["a_movie"]),
                Nd::file(["some document"]),
            ])
            .build()
            .unwrap();

        let b = FileTreeBuilder::new()
            .extended_with([
                Nd::root(),
                Nd::file(["some other document"]),
                Nd::file(["a_movie"]),
                Nd::file(["old picture"]),
            ])
            .build()
            .unwrap();

        let patches = diff(&a, &b).unwrap();
        assert_equal_when_sorted(
            &patches,
            &[
                patch!(Update; File; "a_movie"),
                patch!(Delete; File; "old picture"),
                patch!(Delete; File; "some other document"),
                patch!(Create; File; "some document"),
            ],
        );

        let patches = diff(&b, &a).unwrap();
        assert_equal_when_sorted(
            &patches,
            &[
                patch!(Update; File; "a_movie"),
                patch!(Delete; File; "some document"),
                patch!(Create; File; "some other document"),
                patch!(Create; File; "old picture"),
            ],
        );
    }

    #[test]
    fn folder_turns_into_file() {
        let a = FileTreeBuilder::new()
            .extended_with([
                Nd::root(),
                Nd::dir(["2013"]),
                Nd::dir(["2012"]),
                Nd::dir(["2012", "invoices"]),
                Nd::file(["2013", "readme"]),
                Nd::file(["2012", "invoices", "invoice 1"]),
                Nd::file(["2012", "invoices", "invoice 2"]),
            ])
            .build()
            .unwrap();

        let b = FileTreeBuilder::new()
            .extended_with([
                Nd::root(),
                Nd::dir(["2013"]),
                Nd::file(["2013", "readme"]),
                Nd::file(["2012"]),
            ])
            .build()
            .unwrap();

        // Note that we expect only one deleted item (the directory. We do not want the
        // the deleted children to be listed)
        let patches = diff(&b, &a).unwrap();
        assert_equal_when_sorted(
            &patches,
            &[
                patch!(Update; Dir; "2013"),
                patch!(Update; File; "2013", "readme"),
                patch!(Delete; Dir; "2012"),
                patch!(Create; File; "2012"),
            ],
        );

        let patches = diff(&a, &b).unwrap();
        assert_equal_when_sorted(
            &patches,
            &[
                patch!(Update; Dir; "2013"),
                patch!(Update; File; "2013", "readme"),
                patch!(Delete; File; "2012"),
                patch!(Create; Dir; "2012"),
                patch!(Create; Dir; "2012", "invoices"),
                patch!(Create; File; "2012", "invoices", "invoice 1"),
                patch!(Create; File; "2012", "invoices", "invoice 2"),
            ],
        );
    }

    #[test]
    fn deletions_are_short_1() {
        // Keep deletion patches short. That is, if a parent path is already deleted,
        // we do not need to list every single child that is deleted.

        let old = FileTreeBuilder::new()
            .extended_with([
                Nd::root(),
                Nd::dir(["2013"]),
                Nd::file(["2013", "invoice 1"]),
                Nd::file(["2013", "invoice 2"]),
            ])
            .build()
            .unwrap();

        let new = FileTreeBuilder::new()
            .extended_with([Nd::root()])
            .build()
            .unwrap();

        let patches = diff(&new, &old).unwrap();
        assert_equal_when_sorted(&patches, &[patch!(Delete; Dir; "2013")]);
    }
}
