use crate::paths::RPath;
use anyhow::bail;
use merge::{Merge, Side};

mod merge;

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

impl<T: Node> Node for &T {
    fn path(&self) -> &RPath {
        T::path(self)
    }

    fn typ(&self) -> Type {
        T::typ(self)
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Debug, Clone, Copy)]
pub enum PatchKind {
    Update(Type),
    Delete(Type),
    Create(Type),
    Replace { old: Type, new: Type },
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Debug, Clone, Copy)]
pub struct Patch<'a> {
    pub path: &'a RPath,
    pub kind: PatchKind,
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
                nodes[i].typ() == Type::File && nodes[i + 1].path().starts_with(nodes[i].path());
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

        let reachable_nodes = ret.count_reachable_nodes();
        if reachable_nodes != ret.len() {
            bail!(
                "Tree is not connected (only {reachable_nodes} are reachable from the root {:?})",
                ret.root().unwrap()
            );
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

    /// If `max_depth == 0`, this iterator will be empty (the node itself is never returned).
    /// If `max_depth == 1`, the immediate children of the node are returned
    fn all_children<'a>(
        &'a self,
        path: &'a RPath,
        max_depth: usize,
    ) -> impl Iterator<Item = &'a T> + 'a {
        self.get_index(path)
            .map(move |idx| {
                self.nodes[idx + 1..]
                    .iter()
                    .take_while(move |node| node.path().starts_with(path))
                    .filter(move |node| node.path().len() - path.len() <= max_depth)
            })
            .into_iter()
            .flatten()
    }

    pub fn children<'a>(&'a self, path: &'a RPath) -> impl Iterator<Item = &'a T> + 'a {
        self.all_children(path, 1)
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
            println!("{:?}", path);

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
    let mut patches = Vec::new();

    let merged = Merge::new(new.nodes.iter(), old.nodes.iter(), |x, y| {
        x.path().cmp(y.path())
    });

    let mut skip_over: Option<&RPath> = None;
    for side in merged {
        let path = match side {
            Side::Left(x) => x.path(),
            Side::Right(y) => y.path(),
            Side::Both { left, .. } => {
                // Note that both left and right compare equal according to the above
                // cmp function. Terefore, left and right have the same path
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

        match side {
            Side::Left(x) => {
                // x is only present in the new tree, so we must create it in the old one
                patches.push(Patch {
                    path,
                    kind: PatchKind::Create(x.typ()),
                })
            }
            Side::Right(y) => {
                // y is only present in the old tree, so it must have been deleted
                patches.push(Patch {
                    path,
                    kind: PatchKind::Delete(y.typ()),
                });

                // Skip over all children of this node (we do not care about the children that
                // may have also been deleted)
                skip_over = Some(path);
            }
            Side::Both {
                left: new,
                right: old,
            } => match (new.typ(), old.typ()) {
                (typ @ Type::Dir, Type::Dir) | (typ @ Type::File, Type::File) => {
                    patches.push(Patch {
                        path,
                        kind: PatchKind::Update(typ),
                    });
                }
                (Type::Dir, Type::File) => {
                    patches.push(Patch {
                        path,
                        kind: PatchKind::Replace {
                            new: Type::Dir,
                            old: Type::File,
                        },
                    });
                }
                (Type::File, Type::Dir) => {
                    patches.push(Patch {
                        path,
                        kind: PatchKind::Replace {
                            new: Type::File,
                            old: Type::Dir,
                        },
                    });

                    // Skip over the children of the deleted directory
                    skip_over = Some(path)
                }
            },
        }
    }

    patches.sort();

    Ok(patches)
}

#[cfg(test)]
mod test {
    use crate::{
        assert_equal_when_sorted,
        filetree::{diff, FileTreeBuilder, Patch},
        paths::{RPath, RPathBuf},
    };

    use super::{FileTree, Node, PatchKind, Type};

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

        fn typ(&self) -> Type {
            match self {
                Self::Dir(_) => Type::Dir,
                Self::File(_) => Type::File,
            }
        }
    }

    fn patch(s: &'static str, kind: PatchKind) -> Patch<'static> {
        use ahash::HashMap;
        use std::sync::{LazyLock, Mutex};

        static CACHE: LazyLock<Mutex<HashMap<&'static str, &'static RPath>>> =
            LazyLock::new(|| Default::default());
        let mut map = CACHE.lock().unwrap();
        let path: &'static RPath = map
            .entry(s)
            .or_insert_with(|| Box::leak(Box::new(RPathBuf::from_sep(s, '/'))));
        drop(map);

        Patch { path, kind }
    }

    fn filetree(nodes: &[&'static str]) -> anyhow::Result<FileTree<Nd>> {
        let mut builder = FileTreeBuilder::new();
        for node in nodes {
            let nd = if let Some(folder) = node.strip_suffix('/') {
                Nd::Dir(RPathBuf::from(folder))
            } else {
                Nd::File(RPathBuf::from(*node))
            };

            builder.push(nd);
        }

        builder.build()
    }

    #[test]
    fn trees_must_be_connected() {
        let err = filetree(&[
            "/",
            "/dir/",
            "/dir/file",
            "/dir/file2",
            "/dir2/file1",
            "/dir2/file2"
        ])
        .unwrap_err();

        let msg = err.to_string();
        assert!(err.to_string().contains("connected"), "Wrong error: {msg}");
    }

    #[test]
    fn basic_test() {
        let a = filetree(&[
            "/",
            "/a_movie",
            "/some document"
        ]).unwrap();

        let b = filetree(&[
            "/",
            "/some other document",
            "/a_movie",
            "/old picture"
        ]).unwrap();

        let mut patches = diff(&a, &b).unwrap();
        let mut expected = [
            patch("", PatchKind::Update(Type::Dir)),
            patch("a_movie", PatchKind::Update(Type::File)),
            patch("old picture", PatchKind::Delete(Type::File)),
            patch("some other document", PatchKind::Delete(Type::File)),
            patch("some document", PatchKind::Create(Type::File))
        ];
        
        expected.sort();
        patches.sort();

        assert_equal_when_sorted(&patches, &expected);
    }

    #[test]
    fn folder_turns_into_file() {
        let a = filetree(&[
            "/",
            "/2013/",
            "/2012/",
            "/2012/invoices/",
            "/2013/readme",
            "/2012/invoices/invoice 1",
            "/2012/invoices/invoice 2"
        ]).unwrap();

        let b = filetree(&[
            "/",
            "/2013/",
            "/2013/readme",
            "/2012"
        ]).unwrap();

        // Note that we expect only one deleted item (the directory. We do not want the
        // the deleted children to be listed)
        let patches = diff(&b, &a).unwrap();
        assert_equal_when_sorted(
            &patches,
            &[
                patch("", PatchKind::Update(Type::Dir)),
                patch("2013", PatchKind::Update(Type::Dir)),
                patch("2013/readme", PatchKind::Update(Type::File)),
                patch("2012", PatchKind::Replace { old: Type::Dir, new: Type::File })
            ],
        );

        let patches = diff(&a, &b).unwrap();
        assert_equal_when_sorted(
            &patches,
            &[
                patch("", PatchKind::Update(Type::Dir)),
                patch("2013", PatchKind::Update(Type::Dir)),
                patch("2013/readme", PatchKind::Update(Type::File)),
                patch("2012", PatchKind::Replace { old: Type::File, new: Type::Dir }),
                patch("2012/invoices", PatchKind::Create(Type::Dir)),
                patch("2012/invoices/invoice 1", PatchKind::Create(Type::File)),
                patch("2012/invoices/invoice 2", PatchKind::Create(Type::File)),
            ],
        );
    }

    #[test]
    fn deletions_are_short() {
        // Keep deletion patches short. That is, if a parent path is already deleted,
        // we do not need to list every single child that is deleted
        let old = filetree(&[
            "/",
            "/2013/",
            "2013/invoice 1",
            "2013/invoice 2"
        ]).unwrap();

        let new = filetree(&["/"]).unwrap();

        let patches = diff(&new, &old).unwrap();
        assert_equal_when_sorted(
            &patches,
            &[patch("", PatchKind::Update(Type::Dir)), patch("2013", PatchKind::Delete(Type::Dir))],
        );
    }
}
