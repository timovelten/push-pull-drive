use std::{cmp::Ordering, iter::Peekable};

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Side<L, R> {
    Left(L),
    Right(R),
    Both { left: L, right: R },
}

/// Travserses both lists in parallel, in sorted order. If an item appears
/// in both lists, this is indicated by the [`Side::Both`] variant.
pub struct Merge<I, J, L, R, F>
where
    I: Iterator<Item = L>,
    J: Iterator<Item = R>,
{
    left: Peekable<I>,
    right: Peekable<J>,
    f: F,
}

impl<I, J, L, R, F> Merge<I, J, L, R, F>
where
    I: Iterator<Item = L>,
    J: Iterator<Item = R>,
    F: FnMut(&L, &R) -> Ordering,
{
    pub fn new(left: I, right: J, f: F) -> Self {
        Self {
            left: left.peekable(),
            right: right.peekable(),
            f,
        }
    }
}

impl<I, J, L, R, F> Iterator for Merge<I, J, L, R, F>
where
    I: Iterator<Item = L>,
    J: Iterator<Item = R>,
    F: FnMut(&L, &R) -> Ordering,
{
    type Item = Side<L, R>;

    fn next(&mut self) -> Option<Self::Item> {
        let (x, y) = match (self.left.peek(), self.right.peek()) {
            (None, None) => return None,
            (Some(_), None) => return Some(Side::Left(self.left.next().unwrap())),
            (None, Some(_)) => return Some(Side::Right(self.right.next().unwrap())),
            (Some(x), Some(y)) => (x, y),
        };

        // By assumption, left.peek() <= right.peek(). So x <= than all elements of y.
        let ret = match (self.f)(x, y) {
            Ordering::Less => {
                // the right list is sorted, and x is strictly less than the first element
                // so x is strictly less than all elements in right
                Side::Left(self.left.next().unwrap())
            }
            Ordering::Equal => Side::Both {
                left: self.left.next().unwrap(),
                right: self.right.next().unwrap(),
            },
            Ordering::Greater => {
                // See above for reasoning
                Side::Right(self.right.next().unwrap())
            }
        };

        Some(ret)
    }
}

#[cfg(test)]
mod test {
    use super::{Side, Merge};

    #[test]
    fn test_sorted_intersection() {
        let left = [1, 2, 3, 5, 5, 67, 68, 100];
        let right = [1, 3, 5, 5, 68, 101, 103];

        let res = Merge::new(left.into_iter(), right.into_iter(), |x, y| x.cmp(y))
            .collect::<Vec<_>>();

        let should_be = [
            Side::Both { left: 1, right: 1 },
            Side::Left(2),
            Side::Both { left: 3, right: 3 },
            Side::Both { left: 5, right: 5 },
            Side::Both { left: 5, right: 5 },
            Side::Left(67),
            Side::Both { left: 68, right: 68},
            Side::Left(100),
            Side::Right(101),
            Side::Right(103)
        ];

        assert_eq!(res, should_be);
    }
}
