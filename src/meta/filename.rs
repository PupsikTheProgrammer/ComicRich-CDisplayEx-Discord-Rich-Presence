//! Extracts series, issue, volume and format hints from release file names like
//! `Runaways v2 #01.cbr`, `Dishonored 001 - The Wyrmwood Deceit (Digital).cbr` or
//! `Outlast - The Murkoff Account 01-06` (a folder).

use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use super::Kind;

const ARCHIVE_EXTENSIONS: &[&str] = &["cbz", "cbr", "cb7", "zip", "rar", "7z", "pdf"];

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Parsed {
    pub series: String,
    pub number: Option<String>,
    /// `01-06` style issue range.
    pub range: Option<(u32, u32)>,
    /// `(of 6)`.
    pub of: Option<u32>,
    pub volume: Option<u32>,
    pub year: Option<u32>,
    /// Story title after `NNN - `.
    pub title: Option<String>,
    pub hint: Option<Kind>,
}

macro_rules! re {
    ($name:ident, $pattern:expr) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| Regex::new($pattern).unwrap());
    };
}

re!(YEAR, r"[(\[](\d{4})(?:-\d{0,4})?[)\]]");
re!(GROUPS, r"\([^)]*\)|\[[^\]]*\]|\{[^}]*\}");
re!(DROP_WORDS, r"(?i)\b(?:tpb|hc|fcbd|digital)\b");
re!(OF, r"(?i)\bof\s+(\d{1,3})\b");
re!(VOLUME, r"(?i)\b(?:vol(?:ume)?\.?\s*|v)(\d{1,3})\b");
re!(RANGE, r"(?i)(?:^|\s)[#t]?(\d{1,4})\s*-\s*[#t]?(\d{1,4})\s*$");
re!(COLLECTION_WORDS, r"(?i)\s*\b(?:omnibus|collection|int[eé]grale)\S*");
// Some sites mangle `&#233;` entities into `_233_`.
re!(ENTITY, r"_(\d{2,3})_");
re!(ISSUE_HASH, r"(?i)(?:\bissue\s*)?#\s*(\d+(?:\.\d+)?[a-z]?)(?:\s+-\s+(.+))?");
re!(ISSUE_WORD, r"(?i)\bissue\s+(\d+(?:\.\d+)?)(?:\s+-\s+(.+))?");
re!(ISSUE_TAIL, r"(?:^|\s)(\d{1,4}(?:\.\d+)?)(?:\s*$|\s+-\s+(.+)$)");

static MARKERS: LazyLock<Vec<(Regex, Kind)>> = LazyLock::new(|| {
    [
        (r"(?i)\b(?:tpb|hc|trade paperback|hardcover|omnibus|compendium|collection|collected)\b|\bint[eé]grale", Kind::Collection),
        (r"(?i)\bone[- ]?shot\b|\bfcbd\b|free comic book day|\bhors[- ]s[eé]rie\b|^hs\b", Kind::OneShot),
        (r"(?i)\bannual\b", Kind::Annual),
        (r"(?i)\b(?:ogn|graphic novel)\b", Kind::GraphicNovel),
    ]
    .into_iter()
    .map(|(p, k)| (Regex::new(p).unwrap(), k))
    .collect()
});

/// File name without archive extensions, e.g. `.cbr.cbr`.
pub fn stem(path: &Path) -> String {
    let mut name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    while let Some((stem, ext)) = name.rsplit_once('.') {
        if !ARCHIVE_EXTENSIONS.iter().any(|e| ext.eq_ignore_ascii_case(e)) {
            break;
        }
        name = stem.to_string();
    }
    decode_entities(&name)
}

/// `cr_233_puscule` -> `crépuscule`, `L_39_Homme` -> `L'Homme`. Only apostrophes and
/// Latin-1 letters are decoded, so ordinary numbers between underscores survive.
fn decode_entities(name: &str) -> String {
    ENTITY
        .replace_all(name, |c: &regex::Captures| {
            match c[1].parse::<u32>().ok().filter(|&v| v == 39 || (192..=255).contains(&v)).and_then(char::from_u32) {
                Some(ch) => ch.to_string(),
                None => c[0].to_string(),
            }
        })
        .into_owned()
}

/// Readable name: release tags such as `(Digital)` or `[Group]` dropped, years kept.
pub fn clean(path: &Path) -> String {
    let name = stem(path).replace('_', " ");
    let cleaned = GROUPS.replace_all(&name, |c: &regex::Captures| {
        if YEAR.is_match(&c[0]) { c[0].to_string() } else { " ".to_string() }
    });
    let collapsed = collapse(&cleaned);
    let trimmed = collapsed.trim_matches(|c: char| c == '-' || c.is_whitespace());
    if trimmed.is_empty() { name.trim().to_string() } else { trimmed.to_string() }
}

