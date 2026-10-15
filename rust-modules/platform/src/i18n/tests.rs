use super::*;

#[test]
fn formatting_follows_the_format_locale() {
    let regional = LocaleContext::resolve(Some("en-GB"), None);
    assert_eq!(regional.format_locale(), "en-GB");
    assert_eq!(regional.date(2026, 9, 27).as_deref(), Some("27/09/2026"));
    let cx = LocaleContext::resolve(Some("de_DE.UTF-8"), Some("24"));
    assert_eq!(cx.format_locale(), "de-DE");
    assert_eq!(cx.number(12345), "12.345");
    assert_eq!(cx.decimal(45, 1), "4,5");
    assert_eq!(cx.clock(), Clock::H24);
    assert_eq!(cx.date(2026, 9, 27).as_deref(), Some("27.09.2026"));
    for tag in ["garbage\r\nheader", "C", "POSIX"] {
        assert_eq!(LocaleContext::resolve(Some(tag), None).format_locale(), "en-US");
    }
}

#[test]
fn english_plurals_pick_one_only_for_one() {
    assert!(plural_one(1));
    for n in [0, 2, 5, 11, 21, -1] {
        assert!(!plural_one(n));
    }
}

#[test]
fn all_catalog_glyphs_exist_in_both_shipped_text_faces() {
    for name in ["appfont.ttf", "appfont-bold.ttf"] {
        let cov = plx_base::fontcov::shipped(name).as_ref().unwrap();
        for s in msg::CORPUS.iter().copied() {
            for ch in s.chars().filter(|c| !c.is_control()) {
                assert!(
                    cov.contains(ch as u32),
                    "{name}: missing {ch:?} U+{:04X}",
                    ch as u32
                );
            }
        }
    }
}
