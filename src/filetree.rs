use crate::paths::{RPath, RPathBuf};
use anyhow::{bail, ensure};

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Type {
    Dir,
    File,
}

/// We implement this trait for a bunch of types
pub trait Node: core::fmt::Debug {
    fn path(&self) -> &RPath;

    fn typ(&self) -> Type;
}

/// Generally speaking, all of these operations should happen in the order defined by order
/// and they may not be reordered arbitrarily.
///
/// However, we do allow [`Self::UpdateFile`] and [`Self::CreateFile`] operations to
/// be swapped with other [`Self::UpdateFile`] and [`Self::CreateFile`] operations (i.e.
/// it does not matter in what order the files are created or updated)
#[derive(PartialEq, Eq, PartialOrd, Ord, Debug, Clone)]
pub enum Patch<'a> {
    DeleteFile(&'a RPath),
    DeleteDir(&'a RPath),
    UpdateDir(&'a RPath),
    UpdateFile(&'a RPath),
    CreateDir(&'a RPath),
    CreateFile(&'a RPath),
}

impl<'a> Patch<'a> {
    fn delete_from(node: &'a impl Node) -> Self {
        match node.typ() {
            Type::Dir => Self::DeleteDir(node.path()),
            Type::File => Self::DeleteFile(node.path()),
        }
    }

    fn create_from(node: &'a impl Node) -> Self {
        match node.typ() {
            Type::Dir => Self::CreateDir(node.path()),
            Type::File => Self::CreateFile(node.path()),
        }
    }
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
            bail!("FileTree must not be empty")
        }

        // Count the number of distinct paths. Also make sure that all files are leafs (and have no children)
        let root = &nodes[0];
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
                    "the file {:?} has children, but fails may never have children",
                    nodes[i].path()
                )
            }

            ensure!(
                nodes[i].path().starts_with(&root.path()),
                "{:?} is not a child of root path {:?}",
                nodes[i].path(),
                root.path()
            );
        }

        // Ensure that the first node

        // Note that we do not guarantee that the root of the tree (i.e. nodes[0]) is a directory

        Ok(FileTree { nodes })
    }
}

#[derive(Clone)]
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

    fn all_children<'a>(&'a self, path: &'a RPath) -> impl Iterator<Item = &'a T> + 'a {
        // If path is not in the list, return no elements. This somewhat awkward construction does
        // just that.
        self.get_index(path)
            .map(|idx| {
                self.nodes[idx + 1..]
                    .iter()
                    .take_while(move |node| node.path().starts_with(path))
            })
            .into_iter()
            .flatten()
    }

    fn children<'a>(&'a self, path: &'a RPath) -> impl Iterator<Item = &'a T> + 'a {
        self.all_children(path)
            .filter(|node| node.path().len() == path.len() + 1)
    }

    fn root(&self) -> &T {
        &self.nodes[0]
    }

    fn len(&self) -> usize {
        self.nodes.len()
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

        f(self, self.root().path())
    }
}

