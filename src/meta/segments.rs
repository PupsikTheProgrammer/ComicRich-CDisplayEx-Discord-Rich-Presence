//! Splits a collected edition into the issues it reprints.
//!
//! Sources, most reliable first: ComicInfo bookmarks, one folder or inner archive
//! per issue, issue covers recognized among the pages, and finally an even spread.

use std::path::Path;

use super::comicinfo::ComicInfo;
use super::imagehash::{self, PageHashes};
use super::{filename, IssueRef, Segment};
use crate::archive::Listing;

/// Folders smaller than this are treated as extras, not issues.
const MIN_ISSUE_PAGES: usize = 8;
/// dHash distance (of 64 bits) for a confident cover match anywhere in the book.
const STRONG_DISTANCE: u32 = 10;
/// Looser distance accepted between confident matches, if the page stands out
/// from the runner-up by `WEAK_MARGIN`.
const WEAK_DISTANCE: u32 = 17;
const WEAK_MARGIN: u32 = 4;
const MIN_ISSUE_GAP: usize = 2;
/// Unlocated issues need at least this share of a typical issue's pages each.
const MIN_ROOM: f64 = 0.6;
/// A segment longer than this many typical issues is cut short...
const LONG_GAP: f64 = 1.8;
/// ...after this many typical issue lengths.
const OVERRUN: f64 = 1.25;
const NO_MATCH: u32 = 64;

pub fn from_bookmarks(info: &ComicInfo) -> Vec<Segment> {
    let segments: Vec<Segment> = info
        .pages
        .page
        .iter()
        .filter(|p| !p.bookmark.trim().is_empty())
        .filter_map(|p| {
            Some(Segment { start: p.image.parse().ok()?, label: p.bookmark.trim().to_string(), cover: None, exact: true })
        })
        .collect();
    if segments.len() >= 2 { segments } else { Vec::new() }
}

pub fn from_structure(listing: &Listing) -> Vec<Segment> {
    let mut groups: Vec<(&str, usize, usize)> = Vec::new();
    for (i, page) in listing.pages.iter().enumerate() {
        let folder = page.rsplit_once('\\').map_or("", |(f, _)| f);
        match groups.last_mut() {
            Some(g) if g.0 == folder => g.2 += 1,
            _ => groups.push((folder, i, 1)),
        }
    }
    let issues: Vec<_> = groups.into_iter().filter(|g| g.2 >= MIN_ISSUE_PAGES).collect();
    if issues.len() < 2 {
        return Vec::new();
    }
    issues
        .into_iter()
        .map(|(folder, start, _)| Segment { start, label: folder_label(folder), cover: None, exact: true })
        .collect()
}

fn folder_label(folder: &str) -> String {
    let last = folder.rsplit('\\').next().unwrap_or(folder);
    let parsed = filename::parse(&filename::stem(Path::new(last)));
    match parsed.number {
        Some(n) if !parsed.series.is_empty() => format!("{} #{n}", parsed.series),
        _ => filename::clean(Path::new(last)),
    }
}

/// Finds each issue's cover among the pages.
///
/// Database covers are often scans of yellowed print copies while collections
/// carry restored art, so matching runs in two passes: confident matches aligned
/// to the reading order first, then weaker ones accepted only between those
/// anchors and only when one page clearly stands out.
pub fn match_covers(pages: &[Option<PageHashes>], covers: &[Option<u64>]) -> Vec<Option<usize>> {
    let dist: Vec<Vec<u32>> = covers
        .iter()
        .map(|c| {
            pages
                .iter()
                .map(|p| match (c, p) {
                    (Some(c), Some(p)) => imagehash::distance(p, *c),
                    _ => NO_MATCH,
                })
                .collect()
        })
        .collect();

    let mut anchors = align(&dist, STRONG_DISTANCE);
    for (i, a) in anchors.iter().enumerate() {
        if let Some(j) = a {
            log!("cover of issue {} matched page {} (distance {})", i + 1, j + 1, dist[i][*j]);
        }
    }

    for i in 0..covers.len() {
        if anchors[i].is_some() || covers[i].is_none() {
            continue;
        }
        let lo = anchors[..i].iter().rev().find_map(|a| *a).map_or(0, |p| p + MIN_ISSUE_GAP);
        let hi = anchors[i + 1..].iter().find_map(|a| *a).map_or(pages.len(), |p| p.saturating_sub(MIN_ISSUE_GAP - 1));
        if lo >= hi {
            continue;
        }
        let mut ranked: Vec<(u32, usize)> = (lo..hi).map(|j| (dist[i][j], j)).collect();
        ranked.sort_unstable();
        let (d, j) = ranked[0];
        let runner_up = ranked.iter().find(|(_, k)| k.abs_diff(j) > 1).map_or(NO_MATCH, |r| r.0);
        if d <= WEAK_DISTANCE && runner_up >= d + WEAK_MARGIN {
            log!("cover of issue {} matched page {} (weak, distance {d}, next best {runner_up})", i + 1, j + 1);
            anchors[i] = Some(j);
        } else {
            log!("cover of issue {} not found (best page {} at distance {d})", i + 1, j + 1);
        }
    }
    anchors
}