pub fn normalize_number(n: &str) -> String {
    let n = n.trim().trim_start_matches('#');
    let t = n.trim_start_matches('0');
    if t.is_empty() || t.starts_with('.') { format!("0{t}") } else { t.to_string() }
}

/// Parses a file and folds in what its folder says (`Runaways Vol 1\Runaways 01.cbr`).
pub fn parse_path(path: &Path) -> Parsed {
    let mut p = parse(&stem(path));
    let parent = path.parent().and_then(|d| d.file_name()).map(|n| parse(&n.to_string_lossy()));
    if let Some(parent) = parent {
        let (a, b) = (key(&p.series), key(&parent.series));
        if !a.is_empty() && !b.is_empty() && (a.starts_with(&b) || b.starts_with(&a)) {
            p.volume = p.volume.or(parent.volume);
            p.year = p.year.or(parent.year);
            if p.number.is_some() {
                p.of = p.of.or(parent.range.map(|(_, last)| last));
            }
        }
    }
    p
}

pub fn parse(raw: &str) -> Parsed {
    let mut p = Parsed::default();
    let name = raw.replace('_', " ");
    p.year = YEAR.captures_iter(&name).filter_map(|c| c[1].parse().ok()).find(|y| (1930..=2100).contains(y));

    let mut s = collapse(&GROUPS.replace_all(&name, " "));
    p.hint = MARKERS.iter().find(|(re, _)| re.is_match(&s)).map(|(_, k)| *k);
    s = collapse(&DROP_WORDS.replace_all(&s, " "));

    // Every recognized token cuts the series name short at its position.
    let mut cut = s.len();
    if let Some(c) = OF.captures(&s) {
        p.of = c[1].parse().ok();
        let m = c.get(0).unwrap();
        cut = cut.min(m.start());
        // Blank it out (keeping offsets) so "of 6" is not read as issue 6.
        s.replace_range(m.range(), &" ".repeat(m.len()));
    }
    if let Some(c) = VOLUME.captures(&s) {
        p.volume = c[1].parse().ok();
        let m = c.get(0).unwrap();
        cut = cut.min(m.start());
        let after = s[m.end()..].trim_start();
        if let Some(t) = after.strip_prefix('-') {
            p.title = Some(t.trim().to_string()).filter(|t| !t.is_empty());
        }
    }
    // The issue may follow the volume tag (`Runaways v2 #01`), so search all of it.
    let head = s.trim_end();
    if let Some(c) = RANGE.captures(head).filter(|c| {
        let (a, b): (u32, u32) = (c[1].parse().unwrap_or(0), c[2].parse().unwrap_or(0));
        a < b && b < 1900
    }) {
        p.range = Some((c[1].parse().unwrap(), c[2].parse().unwrap()));
        cut = cut.min(c.get(0).unwrap().start());
    } else if let Some(c) = [&*ISSUE_HASH, &*ISSUE_WORD, &*ISSUE_TAIL]
        .iter()
        .find_map(|re| re.captures(head).filter(|c| !is_year(&c[1])))
    {
        p.number = Some(normalize_number(&c[1]));
        if let Some(t) = c.get(2) {
            p.title = Some(t.as_str().trim().to_string());
        }
        cut = cut.min(c.get(0).unwrap().start());
    }

    p.series = s[..cut].trim_matches(|c: char| " -#:,.".contains(c)).to_string();
    if p.range.is_some() {
        // `Series Integrale T01-05` collects issues of `Series`.
        p.series = COLLECTION_WORDS.replace_all(&p.series, "").trim_matches(|c: char| " -#:,.".contains(c)).to_string();
        if p.hint.is_none() && p.number.is_none() {
            p.hint = Some(Kind::Collection);
        }
    }
    // Collected editions are known by their full title, volume and subtitle included.
    if p.hint == Some(Kind::Collection) && p.number.is_none() && p.range.is_none() {
        p.series = s.trim_matches(|c: char| " -#:,.".contains(c)).to_string();
        p.title = None;
    }
    p
}

