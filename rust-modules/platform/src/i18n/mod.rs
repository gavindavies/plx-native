//! Immutable per-launch localization. The interface is English only; its strings live in
//! `locales/en/*.json` and are compiled into typed accessors (`msg::*`). Numbers, dates and the
//! clock follow the process locale (`LC_ALL` / `LC_TIME` / `LANG`, or `PITV_FORMAT_LOCALE`).
use icu_decimal::{input::Decimal, DecimalFormatter};
use icu_locale::Locale;
use std::sync::OnceLock;

/// The interface language's tag: the subtitle/audio preference falls back to it.
pub const LANGUAGE: &str = "en";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Clock {
    Locale,
    H12,
    H24,
}

/// How numbers, dates and times are written.
pub struct LocaleContext {
    format_locale: String,
    clock: Clock,
    decimal: DecimalFormatter,
    date: icu_datetime::FixedCalendarDateTimeFormatter<
        icu_calendar::Gregorian,
        icu_datetime::fieldsets::YMD,
    >,
}
impl LocaleContext {
    /// `fmt` is a locale tag for formatting (normalized here); `None` is `en-US`.
    pub fn resolve(fmt: Option<&str>, clock: Option<&str>) -> Self {
        let format_locale = fmt.and_then(normalize).unwrap_or_else(|| "en-US".into());
        let locale = format_locale.parse::<Locale>().expect("validated locale");
        let decimal = DecimalFormatter::try_new(locale.clone().into(), Default::default())
            .unwrap_or_else(|_| {
                DecimalFormatter::try_new(Default::default(), Default::default())
                    .expect("compiled English number data")
            });
        let fields = || {
            icu_datetime::fieldsets::YMD::short()
                .with_year_style(icu_datetime::options::YearStyle::Full)
        };
        let date = icu_datetime::FixedCalendarDateTimeFormatter::try_new(locale.into(), fields())
            .unwrap_or_else(|_| {
                icu_datetime::FixedCalendarDateTimeFormatter::try_new(Default::default(), fields())
                    .expect("compiled English date data")
            });
        Self {
            format_locale,
            clock: match clock {
                Some("12") => Clock::H12,
                Some("24") => Clock::H24,
                _ => Clock::Locale,
            },
            decimal,
            date,
        }
    }
    pub fn format_locale(&self) -> &str {
        &self.format_locale
    }
    pub fn clock(&self) -> Clock {
        self.clock
    }
    pub fn number(&self, n: i64) -> String {
        self.decimal.format(&Decimal::from(n)).to_string()
    }
    pub fn decimal(&self, n: i64, scale: i16) -> String {
        let mut d = Decimal::from(n);
        d.multiply_pow10(-scale);
        d.pad_end(-scale);
        self.decimal.format(&d).to_string()
    }
    pub fn date(&self, year: i32, month: u8, day: u8) -> Option<String> {
        let d = icu_calendar::Date::try_new_gregorian(year, month, day).ok()?;
        Some(self.date.format(&d).to_string())
    }
}

/// A POSIX or BCP-47 locale string as a canonical BCP-47 tag, or `None` for `C`/`POSIX`, control
/// bytes and anything that does not parse.
pub fn normalize(s: &str) -> Option<String> {
    if !s.is_ascii() || s.bytes().any(|b| b.is_ascii_control()) {
        return None;
    }
    let base = s.trim().split(['.', '@']).next()?.replace('_', "-");
    if base.eq_ignore_ascii_case("C") || base.eq_ignore_ascii_case("POSIX") {
        return None;
    }
    let l = base.parse::<Locale>().ok()?;
    (l.id.language.as_str() != "und").then(|| l.to_string())
}

static CURRENT: OnceLock<LocaleContext> = OnceLock::new();

pub fn current() -> &'static LocaleContext {
    #[cfg(any(test, feature = "test-support"))]
    {
        CURRENT.get_or_init(|| LocaleContext::resolve(None, None))
    }
    #[cfg(not(any(test, feature = "test-support")))]
    {
        CURRENT.get().expect("locale initialized before any screen")
    }
}

/// Resolve the formatting locale once, at boot. Always compiled: the application's boot calls it
/// from code that is `cfg(not(test))`, which can build against this crate WITH `test-support`.
pub fn initialize() {
    let env = ["PITV_FORMAT_LOCALE", "LC_ALL", "LC_TIME", "LANG"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|s| !s.is_empty()));
    let cx = LocaleContext::resolve(env.as_deref(), None);
    plx_base::eventlog::log(&format!("locale: format={} clock={:?}", cx.format_locale(), cx.clock()));
    if CURRENT.set(cx).is_err() {
        plx_base::eventlog::log("locale: initialization already completed");
    }
}

/// English's cardinal plural rule, which is all the catalog's plural messages need.
fn plural_one(n: i64) -> bool {
    n == 1
}

// Every generated key has str and CStr variants; not every consumer needs both. Keep the
// allowance on this generated API rather than suppressing handwritten warnings.
#[allow(dead_code)]
pub mod msg {
    include!(concat!(env!("OUT_DIR"), "/messages.rs"));
}
#[cfg(test)]
#[allow(dead_code)]
#[path = "../../build_support/catalog.rs"]
mod catalog_tests;
#[cfg(test)]
mod tests;