/// Order-preserving alignment of issues to pages that maximizes the number of
/// matches within `max_distance`, preferring closer ones.
fn align(dist: &[Vec<u32>], max_distance: u32) -> Vec<Option<usize>> {
    let n = dist.len();
    let m = dist.first().map_or(0, Vec::len);
    let score = |i: usize, j: usize| {
        let d = dist[i][j];
        if d <= max_distance { 1000 - d } else { 0 }
    };
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in 1..=n {
        for j in 1..=m {
            let s = score(i - 1, j - 1);
            let take = if s > 0 { dp[i - 1][j - 1] + s } else { 0 };
            dp[i][j] = dp[i - 1][j].max(dp[i][j - 1]).max(take);
        }
    }
    let mut anchors = vec![None; n];
    let (mut i, mut j) = (n, m);
    while i > 0 && j > 0 {
        let s = score(i - 1, j - 1);
        if s > 0 && dp[i][j] == dp[i - 1][j - 1] + s {
            anchors[i - 1] = Some(j - 1);
            i -= 1;
            j -= 1;
        } else if dp[i][j] == dp[i - 1][j] {
            i -= 1;
        } else {
            j -= 1;
        }
    }
    anchors
}

/// Segments for `contents`, using detected start pages where known and
/// interpolating the rest.
pub fn estimate(total: usize, contents: &[IssueRef], anchors: &[Option<usize>]) -> Vec<Segment> {
    estimate_with_covers(total, contents, anchors, &[])
}

pub fn estimate_with_covers(
    total: usize,
    contents: &[IssueRef],
    anchors: &[Option<usize>],
    covers: &[Option<String>],
) -> Vec<Segment> {
    let n = contents.len();
    if n < 2 || total < n {
        return Vec::new();
    }
    let keep = plausible(total, n, anchors);
    // Known points as (issue index, page); page 1 skips the collection's own cover.
    let mut known: Vec<(f64, f64)> = Vec::new();
    if anchors.first().copied().flatten().is_none() {
        known.push((0.0, 1.0));
    }
    known.extend(anchors.iter().enumerate().filter_map(|(i, a)| a.map(|p| (i as f64, p as f64))));
    known.push((n as f64, total as f64));

    let segments = contents
        .iter()
        .enumerate()
        .filter(|(i, _)| keep[*i])
        .map(|(i, issue)| {
            let anchor = anchors.get(i).copied().flatten();
            let start = anchor.unwrap_or_else(|| {
                let x = i as f64;
                let after = known.iter().position(|k| k.0 >= x).unwrap_or(known.len() - 1);
                let (b, a) = (known[after], known[after.saturating_sub(1)]);
                if b.0 == x || b.0 == a.0 {
                    b.1 as usize
                } else {
                    (a.1 + (b.1 - a.1) * (x - a.0) / (b.0 - a.0)) as usize
                }
            });
            Segment {
                start,
                label: issue.label(),
                cover: covers.get(i).cloned().flatten(),
                exact: anchor.is_some(),
            }
        })
        .collect();
    match typical_issue_length(anchors) {
        Some(typical) => close_long_gaps(segments, total, typical),
        None => segments,
    }
}

/// An issue running far longer than usual means unlocated material follows it
/// (issues listed out of reading order, extras). Past a typical issue's length
/// the pages get an unlabeled segment rather than the previous issue's name.
fn close_long_gaps(segments: Vec<Segment>, total: usize, typical: f64) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::with_capacity(segments.len());
    for (k, s) in segments.iter().enumerate() {
        let next = segments.get(k + 1).map_or(total, |n| n.start);
        out.push(s.clone());
        if (next.saturating_sub(s.start)) as f64 > LONG_GAP * typical {
            let start = s.start + (OVERRUN * typical) as usize;
            log!("pages {}..{} after {} are not identified", start + 1, next, s.label);
            out.push(Segment { start, label: String::new(), cover: None, exact: false });
        }
    }
    out
}

/// Which issues get a segment. A run of unlocated issues is dropped when the pages
/// between its neighbouring located covers could not hold it: such issues are
/// elsewhere in the book (the contents list is not always in reading order) or
/// reprinted only in part, and guessing would label pages with the wrong issue.
fn plausible(total: usize, n: usize, anchors: &[Option<usize>]) -> Vec<bool> {
    let mut keep = vec![true; n];
    let Some(typical) = typical_issue_length(anchors) else { return keep };
    let located = |i: usize| anchors.get(i).copied().flatten();
    let mut i = 0;
    while i < n {
        if located(i).is_some() {
            i += 1;
            continue;
        }
        let start = i;
        while i < n && located(i).is_none() {
            i += 1;
        }
        // The located issue before the run also needs its pages.
        let before = start.checked_sub(1).and_then(located);
        let from = before.unwrap_or(0);
        let to = located(i).unwrap_or(total);
        let slots = (i - start) + before.is_some() as usize;
        if ((to.saturating_sub(from)) as f64) < MIN_ROOM * slots as f64 * typical {
            log!("issues {}..={} do not fit between pages {} and {}; not shown", start + 1, i, from + 1, to);
            keep[start..i].iter_mut().for_each(|k| *k = false);
        }
    }
    keep
}

