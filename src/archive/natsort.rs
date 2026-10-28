//! Natural, case-insensitive ordering: `page 2` < `page 10`.

use std::cmp::Ordering;

pub fn compare(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let da = take_digits(&mut a);
                let db = take_digits(&mut b);
                let (ta, tb) = (da.trim_start_matches('0'), db.trim_start_matches('0'));
                let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb)).then_with(|| da.len().cmp(&db.len()));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(x), Some(y)) => {
                let ord = x.to_lowercase().cmp(y.to_lowercase());
                if ord != Ordering::Equal {
                    return ord;
                }
                a.next();
                b.next();
            }
        }
    }
}

fn take_digits(it: &mut std::iter::Peekable<std::str::Chars>) -> String {
    let mut s = String::new();
    while let Some(c) = it.next_if(|c| c.is_ascii_digit()) {
        s.push(c);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_numbers_naturally() {
        let mut v = vec!["p10.jpg", "P2.jpg", "p1.jpg", "p02.jpg"];
        v.sort_by(|a, b| compare(a, b));
        assert_eq!(v, ["p1.jpg", "P2.jpg", "p02.jpg", "p10.jpg"]);
    }

    #[test]
    fn handles_zero_padded_runs() {
        assert_eq!(compare("x 00003.jpg", "x 00010.jpg"), Ordering::Less);
        assert_eq!(compare("a\\001.jpg", "b\\000.jpg"), Ordering::Less);
    }
}