fn is_year(n: &str) -> bool {
    n.len() == 4 && n.parse::<u32>().is_ok_and(|y| (1930..=2100).contains(&y))
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Lowercase letters and digits only, for loose comparisons.
pub fn key(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Parsed {
        parse(s)
    }

    #[test]
    fn issues() {
        let r = p("Dishonored 001 - The Wyrmwood Deceit (Digital) (comicsall.net)");
        assert_eq!((r.series.as_str(), r.number.as_deref()), ("Dishonored", Some("1")));
        assert_eq!(r.title.as_deref(), Some("The Wyrmwood Deceit"));

        let r = p("Dishonored 002 (2016) (Digital) (comicsall.net)");
        assert_eq!((r.series.as_str(), r.number.as_deref(), r.year), ("Dishonored", Some("2"), Some(2016)));

        let r = p("Dishonored_(2017)_Issue_#1");
        assert_eq!((r.series.as_str(), r.number.as_deref(), r.year), ("Dishonored", Some("1"), Some(2017)));

        let r = p("Midnight_Spider-Man_(2026-)_#1");
        assert_eq!((r.series.as_str(), r.number.as_deref(), r.year), ("Midnight Spider-Man", Some("1"), Some(2026)));

        let r = p("Runaways v2 #01");
        assert_eq!((r.series.as_str(), r.number.as_deref(), r.volume), ("Runaways", Some("1"), Some(2)));

        let r = p("Runaways V3 01");
        assert_eq!((r.series.as_str(), r.number.as_deref(), r.volume), ("Runaways", Some("1"), Some(3)));

        let r = p("Outlast - The Murkoff Account 01");
        assert_eq!((r.series.as_str(), r.number.as_deref()), ("Outlast - The Murkoff Account", Some("1")));
    }

    #[test]
    fn collections_and_one_shots() {
        let r = p("Ghost Rider - Danny Ketch Epic Collection v01 - Vengeance Reborn (2023) (Digital) (dekabro-Empire)");
        assert_eq!(r.hint, Some(Kind::Collection));
        assert_eq!(r.series, "Ghost Rider - Danny Ketch Epic Collection v01 - Vengeance Reborn");
        assert_eq!(r.number, None);

        let r = p("Dishonored_(2017)_TPB");
        assert_eq!((r.hint, r.series.as_str()), (Some(Kind::Collection), "Dishonored"));

        let r = p("Outlast - The Murkoff Account 01-06");
        assert_eq!((r.range, r.series.as_str()), (Some((1, 6)), "Outlast - The Murkoff Account"));

        let r = p("FCBD Marvel - X-Men - Runaways (2006)");
        assert_eq!((r.hint, r.number.as_deref(), r.year), (Some(Kind::OneShot), None, Some(2006)));

        let r = p("Mystic Arcana - Sister Grimm");
        assert_eq!((r.hint, r.number.as_deref()), (None, None));

        let r = p("Avengers-Doctor Strange - Rise of the Darkhold (2018) (Digital) (Relic-Empire)");
        assert_eq!((r.series.as_str(), r.number.as_deref()), ("Avengers-Doctor Strange - Rise of the Darkhold", None));
    }

    #[test]
    fn french_names() {
        let s = |n: &str| stem(Path::new(n));
        assert_eq!(s("Les_Compagnons_du_cr_233_puscule_-_int_233_grale_Fran_231_ois_Bourgeon.pdf"),
            "Les_Compagnons_du_crépuscule_-_intégrale_François_Bourgeon");
        // The source name itself lost the space after the entity.
        assert_eq!(s("HS_-_L_39_Homme__224_la_Hache.pdf"), "HS_-_L'Homme_àla_Hache");
        assert_eq!(s("Comic_100_Bullets.cbz"), "Comic_100_Bullets");

        let r = p("Les Tours de Bois-Maury IntegraleT1 T01-05");
        assert_eq!((r.hint, r.range), (Some(Kind::Collection), Some((1, 5))));
        assert_eq!(r.series, "Les Tours de Bois-Maury");
        assert_eq!(p("HS - L'Homme à la Hache").hint, Some(Kind::OneShot));
    }

    #[test]
    fn folders_add_context() {
        let r = parse_path(Path::new(r"D:\C\Runaways\Runaways Vol 1\Runaways 01.cbr"));
        assert_eq!((r.series.as_str(), r.number.as_deref(), r.volume), ("Runaways", Some("1"), Some(1)));

        let r = parse_path(Path::new(r"D:\C\Outlast - The Murkoff Account 01-06\Outlast - The Murkoff Account 03.cbr"));
        assert_eq!((r.number.as_deref(), r.of), (Some("3"), Some(6)));
    }

    #[test]
    fn cleans_release_names() {
        let c = |s: &str| clean(Path::new(s));
        assert_eq!(c(r"D:\C\Dishonored 001 - The Wyrmwood Deceit (Digital) (comicsall.net).cbr.cbr"), "Dishonored 001 - The Wyrmwood Deceit");
        assert_eq!(c("Midnight_Spider-Man_(2026-)_#1.cbz"), "Midnight Spider-Man (2026-) #1");
        assert_eq!(c("[Group] (Digital).cbz"), "[Group] (Digital)");
    }
}
