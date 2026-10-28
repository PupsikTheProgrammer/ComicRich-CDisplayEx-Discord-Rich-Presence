//! What the open comic is: title, issue, format and, for collected editions,
//! which issue the reader is currently in.

pub mod cache;
mod comicinfo;
mod comicvine;
pub mod filename;
mod imagehash;
mod metron;
mod online;
mod segments;
pub mod worker;

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::archive::Listing;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Kind {
    #[default]
    Unknown,
    /// Issue of an ongoing series.
    Issue,
    /// Issue of a limited series.
    Limited,
    OneShot,
    Annual,
    /// TPB, hardcover, omnibus, complete run in one file.
    Collection,
    GraphicNovel,
}

impl Kind {
    /// Maps ComicInfo `Format` values and database series types.
    pub fn from_format(format: &str) -> Option<Kind> {
        let f = format.trim().to_lowercase();
        let has = |s: &str| f.contains(s);
        if f == "tpb" || f == "hc" || has("trade") || has("hardcover") || has("hard cover") || has("omnibus")
            || has("collect") || has("compendium") || has("digest")
        {
            Some(Kind::Collection)
        } else if has("one-shot") || has("one shot") || has("oneshot") {
            Some(Kind::OneShot)
        } else if has("annual") {
            Some(Kind::Annual)
        } else if has("limited") || has("mini") {
            Some(Kind::Limited)
        } else if has("graphic novel") || f == "ogn" {
            Some(Kind::GraphicNovel)
        } else if has("ongoing") || has("series") || has("cancelled") {
            Some(Kind::Issue)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IssueRef {
    pub series: String,
    pub year: Option<u32>,
    pub number: String,
}

impl IssueRef {
    pub fn label(&self) -> String {
        format!("{} #{}", self.series, self.number)
    }
}

/// Where one collected issue starts inside a collection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    /// Zero-based page index.
    pub start: usize,
    pub label: String,
    pub cover: Option<String>,
    /// False when the start page is estimated rather than detected.
    pub exact: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ComicMeta {
    pub kind: Kind,
    /// Series name for issues; the full title for collections and one-shots.
    pub title: String,
    pub number: Option<String>,
    pub volume: Option<u32>,
    pub year: Option<u32>,
    /// Issues in a limited series.
    pub count: Option<u32>,
    pub issue_title: Option<String>,
    /// Cover image URL.
    pub cover: Option<String>,
    /// Issues a collection reprints, in reading order.
    pub contents: Vec<IssueRef>,
    pub segments: Vec<Segment>,
    /// "file", "metron" or "comicvine".
    pub source: String,
    /// Online lookup was attempted.
    pub online: bool,
}

impl ComicMeta {
    /// The issue containing `page`; `None` before the first issue and in
    /// unidentified stretches.
    pub fn segment_at(&self, page: usize) -> Option<&Segment> {
        self.segments.iter().rev().find(|s| s.start <= page).filter(|s| !s.label.is_empty())
    }

    fn has_exact_segments(&self) -> bool {
        self.segments.iter().any(|s| s.exact)
    }
}

/// Metadata from the file alone: ComicInfo.xml, file and folder names, archive layout.
pub fn local(path: &Path, listing: &Listing) -> ComicMeta {
    let info = listing.comic_info.as_deref().and_then(|entry| comicinfo::read(path, entry)).unwrap_or_default();
    let parsed = filename::parse_path(path);
    let total = listing.pages.len();

    let mut m = ComicMeta { source: "file".into(), ..Default::default() };
    m.title = some(&info.series).unwrap_or_else(|| parsed.series.clone());
    if m.title.is_empty() {
        m.title = filename::clean(path);
    }
    m.number = some(&info.number).map(|n| filename::normalize_number(&n)).or(parsed.number.clone());
    // Taggers often put the series start year into Volume.
    let info_volume: Option<u32> = some(&info.volume).and_then(|v| v.parse().ok());
    let (info_volume, volume_year) = match info_volume {
        Some(v) if v >= 1900 => (None, Some(v)),
        v => (v, None),
    };
    m.volume = info_volume.or(parsed.volume);
    m.year = some(&info.year).and_then(|y| y.parse().ok()).or(volume_year).or(parsed.year);
    m.count = some(&info.count).and_then(|c| c.parse().ok()).filter(|&c| c > 0).or(parsed.of);
    m.issue_title = some(&info.title).filter(|t| *t != m.title).or(parsed.title.clone());

    m.contents = comicinfo::parse_collects(&info.summary);
    if m.contents.is_empty() && m.number.is_none() {
        if let Some((a, b)) = parsed.range {
            m.contents = (a..=b)
                .map(|n| IssueRef { series: parsed.series.clone(), year: parsed.year, number: n.to_string() })
                .collect();
        }
    }

    m.segments = segments::from_bookmarks(&info);
    if m.segments.is_empty() {
        m.segments = segments::from_structure(listing);
    }

    m.kind = Kind::from_format(&info.format)
        .or(parsed.hint)
        .unwrap_or(if !m.contents.is_empty() || m.segments.len() >= 2 {
            Kind::Collection
        } else if m.number.is_some() {
            // ComicInfo Count is also set for ongoing series; only "of N" or a
            // numbered folder range means a limited series.
            if parsed.of.is_some() { Kind::Limited } else { Kind::Issue }
        } else if !listing.total_known {
            Kind::Unknown
        } else if total > 80 {
            Kind::Collection
        } else {
            Kind::OneShot
        });

    if m.kind == Kind::Collection && m.segments.is_empty() {
        m.segments = segments::estimate(total, &m.contents, &[]);
    }
    m
}

fn some(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}
