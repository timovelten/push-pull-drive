use std::ops::Index;

use crate::paths::RPath;
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

impl<'a> Iterator for SiblingIndicesIterator<'a> {
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
    fn get_index(&self, path: &RPath) -> Result<usize, usize> {
        self.nodes.binary_search_by_key(&path, |node| &node.path())
    }

    pub fn get(&self, path: &RPath) -> Option<&T> {
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

    pub fn root(&self) -> Option<&RPath> {
        self.nodes.get(0).map(|x| x.path())
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
        nodes.sort_by(|a, b| a.path().cmp(&b.path()));

        if nodes.is_empty() {
            return Ok(FileTree {
                nodes,
                sibling_offset: Default::default(),
            });
        }

        // Ensure the paths of the nodes are unique. Also make sure that all files are leafs (and have no children)
        // and that the tree is connected.
        let mut sibling_offset = vec![0; nodes.len()];

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
    Unchanged { terminal: bool },
    Delete,
    Create { terminal: bool },
    Replace { terminal: bool },
}

impl<'a> Node for Patch<'a> {
    fn path(&self) -> &RPath {
        &self.path
    }

    fn is_terminal(&self) -> bool {
        // This is essentially only a debugging aid. We could just return false here.
        // But returning something accurate makes the builder verify our assumptions
        match self.kind {
            PatchKind::Unchanged { terminal } => terminal,
            PatchKind::Delete => true,
            PatchKind::Create { terminal } => terminal,
            PatchKind::Replace { terminal } => terminal,
        }
    }
}

/// What operations do we have to do to the old tree so that it equals the new tree?
///
/// Nodes that appear in both trees are output as [`Patch::Update`], regardless of whether
/// they were actually updated or not (there is no way for this function to determine that).
pub fn diff<'a, A: Node, B: Node>(
    old: &'a FileTree<A>,
    new: &'a FileTree<B>,
) -> FileTree<Patch<'a>> {
    let mut patches = Vec::new();

    let merged: _ = Merge::new(old.nodes.iter(), new.nodes.iter(), |x, y| {
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

        match side {
            Side::Left(_) => {
                // x is only present in the old tree, so we must delete it in the new one
                patches.push(Patch {
                    path,
                    kind: PatchKind::Delete,
                });

                // Skip over all children of this node (we do not care about the children that
                // may have also been deleted)
                skip_over = Some(path);
            }
            Side::Right(y) => {
                // y is only present in the new tree, so it has to be created
                patches.push(Patch {
                    path,
                    kind: PatchKind::Create {
                        terminal: y.is_terminal(),
                    },
                });
            }
            Side::Both { left, right } => match (left.is_terminal(), right.is_terminal()) {
                (terminal @ true, true) | (terminal @ false, false) => patches.push(Patch {
                    path,
                    kind: PatchKind::Unchanged { terminal },
                }),
                (false, true) => {
                    patches.push(Patch {
                        path,
                        kind: PatchKind::Replace { terminal: true },
                    });

                    skip_over = Some(path)
                }
                (true, false) => patches.push(Patch {
                    path,
                    kind: PatchKind::Replace { terminal: false },
                }),
            },
        }
    }

    patches.try_into().unwrap()
}

#[cfg(test)]
mod test {
    use crate::{
        filetree::{diff, Patch},
        paths::{RPath, RPathBuf},
    };

    use super::{FileTree, Node, PatchKind};

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
        assert!(err.to_string().contains("connected"), "Wrong error: {msg}");
    }

    #[test]
    fn basic_test() {
        let old = filetree(&["/", "some other document", "a_movie", "old picture"]).unwrap();
        let new = filetree(&["/", "a_movie", "some document"]).unwrap();

        let patches = diff(&old, &new);
        let expected = patchtree(&[
            ("", PatchKind::Unchanged { terminal: false }),
            ("a_movie", PatchKind::Unchanged { terminal: true }),
            ("old picture", PatchKind::Delete),
            ("some other document", PatchKind::Delete),
            ("some document", PatchKind::Create { terminal: true }),
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
            vec!["a/2011".into(), "a/2012".into(), "a/2013".into(), "a/2014".into()]
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
            .children(&tree.root().unwrap())
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

    // #[test]
    // fn folder_turns_into_file() {
    //     let a = filetree(&[
    //         "/",
    //         "/2013/",
    //         "/2012/",
    //         "/2012/invoices/",
    //         "/2013/readme",
    //         "/2012/invoices/invoice 1",
    //         "/2012/invoices/invoice 2",
    //     ])
    //     .unwrap();

    //     let b = filetree(&["/", "/2013/", "/2013/readme", "/2012"]).unwrap();

    //     // Note that we expect only one deleted item (the directory. We do not want the
    //     // the deleted children to be listed)
    //     let patches = diff(&b, &a).unwrap();
    //     assert_equal_when_sorted(
    //         &patches,
    //         &[
    //             patch("", PatchKind::Update(Type::Dir)),
    //             patch("2013", PatchKind::Update(Type::Dir)),
    //             patch("2013/readme", PatchKind::Update(Type::File)),
    //             patch(
    //                 "2012",
    //                 PatchKind::Replace {
    //                     old: Type::Dir,
    //                     new: Type::File,
    //                 },
    //             ),
    //         ],
    //     );

    //     let patches = diff(&a, &b).unwrap();
    //     assert_equal_when_sorted(
    //         &patches,
    //         &[
    //             patch("", PatchKind::Update(Type::Dir)),
    //             patch("2013", PatchKind::Update(Type::Dir)),
    //             patch("2013/readme", PatchKind::Update(Type::File)),
    //             patch(
    //                 "2012",
    //                 PatchKind::Replace {
    //                     old: Type::File,
    //                     new: Type::Dir,
    //                 },
    //             ),
    //             patch("2012/invoices", PatchKind::Create(Type::Dir)),
    //             patch("2012/invoices/invoice 1", PatchKind::Create(Type::File)),
    //             patch("2012/invoices/invoice 2", PatchKind::Create(Type::File)),
    //         ],
    //     );
    // }

    // #[test]
    // fn deletions_are_short() {
    //     // Keep deletion patches short. That is, if a parent path is already deleted,
    //     // we do not need to list every single child that is deleted
    //     let old = filetree(&["/", "/2013/", "2013/invoice 1", "2013/invoice 2"]).unwrap();

    //     let new = filetree(&["/"]).unwrap();

    //     let patches = diff(&new, &old).unwrap();
    //     assert_equal_when_sorted(
    //         &patches,
    //         &[
    //             patch("", PatchKind::Update(Type::Dir)),
    //             patch("2013", PatchKind::Delete(Type::Dir)),
    //         ],
    //     );
    // }
}