/// What operations do we have to do to the old tree so that it equals the new tree?
///
/// This outputs a tuple `(list, patches)` where `list` contains all nodes of
/// type file that are present in both trees, and patches contains operations that need to be
/// done to the right tree in order for it to match the left tree.
///
/// We guarantee that the list of patches is sorted.
pub fn diff<'a, A: Node, B: Node>(
    new: &'a FileTree<A>,
    old: &'a FileTree<B>,
) -> anyhow::Result<Vec<Patch<'a>>> {
    ensure!(
        new.root().path() == new.root().path(),
        "roots of filetrees do not match {} vs {}",
        new.root().path(),
        old.root().path()
    );
    ensure!(
        new.count_reachable_nodes() == new.len(),
        "left tree is not connected"
    );
    ensure!(
        old.count_reachable_nodes() == old.len(),
        "right tree is not connected"
    );

    fn f<'a, A: Node, B: Node>(
        path: &'a RPath,
        new_tree: &'a FileTree<A>,
        old_tree: &'a FileTree<B>,
        patches: &mut Vec<Patch<'a>>,
    ) {
        // Invariant: path exists in new_tree and may or may not exist in old_tree.

        // Both of these are sorted. If path does not exist in the old tree, ys will be empty
        let xs = new_tree.children(path);
        let mut ys = old_tree.children(path).peekable();

        for x in xs {
            while let Some(y) = ys.next_if(|y| y.path() < x.path()) {
                patches.push(Patch::delete_from(y));
            }

            // At this point the "top" (xs.peek()) of x is smaller than all elements in ys
            if let Some(y) = ys.next_if(|y| y.path() == x.path()) {
                match (x.typ(), y.typ()) {
                    (Type::Dir, Type::Dir) => {
                        patches.push(Patch::UpdateDir(x.path()));
                    }
                    (Type::File, Type::File) => {
                        patches.push(Patch::UpdateFile(x.path()));
                    }
                    (Type::Dir, Type::File) => {
                        patches.push(Patch::DeleteFile(x.path()));
                        patches.push(Patch::CreateDir(x.path()));
                    }
                    (Type::File, Type::Dir) => {
                        patches.push(Patch::DeleteDir(x.path()));
                        patches.push(Patch::CreateFile(x.path()));
                    }
                }
            } else {
                // All elements in y are strictly bigger than the top of x. Therefore
                // x cannot exist in y and needs to be created
                patches.push(Patch::create_from(x));
            }

            // If this is a directory, keep going
            if x.typ() == Type::Dir {
                f(&x.path(), new_tree, old_tree, patches)
            }
        }

        // Elements in y that are left over need to be deleted
        for y in ys {
            patches.push(Patch::delete_from(y));
        }
    }

    let mut patches = Vec::new();

    f(old.root().path(), new, old, &mut patches);

    patches.sort();
    Ok(patches)
}

#[cfg(test)]
mod test {
    use crate::{
        assert_equal_when_sorted,
        filetree::{diff, FileTree, FileTreeBuilder, Patch},
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
                Self::Dir(path) => Type::Dir,
                Self::File(path) => Type::File,
            }
        }
    }

    // That this works at all is kind of a mircale (that the reference lives long enough).
    // If it didn't we maybe would have had to leak the array or something...
    macro_rules! patch {
        ($ty:ident; $($elem:expr),*) => {
               Patch::$ty(&RPathBuf::from([$($elem),*]))
        };
    }

    #[test]
    fn reachability_works() {
        let left = FileTreeBuilder::new()
            .extended_with([
                Nd::root(),
                Nd::dir(["dir"]),
                Nd::file(["dir", "file"]),
                Nd::file(["dir", "file2"]),
                Nd::file(["dir2", "file1"]),
                Nd::file(["dir2", "file2"]),
            ])
            .build()
            .unwrap();

        assert_eq!(left.count_reachable_nodes(), 4);
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
                patch!(UpdateFile; "a_movie"),
                patch!(DeleteFile; "old picture"),
                patch!(DeleteFile; "some other document"),
                patch!(CreateFile; "some document"),
            ],
        );

        let patches = diff(&b, &a).unwrap();
        assert_equal_when_sorted(
            &patches,
            &[
                patch!(UpdateFile; "a_movie"),
                patch!(DeleteFile; "some document"),
                patch!(CreateFile; "some other document"),
                patch!(CreateFile; "old picture"),
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

        let patches = diff(&b, &a).unwrap();
        assert_equal_when_sorted(
            &patches,
            &[
                patch!(UpdateDir; "2013"),
                patch!(UpdateFile; "2013", "readme"),
                patch!(DeleteDir; "2012"),
                patch!(CreateFile; "2012"),
            ],
        );

        let patches = diff(&a, &b).unwrap();
        assert_equal_when_sorted(
            &patches,
            &[
                patch!(UpdateDir; "2013"),
                patch!(UpdateFile; "2013", "readme"),
                patch!(DeleteFile; "2012"),
                patch!(CreateDir; "2012"),
                patch!(CreateDir; "2012", "invoices"),
                patch!(CreateFile; "2012", "invoices", "invoice 1"),
                patch!(CreateFile; "2012", "invoices", "invoice 2"),
            ],
        );
    }
}