/// Median length of issues whose start and the next issue's start were both located.
fn typical_issue_length(anchors: &[Option<usize>]) -> Option<f64> {
    let mut lengths: Vec<usize> = anchors
        .windows(2)
        .filter_map(|w| match (w[0], w[1]) {
            (Some(a), Some(b)) if b > a => Some(b - a),
            _ => None,
        })
        .collect();
    if lengths.len() < 2 {
        return None;
    }
    lengths.sort_unstable();
    Some(lengths[lengths.len() / 2] as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_issues_that_cannot_fit() {
        // Issues 0-3 are ~20 pages each; issues 4-5 would have to squeeze into the
        // 22 pages after issue 3's start, so they are not placed there.
        let anchors = [Some(0), Some(20), Some(40), Some(60), None, None, Some(82)];
        let s = estimate(100, &refs(7), &anchors);
        let labels: Vec<&str> = s.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, ["X #1", "X #2", "X #3", "X #4", "X #7"]);
    }

    #[test]
    fn long_unexplained_run_is_unlabeled() {
        // Issue 2 seemingly runs 100 pages while issues are ~20 long.
        let anchors = [Some(0), Some(20), Some(120), Some(140)];
        let s = estimate(160, &refs(4), &anchors);
        let meta = super::super::ComicMeta { segments: s, ..Default::default() };
        assert_eq!(meta.segment_at(30).unwrap().label, "X #2");
        assert!(meta.segment_at(60).is_none());
        assert_eq!(meta.segment_at(125).unwrap().label, "X #3");
    }

    fn refs(n: usize) -> Vec<IssueRef> {
        (1..=n).map(|i| IssueRef { series: "X".into(), year: None, number: i.to_string() }).collect()
    }

    #[test]
    fn even_spread_without_anchors() {
        let s = estimate(101, &refs(4), &[]);
        let starts: Vec<usize> = s.iter().map(|s| s.start).collect();
        assert_eq!(starts, [1, 26, 51, 76]);
        assert!(s.iter().all(|s| !s.exact));
    }

    #[test]
    fn interpolates_between_anchors() {
        let s = estimate(100, &refs(4), &[Some(2), None, Some(60), None]);
        let starts: Vec<usize> = s.iter().map(|s| s.start).collect();
        assert_eq!(starts, [2, 31, 60, 80]);
        assert_eq!(s.iter().map(|s| s.exact).collect::<Vec<_>>(), [true, false, true, false]);
    }

    #[test]
    fn matches_covers_in_order() {
        let p = |h: u64| Some([h; imagehash::CROPS]);
        let pages = [p(0), p(0xFF), p(0), p(0xFFFF_0000), p(0xFF)];
        let anchors = match_covers(&pages, &[Some(0xFF), Some(0xFFFF_0000), Some(0xFF)]);
        assert_eq!(anchors, [Some(1), Some(3), Some(4)]);
    }

    #[test]
    fn out_of_order_match_does_not_block_the_rest() {
        let p = |h: u64| Some([h; imagehash::CROPS]);
        let (a, b, c, d) = (0x0F0F, 0xF0F0_0000, 0xFF00_FF00_0000_0000, 0x00FF_00FF_0000_00FF);
        // Contents list A, D, B, C, but D's cover appears last in the book.
        let pages = [p(a), p(!0), p(b), p(!0), p(c), p(d)];
        let anchors = match_covers(&pages, &[Some(a), Some(d), Some(b), Some(c)]);
        assert_eq!(anchors, [Some(0), None, Some(2), Some(4)]);
    }

    #[test]
    fn weak_match_needs_to_stand_out() {
        let p = |h: u64| Some([h; imagehash::CROPS]);
        let cover = 0u64;
        let weak = (1u64 << 15) - 1; // 15 bits off
        let noise = (1u64 << 30) - 1; // 30 bits off
        let anchors = match_covers(&[p(noise), p(weak), p(noise), p(noise)], &[Some(cover)]);
        assert_eq!(anchors, [Some(1)]);
        // Two similar weak candidates: nothing stands out, so no match.
        let anchors = match_covers(&[p(weak), p(noise), p(weak | 1 << 40), p(noise)], &[Some(cover)]);
        assert_eq!(anchors, [None]);
    }

    #[test]
    fn current_segment_by_page() {
        let meta = super::super::ComicMeta { segments: estimate(101, &refs(4), &[]), ..Default::default() };
        assert!(meta.segment_at(0).is_none());
        assert_eq!(meta.segment_at(25).unwrap().label, "X #1");
        assert_eq!(meta.segment_at(26).unwrap().label, "X #2");
        assert_eq!(meta.segment_at(100).unwrap().label, "X #4");
    }
}
