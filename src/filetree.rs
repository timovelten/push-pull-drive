use crate::paths::{RPath, RPathBuf};
use anyhow::{bail, ensure};

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Type {
    Dir,
    File,
}

/// Generally speaking, all of these operations should happen in the order defined by order
/// and they may not be reordered arbitrarily.
///
/// However, we do allow [`Self::UpdateFile`] and [`Self::CreateFile`] operations to
/// be swapped with other [`Self::UpdateFile`] and [`Self::CreateFile`] operations (i.e.
/// it does not matter in what order the files are created or updated)
#[derive(PartialEq, Eq, PartialOrd, Ord, Debug, Clone)]
pub enum Patch {
    DeleteFile(RPathBuf),
    DeleteDir(RPathBuf),
    UpdateDir(RPathBuf),
    UpdateFile(RPathBuf),
    CreateDir(RPathBuf),
    CreateFile(RPathBuf),
}

impl Patch {
    fn delete_from(node: &Node) -> Self {
        match node.typ {
            Type::Dir => Self::DeleteDir(node.path.clone()),
            Type::File => Self::DeleteFile(node.path.clone()),
        }
    }

    fn create_from(node: &Node) -> Self {
        match node.typ {
            Type::Dir => Self::CreateDir(node.path.clone()),
            Type::File => Self::CreateFile(node.path.clone()),
        }
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct Node {
    path: RPathBuf,
    typ: Type,
}

#[derive(Clone)]
pub struct FileTree {
    /// This list must not be modified, and the only way to construct it, is to use try from below.
    ///
    /// This ensures that the list is sorted properly, and that paths cannot appear twice.
    nodes: Vec<Node>,
}

impl FileTree {
    pub fn try_from(items: impl IntoIterator<Item = (RPathBuf, Type)>) -> anyhow::Result<Self> {
        let mut nodes = Vec::new();
        for (path, typ) in items {
            let node = Node { path, typ };
            nodes.push(node);
        }

        nodes.sort();

        // Count the number of distinct paths
        for i in 1..nodes.len() {
            if nodes[i - 1].path == nodes[i].path {
                bail!(
                    "paths must be unique but the path {:?} appears twice",
                    nodes[i].path
                )
            }
        }

        Ok(Self { nodes })
    }

    fn get_index(&self, path: &RPath) -> Result<usize, usize> {
        self.nodes.binary_search_by_key(&path, |node| &node.path)
    }

    fn all_children<'a>(&'a self, path: &'a RPath) -> impl Iterator<Item = &'a Node> + 'a {
        // If path is not in the list, return no elements. This somewhat awkward construction does
        // just that.
        self.get_index(path)
            .map(|idx| {
                self.nodes[idx + 1..]
                    .iter()
                    .take_while(move |node| node.path.starts_with(path))
            })
            .into_iter()
            .flatten()
    }

    fn children<'a>(&'a self, path: &'a RPath) -> impl Iterator<Item = &'a Node> + 'a {
        self.all_children(path)
            .filter(|node| node.path.len() == path.len() + 1)
    }

    fn count_reachable_nodes(&self) -> usize {
        fn f(tree: &FileTree, path: &RPath) -> usize {
            // Count the node at path itself
            let mut ret = 1;

            for child in tree.children(path) {
                ret += match child.typ {
                    Type::Dir => f(tree, &child.path),
                    Type::File => 1,
                };
            }

            ret
        }

        f(self, self.root())
    }

    fn root(&self) -> &RPath {
        &self.nodes[0].path
    }

    fn len(&self) -> usize {
        self.nodes.len()
    }
}

/// What operations do we have to do to the old tree so that it equals the new tree?
///
/// This outputs a tuple `(list, patches)` where `list` contains all nodes of
/// type file that are present in both trees, and patches contains operations that need to be
/// done to the right tree in order for it to match the left tree.
///
/// We guarantee that the list of patches is sorted.
pub fn diff(new: &FileTree, old: &FileTree) -> anyhow::Result<Vec<Patch>> {
    ensure!(
        new.root() == new.root(),
        "roots of filetrees do not match {} vs {}",
        new.root(),
        old.root()
    );
    ensure!(
        new.count_reachable_nodes() == new.len(),
        "left tree is not connected"
    );
    ensure!(
        old.count_reachable_nodes() == old.len(),
        "right tree is not connected"
    );

    fn f(path: &RPath, new_tree: &FileTree, old_tree: &FileTree, patches: &mut Vec<Patch>) {
        // Invariant: path exists in new_tree and may or may not exist in old_tree.

        // Both of these are sorted. If path does not exist in the old tree, ys will be empty
        let xs = new_tree.children(path);
        let mut ys = old_tree.children(path).peekable();

        for x in xs {
            while let Some(y) = ys.next_if(|y| *y < x) {
                patches.push(Patch::delete_from(y));
            }

            // At this point the "top" (xs.peek()) of x is smaller than all elements in ys
            if let Some(y) = ys.next_if(|y| y.path == x.path) {
                match (x.typ, y.typ) {
                    (Type::Dir, Type::Dir) => {
                        patches.push(Patch::UpdateDir(x.path.clone()));
                    }
                    (Type::File, Type::File) => {
                        patches.push(Patch::UpdateFile(x.path.clone()));
                    }
                    (Type::Dir, Type::File) => {
                        patches.push(Patch::DeleteFile(x.path.clone()));
                        patches.push(Patch::CreateDir(x.path.clone()));
                    }
                    (Type::File, Type::Dir) => {
                        patches.push(Patch::DeleteDir(x.path.clone()));
                        patches.push(Patch::CreateFile(x.path.clone()));
                    }
                }
            } else {
                // All elements in y are strictly bigger than the top of x. Therefore
                // x cannot exist in y and needs to be created
                patches.push(Patch::create_from(x));
            }

            // If this is a directory, keep going
            if x.typ == Type::Dir {
                f(&x.path, new_tree, old_tree, patches)
            }
        }

        // Elements in y that are left over need to be deleted
        for y in ys {
            patches.push(Patch::delete_from(y));
        }
    }

    let mut patches = Vec::new();

    f(old.root(), new, old, &mut patches);

    patches.sort();
    Ok(patches)
}

