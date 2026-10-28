//! Presence text for each kind of comic.

use crate::config::Language;
use crate::meta::{ComicMeta, Kind};

/// Activity content apart from timing.
pub struct Lines {
    pub details: String,
    pub state: Option<String>,
    pub large_text: String,
    pub large_image: Option<String>,
    pub small_image: Option<String>,
    pub small_text: Option<String>,
}

/// Longest `Series #N: Story title` before the story title is dropped.
const MAX_DETAILS_WITH_TITLE: usize = 64;

/// `page` is one-based; `total` is `None` when unknown.
pub fn lines(lang: Language, m: &ComicMeta, page: usize, total: Option<usize>) -> Lines {
    let pg = page_line(lang, page, total);
    let issue = match &m.number {
        Some(n) => format!("{} #{n}", m.title),
        None => m.title.clone(),
    };
    let issue_full = match &m.issue_title {
        Some(t) if m.number.is_some() && issue.len() + t.len() + 2 <= MAX_DETAILS_WITH_TITLE => format!("{issue}: {t}"),
        _ => issue.clone(),
    };
    let simple = |details: String, state: String| Lines {
        large_text: details.clone(),
        details,
        state: Some(state),
        large_image: m.cover.clone(),
        small_image: None,
        small_text: None,
    };

    match m.kind {
        Kind::Collection => match m.segment_at(page - 1) {
            Some(seg) => {
                let label = if seg.exact { seg.label.clone() } else { format!("≈ {}", seg.label) };
                Lines {
                    details: label,
                    state: Some(format!("{} · {pg}", m.title)),
                    large_text: format!("{} — {}", m.title, seg.label),
                    small_image: seg.cover.as_ref().and(m.cover.clone()),
                    small_text: seg.cover.as_ref().and(Some(m.title.clone())),
                    large_image: seg.cover.clone().or(m.cover.clone()),
                }
            }
            None => simple(m.title.clone(), format!("{} · {pg}", kind_label(lang, Kind::Collection))),
        },
        Kind::Issue => simple(issue_full, format!("{} · {pg}", volume_line(lang, m))),
        Kind::Limited => {
            let label = match (&m.number, m.count) {
                (Some(n), Some(c)) => match lang {
                    Language::Ru => format!("Лимитка, выпуск {n} из {c}"),
                    Language::En => format!("Limited series, issue {n} of {c}"),
                },
                _ => kind_label(lang, Kind::Limited).to_string(),
            };
            simple(issue_full, format!("{label} · {pg}"))
        }
        Kind::OneShot | Kind::Annual | Kind::GraphicNovel => {
            simple(issue_full, format!("{} · {pg}", kind_label(lang, m.kind)))
        }
        Kind::Unknown => simple(issue_full, pg),
    }
}

/// `Runaways, том 1 (2003)`.
fn volume_line(lang: Language, m: &ComicMeta) -> String {
    let mut s = m.title.clone();
    if let Some(v) = m.volume {
        s.push_str(&match lang {
            Language::Ru => format!(", том {v}"),
            Language::En => format!(" Vol. {v}"),
        });
    }
    if let Some(y) = m.year {
        s.push_str(&format!(" ({y})"));
    }
    s
}

fn kind_label(lang: Language, kind: Kind) -> &'static str {
    match (lang, kind) {
        (Language::Ru, Kind::Collection) => "Сборник",
        (Language::Ru, Kind::OneShot) => "Ван-шот",
        (Language::Ru, Kind::Annual) => "Ежегодник",
        (Language::Ru, Kind::GraphicNovel) => "Графический роман",
        (Language::Ru, Kind::Limited) => "Лимитка",
        (Language::Ru, _) => "Выпуск",
        (Language::En, Kind::Collection) => "Collected edition",
        (Language::En, Kind::OneShot) => "One-shot",
        (Language::En, Kind::Annual) => "Annual",
        (Language::En, Kind::GraphicNovel) => "Graphic novel",
        (Language::En, Kind::Limited) => "Limited series",
        (Language::En, _) => "Issue",
    }
}

pub fn page_line(lang: Language, page: usize, total: Option<usize>) -> String {
    match (lang, total) {
        (Language::Ru, Some(t)) => format!("Стр. {page} из {t}"),
        (Language::Ru, None) => format!("Стр. {page}"),
        (Language::En, Some(t)) => format!("Page {page} of {t}"),
        (Language::En, None) => format!("Page {page}"),
    }
}

pub fn reading_line(lang: Language) -> &'static str {
    match lang {
        Language::Ru => "Читает комикс",
        Language::En => "Reading a comic",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::Segment;

    fn meta(kind: Kind) -> ComicMeta {
        ComicMeta { kind, title: "Runaways".into(), number: Some("1".into()), ..Default::default() }
    }

    #[test]
    fn issue_shows_volume() {
        let m = ComicMeta { volume: Some(1), year: Some(2003), issue_title: Some("Pride & Joy".into()), ..meta(Kind::Issue) };
        let l = lines(Language::Ru, &m, 17, Some(25));
        assert_eq!(l.details, "Runaways #1: Pride & Joy");
        assert_eq!(l.state.as_deref(), Some("Runaways, том 1 (2003) · Стр. 17 из 25"));
    }

    #[test]
    fn limited_shows_count() {
        let m = ComicMeta { count: Some(6), ..meta(Kind::Limited) };
        let l = lines(Language::En, &m, 3, Some(24));
        assert_eq!(l.state.as_deref(), Some("Limited series, issue 1 of 6 · Page 3 of 24"));
    }

    #[test]
    fn collection_shows_current_issue() {
        let seg = |start, label: &str, exact| Segment { start, label: label.into(), cover: None, exact };
        let m = ComicMeta {
            kind: Kind::Collection,
            title: "Rise of the Darkhold".into(),
            segments: vec![seg(1, "Marvel Spotlight #3", true), seg(60, "Werewolf By Night #3", false)],
            ..Default::default()
        };
        assert_eq!(lines(Language::Ru, &m, 1, Some(496)).details, "Rise of the Darkhold");
        assert_eq!(lines(Language::Ru, &m, 2, Some(496)).details, "Marvel Spotlight #3");
        let l = lines(Language::Ru, &m, 72, Some(496));
        assert_eq!(l.details, "≈ Werewolf By Night #3");
        assert_eq!(l.state.as_deref(), Some("Rise of the Darkhold · Стр. 72 из 496"));
    }
}
