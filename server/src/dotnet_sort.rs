//! .NET's sort, for the places where the game sorts with it: `List<T>.Sort(Comparison<T>)` and
//! `List<T>.Sort()` are an introsort (ArraySortHelper<T>.IntrospectiveSort), which does not keep
//! equal elements in order. The order they end in decides who is served first, so the same steps
//! are taken here, comparison by comparison, as in the game's mscorlib (Mono, Unity 6000.3.2f1):
//! insertion sort for up to 16 elements, otherwise a median-of-three quicksort that falls back to
//! a heap sort when the depth runs out (2 * bit length of the count). tests/dotnet_sort/ has the
//! reference it is checked against, and a run of the same cases on a real Mono.

use std::cmp::Ordering;

/// The comparer made a search leave the array (the game: an InvalidOperationException, the list
/// left as far as the sort had got).
#[derive(Debug, PartialEq)]
pub struct BadComparer;

type R = Result<(), BadComparer>;

/// Sorts like `List<T>.Sort(Comparison<T>)` of the game, `cmp` being its comparison.
pub fn sort_by<T>(keys: &mut [T], mut cmp: impl FnMut(&T, &T) -> Ordering) -> R {
    let n = keys.len();
    if n < 2 {
        return Ok(());
    }
    intro_sort(keys, 0, n as isize - 1, 2 * (usize::BITS - n.leading_zeros()), &mut cmp)
}

fn intro_sort<T>(keys: &mut [T], lo: isize, mut hi: isize, mut depth: u32, cmp: &mut impl FnMut(&T, &T) -> Ordering) -> R {
    while hi > lo {
        match hi - lo + 1 {
            2 => swap_if_greater(keys, cmp, lo, hi),
            3 => {
                swap_if_greater(keys, cmp, lo, hi - 1);
                swap_if_greater(keys, cmp, lo, hi);
                swap_if_greater(keys, cmp, hi - 1, hi);
            }
            ..=16 => insertion_sort(keys, cmp, lo, hi),
            _ if depth == 0 => heap_sort(keys, cmp, lo, hi),
            _ => {
                depth -= 1;
                let p = pick_pivot_and_partition(keys, cmp, lo, hi)?;
                intro_sort(keys, p + 1, hi, depth, cmp)?;
                hi = p - 1;
                continue;
            }
        }
        return Ok(());
    }
    Ok(())
}

fn swap_if_greater<T>(keys: &mut [T], cmp: &mut impl FnMut(&T, &T) -> Ordering, a: isize, b: isize) {
    if a != b && cmp(&keys[a as usize], &keys[b as usize]) == Ordering::Greater {
        keys.swap(a as usize, b as usize);
    }
}

fn pick_pivot_and_partition<T>(keys: &mut [T], cmp: &mut impl FnMut(&T, &T) -> Ordering, lo: isize, hi: isize) -> Result<isize, BadComparer> {
    let middle = lo + (hi - lo) / 2;
    swap_if_greater(keys, cmp, lo, middle);
    swap_if_greater(keys, cmp, lo, hi);
    swap_if_greater(keys, cmp, middle, hi);
    // The pivot goes next to the end and stays there until the last swap.
    keys.swap(middle as usize, hi as usize - 1);
    let pivot = hi as usize - 1;
    let (mut left, mut right) = (lo, hi - 1);
    while left < right {
        loop {
            left += 1;
            if cmp(keys.get(left as usize).ok_or(BadComparer)?, &keys[pivot]) != Ordering::Less {
                break;
            }
        }
        loop {
            right -= 1;
            if cmp(&keys[pivot], keys.get(usize::try_from(right).map_err(|_| BadComparer)?).ok_or(BadComparer)?) != Ordering::Less {
                break;
            }
        }
        if left >= right {
            break;
        }
        keys.swap(left as usize, right as usize);
    }
    keys.swap(left as usize, pivot);
    Ok(left)
}

fn heap_sort<T>(keys: &mut [T], cmp: &mut impl FnMut(&T, &T) -> Ordering, lo: isize, hi: isize) {
    let n = hi - lo + 1;
    for i in (1..=n / 2).rev() {
        down_heap(keys, cmp, i, n, lo);
    }
    for i in (2..=n).rev() {
        keys.swap(lo as usize, (lo + i - 1) as usize);
        down_heap(keys, cmp, 1, i - 1, lo);
    }
}

/// The game holds the element it sifts out of the array; here it is swapped down, which compares
/// the same elements in the same order.
fn down_heap<T>(keys: &mut [T], cmp: &mut impl FnMut(&T, &T) -> Ordering, mut i: isize, n: isize, lo: isize) {
    let at = |i: isize| (lo + i - 1) as usize;
    while i <= n / 2 {
        let mut child = 2 * i;
        if child < n && cmp(&keys[at(child)], &keys[at(child + 1)]) == Ordering::Less {
            child += 1;
        }
        if cmp(&keys[at(i)], &keys[at(child)]) != Ordering::Less {
            break;
        }
        keys.swap(at(i), at(child));
        i = child;
    }
}

fn insertion_sort<T>(keys: &mut [T], cmp: &mut impl FnMut(&T, &T) -> Ordering, lo: isize, hi: isize) {
    for i in lo..hi {
        let mut j = i;
        while j >= lo && cmp(&keys[j as usize + 1], &keys[j as usize]) == Ordering::Less {
            keys.swap(j as usize + 1, j as usize);
            j -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// tests/dotnet_sort/vectors.txt: `name|keys|order`, from reference.py, the same on a real Mono.
    #[test]
    fn equal_keys_end_in_the_order_of_the_games_sort() {
        let text = include_str!("../tests/dotnet_sort/vectors.txt");
        let mut cases = 0;
        for line in text.lines() {
            let f: Vec<&str> = line.split('|').collect();
            let keys: Vec<i32> = f[1].split(',').map(|k| k.parse().unwrap()).collect();
            let want: Vec<usize> = f[2].split(',').map(|k| k.parse().unwrap()).collect();
            let mut items: Vec<(i32, usize)> = keys.iter().copied().zip(0..).collect();
            sort_by(&mut items, |a, b| a.0.cmp(&b.0)).unwrap();
            assert_eq!(items.iter().map(|i| i.1).collect::<Vec<_>>(), want, "{}", f[0]);
            cases += 1;
        }
        assert!(cases > 70);
    }

    #[test]
    fn up_to_sixteen_equal_elements_keep_their_order_and_above_that_they_do_not() {
        for n in [0usize, 1, 2, 3, 16] {
            let mut v: Vec<(i32, usize)> = (0..n).map(|i| (0, i)).collect();
            sort_by(&mut v, |a, b| a.0.cmp(&b.0)).unwrap();
            assert!(v.windows(2).all(|w| w[0].1 < w[1].1), "{n}");
        }
        let mut v: Vec<(i32, usize)> = (0..17).map(|i| (0, i)).collect();
        sort_by(&mut v, |a, b| a.0.cmp(&b.0)).unwrap();
        assert!(!v.windows(2).all(|w| w[0].1 < w[1].1));
    }

    /// A comparer that is not an order does not panic (the game throws).
    #[test]
    fn a_comparer_that_is_no_order_ends_in_an_error_not_a_panic() {
        let mut seed = 1u32;
        for _ in 0..200 {
            let mut v: Vec<u32> = (0..60).collect();
            let r = sort_by(&mut v, |_, _| {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                [Ordering::Less, Ordering::Equal, Ordering::Greater][(seed >> 16) as usize % 3]
            });
            assert!(r.is_ok() || r == Err(BadComparer));
        }
    }
}