#[cfg(test)]
mod test {
    use crate::{
        filetree::{diff, FileTree, Patch},
        paths::RPathBuf,
    };

    use super::Type;

    fn file<const N: usize>(p: [&'static str; N]) -> (RPathBuf, Type) {
        (p.into(), Type::File)
    }

    fn dir<const N: usize>(p: [&'static str; N]) -> (RPathBuf, Type) {
        (p.into(), Type::Dir)
    }

    #[test]
    fn reachability_works() {
        let left = FileTree::try_from([
            file(["dir", "file"]),
            file(["dir", "file2"]),
            dir(["dir"]),
            file(["dir", "dir2", "file"]),
            dir(["dir", "dir2", "file2"]),
        ])
        .unwrap();

        println!("{:?}", left.nodes);

        for child in left.children(left.root()) {
            println!("{:?}", child);
        }

        assert_eq!(left.count_reachable_nodes(), 3);
    }

    fn assert_equals_unordered<T: Ord + Clone + core::fmt::Debug>(a: &[T], b: &[T]) {
        let mut a = Vec::from(a);
        let mut b = Vec::from(b);
        a.sort();
        b.sort();

        assert_eq!(a, b)
    }

    #[test]
    fn basic_test() {
        let a = FileTree::try_from([dir([]), file(["a_movie"]), file(["some document"])]).unwrap();

        let b = FileTree::try_from([
            dir([]),
            file(["some other document"]),
            file(["a_movie"]),
            file(["old picture"]),
        ])
        .unwrap();

        let patches = diff(&a, &b).unwrap();
        assert_equals_unordered(
            &patches,
            &[
                Patch::UpdateFile(["a_movie"].into()),
                Patch::DeleteFile(["old picture"].into()),
                Patch::DeleteFile(["some other document"].into()),
                Patch::CreateFile(["some document"].into()),
            ],
        );

        let patches = diff(&b, &a).unwrap();
        assert_equals_unordered(
            &patches,
            &[
                Patch::UpdateFile(["a_movie"].into()),
                Patch::DeleteFile(["some document"].into()),
                Patch::CreateFile(["some other document"].into()),
                Patch::CreateFile(["old picture"].into()),
            ],
        );
    }

    #[test]
    fn folder_turns_into_file() {
        let a = FileTree::try_from([
            dir([]),
            dir(["2013"]),
            dir(["2012"]),
            dir(["2012", "invoices"]),
            file(["2013", "readme"]),
            file(["2012", "invoices", "invoice 1"]),
            file(["2012", "invoices", "invoice 2"]),
        ])
        .unwrap();

        let b = FileTree::try_from([
            dir([]),
            dir(["2013"]),
            file(["2013", "readme"]),
            file(["2012"]),
        ])
        .unwrap();

        let patches = diff(&b, &a).unwrap();
        assert_equals_unordered(
            &patches,
            &[
                Patch::UpdateDir(["2013"].into()),
                Patch::UpdateFile(["2013", "readme"].into()),
                Patch::DeleteDir(["2012"].into()),
                Patch::CreateFile(["2012"].into()),
            ],
        );

        let patches = diff(&a, &b).unwrap();
        assert_equals_unordered(
            &patches,
            &[
                Patch::UpdateDir(["2013"].into()),
                Patch::UpdateFile(["2013", "readme"].into()),
                Patch::DeleteFile(["2012"].into()),
                Patch::CreateDir(["2012"].into()),
                Patch::CreateDir(["2012", "invoices"].into()),
                Patch::CreateFile(["2012", "invoices", "invoice 1"].into()),
                Patch::CreateFile(["2012", "invoices", "invoice 2"].into()),
            ],
        );
    }
}
