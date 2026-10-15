//! **Settings** — the tab bar's last pill: where the library reads from, how the player shows
//! subtitles, how it plays a file, how it reads ahead of the picture, and what it is.
//!
//! One screen, three kinds of page ([`Page`]), each a `ui::table_screen::TableScreen` (the title
//! and copy in the narrative column, a `FormTable` in the content column):
//!
//! - **The root**: a section per area, each row a drill-in that reads out its current value. The
//!   Library section lists every folder and share with its status (`plx_data::library`), a Rescan
//!   button that reads the config file again, and a note saying where that file is — the library
//!   is configured by editing it, never on screen.
//! - **A picker** ([`Pick`]): one checked row per option. Choosing one applies it at once
//!   (`plx_data::settings`, which saves on a worker) and returns to the root on the row it came
//!   from; BACK or LEFT returns without changing anything.
//! - **A source** ([`Page::Source`]): a folder's or share's facts, and a button that checks it
//!   again (a share: mounts it again).
//!
//! While the root or a subtitle picker is up, the narrative column shows a sample caption drawn
//! the way the player draws one now (`appkit::player_hud::draw_caption_preview`), so a tone, size or
//! position is seen before a film is opened.
//!
//! The page is a tab page: UP from the first row reaches the tab bar ([`Screen::links`]), and its
//! own pill keeps the bar's focus memory ([`super::tabs::TabSeat`]).

use std::borrow::Cow;

use plx_data::library::{self, Origin, Place, SourceView, Status};
use plx_data::settings::{self, CacheSize, ReadAhead, ReadFactor, SkipInterval, SubtitlePosition, SubtitleSize, SubtitleTone};
use plx_machine::machine::{
    Canon, Cx, Delivery, Edge, Effects, Enter, EntryId, FocusKey, FocusTarget, Fx, GroupId, Handled, InputKind,
    InstanceId, Key, LogicalState, Machine, MachineId,
};
use plx_machine::present::Provenance;
use plx_platform::i18n::msg;
use plx_ui::form::{Activation, Form, FormId, FormSection, FormTable, RowKey, RowKind};
use plx_ui::route_screen::RouteLayout;
use plx_ui::screen::{
    DrawFrame, FocusSource, HitSource, Link, Part, RenderStrategy, Screen, ScreenEvent,
};
use plx_ui::table::Row;
use plx_ui::table_screen::{Header, TableScreen};
use plx_ui::{theme, Rect};

use super::registry::{AppArg, AppFx, ContentLike, ContentReq};

/// The table's focus group: the group a tab page opened from the bar is seated in.
const TABLE: GroupId = GroupId(0);
/// The preview card's corner.
const PREVIEW_RAD: f32 = 16.0;

/// **One setting a picker chooses.**
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pick {
    Tone,
    Size,
    Position,
    Skip,
    SubtitleLanguage,
    AudioLanguage,
    ReadAhead,
    CacheSize,
    ReadFactor,
}

impl Pick {
    pub const ALL: [Pick; 9] = [
        Pick::Tone,
        Pick::Size,
        Pick::Position,
        Pick::Skip,
        Pick::SubtitleLanguage,
        Pick::AudioLanguage,
        Pick::ReadAhead,
        Pick::CacheSize,
        Pick::ReadFactor,
    ];

    /// The row's name, and the picker's title.
    pub fn label(self) -> &'static str {
        match self {
            Pick::Tone => msg::widgets_tracks_color(),
            Pick::Size => msg::widgets_tracks_style_size(),
            Pick::Position => msg::widgets_tracks_style_position(),
            Pick::Skip => msg::settings_playback_skip_interval(),
            Pick::SubtitleLanguage => msg::settings_audio_subtitle_language(),
            Pick::AudioLanguage => msg::settings_audio_language(),
            Pick::ReadAhead => msg::settings_cache_read_ahead(),
            Pick::CacheSize => msg::settings_cache_size(),
            Pick::ReadFactor => msg::settings_cache_factor(),
        }
    }

    /// The picker's explanation, under its title.
    fn copy(self) -> &'static str {
        match self {
            Pick::Tone => msg::settings_subtitles_color_copy(),
            Pick::Size => msg::settings_playback_subtitle_size_copy(),
            Pick::Position => msg::settings_playback_subtitle_position_copy(),
            Pick::Skip => msg::settings_playback_skip_interval_copy(),
            Pick::SubtitleLanguage => msg::settings_audio_subtitle_language_copy(),
            Pick::AudioLanguage => msg::settings_audio_language_copy(),
            Pick::ReadAhead => msg::settings_cache_read_ahead_copy(),
            Pick::CacheSize => msg::settings_cache_size_copy(),
            Pick::ReadFactor => msg::settings_cache_factor_copy(),
        }
    }

    /// Whether this picker's choice shows in the subtitle preview.
    fn previews(self) -> bool {
        matches!(self, Pick::Tone | Pick::Size | Pick::Position)
    }

    /// Every option's name, in the order the picker lists them.
    pub fn options(self) -> Vec<String> {
        use plx_appkit::track_menu::{subtitle_position_label, subtitle_size_label, subtitle_tone_label};
        let languages = |first: &str| {
            std::iter::once(first.to_owned())
                .chain(settings::LANGUAGES.iter().map(|c| plx_data::metadata::languages::label(c).to_owned()))
                .collect()
        };
        match self {
            Pick::Tone => SubtitleTone::LADDER.iter().map(|&t| subtitle_tone_label(t).to_owned()).collect(),
            Pick::Size => SubtitleSize::LADDER.iter().map(|&s| subtitle_size_label(s).to_owned()).collect(),
            Pick::Position => SubtitlePosition::LADDER.iter().map(|&p| subtitle_position_label(p).to_owned()).collect(),
            Pick::Skip => SkipInterval::LADDER.iter().map(|&s| msg::settings_playback_skip_interval_seconds(s.seconds())).collect(),
            Pick::SubtitleLanguage => languages(msg::widgets_tracks_off()),
            Pick::AudioLanguage => languages(msg::settings_language_file_default()),
            Pick::ReadAhead => vec![
                msg::settings_cache_read_ahead_off().to_owned(),
                msg::settings_cache_read_ahead_network().to_owned(),
                msg::settings_cache_read_ahead_all().to_owned(),
            ],
            Pick::CacheSize => CacheSize::LADDER.iter().map(|&s| msg::settings_cache_size_mb(s.megabytes() as i64)).collect(),
            Pick::ReadFactor => ReadFactor::LADDER
                .iter()
                .map(|&f| f.multiple().map_or_else(|| msg::settings_cache_factor_unlimited().to_owned(), |m| msg::settings_cache_factor_times(i64::from(m))))
                .collect(),
        }
    }

    /// Which option is in force now.
    pub fn current(self) -> usize {
        let language = |code: String| settings::LANGUAGES.iter().position(|&c| c == code).map_or(0, |i| i + 1);
        match self {
            Pick::Tone => settings::subtitle_tone().index() as usize,
            Pick::Size => settings::subtitle_size().index() as usize,
            Pick::Position => settings::subtitle_position().index() as usize,
            Pick::Skip => settings::skip_interval().index() as usize,
            Pick::SubtitleLanguage => language(settings::subtitle_language()),
            Pick::AudioLanguage => language(settings::audio_language()),
            Pick::ReadAhead => settings::read_ahead().index() as usize,
            Pick::CacheSize => settings::cache_size().index() as usize,
            Pick::ReadFactor => settings::read_factor().index() as usize,
        }
    }

    /// Make option `i` the setting.
    pub fn apply(self, i: usize) {
        let ix = u8::try_from(i).unwrap_or(u8::MAX);
        let language = |i: usize| i.checked_sub(1).and_then(|i| settings::LANGUAGES.get(i)).copied().unwrap_or("");
        match self {
            Pick::Tone => settings::set_subtitle_tone(SubtitleTone::from_index(ix)),
            Pick::Size => settings::set_subtitle_size(SubtitleSize::from_index(ix)),
            Pick::Position => settings::set_subtitle_position(SubtitlePosition::from_index(ix)),
            Pick::Skip => settings::set_skip_interval(SkipInterval::from_index(ix)),
            Pick::SubtitleLanguage => settings::set_subtitle_language(language(i)),
            Pick::AudioLanguage => settings::set_audio_language(language(i)),
            Pick::ReadAhead => settings::set_read_ahead(ReadAhead::from_index(ix)),
            Pick::CacheSize => settings::set_cache_size(CacheSize::from_index(ix)),
            Pick::ReadFactor => settings::set_read_factor(ReadFactor::from_index(ix)),
        }
    }

    /// The current option's name — the root row's read-out.
    fn value(self) -> String {
        self.options().into_iter().nth(self.current()).unwrap_or_default()
    }
}

/// **A page of the screen** — the form's `Dest`: a drill-in pushes one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Page {
    Pick(Pick),
    /// The library source at this position in `library::sources`.
    Source(u32),
}

/// A row's identity: a drill-in, a picker's option, a library row, or a fact.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SetRow {
    Open(Pick),
    Option(u32),
    About(u32),
    Source(u32),
    Rescan,
    Fact(u32),
    Reconnect,
}

impl FormId for SetRow {
    fn key(&self) -> RowKey {
        RowKey(match self {
            SetRow::Rescan => 5,
            SetRow::Reconnect => 6,
            SetRow::Open(p) => 10 + Pick::ALL.iter().position(|q| q == p).unwrap_or(0) as u32,
            SetRow::Option(i) => 100 + i,
            SetRow::About(i) => 200 + i,
            SetRow::Source(i) => 300 + i,
            SetRow::Fact(i) => 400 + i,
        })
    }
}

/// What pressing a row does: drill-ins push their [`Page`] (the form's `Dest`), so an action is a
/// choice, a library command, or nothing at all.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Act {
    Choose(Pick, u32),
    Rescan,
    Reconnect(u32),
    Nothing,
}

type SettingsForm = Form<SetRow, Act, Page>;
type SettingsSection = FormSection<SetRow, Act, Page>;

/// A source's status, as its row reads it out.
fn status_text(view: &SourceView) -> &'static str {
    let share = matches!(view.source.place, Place::Share(_));
    match (&view.status, share) {
        (Status::Checking, false) => msg::settings_library_status_checking(),
        (Status::Checking, true) => msg::settings_library_status_connecting(),
        (Status::Ready(_), false) => msg::settings_library_status_ready(),
        (Status::Ready(_), true) => msg::settings_library_status_connected(),
        (Status::Missing, _) => msg::settings_library_status_missing(),
        (Status::Failed(_), _) => msg::settings_library_status_failed(),
    }
}

/// `path` with the home folder written `~`.
fn tilde(path: &std::path::Path) -> String {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    match home.as_deref().and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// What the library is read from now: the Library section's notes.
#[derive(Clone, Debug, Default)]
struct LibraryNotes {
    lines: Vec<String>,
}

impl LibraryNotes {
    fn now() -> Self {
        let origin = library::origin();
        let path = tilde(origin.config_path());
        let mut lines = vec![match origin {
            Origin::Env(_) => msg::settings_library_note_env(&path),
            Origin::File(_) => msg::settings_library_note(&path),
            Origin::Default(_) => msg::settings_library_note_default(&path),
        }];
        if let Some(first) = library::problems().first() {
            lines.push(msg::settings_library_problem(first));
        }
        Self { lines }
    }
}

/// The Library section: a row per source, Rescan, and where the config is.
fn library_section(sources: &[SourceView], notes: &LibraryNotes) -> SettingsSection {
    let rows = sources.iter().enumerate().fold(SettingsSection::new(msg::settings_library_section()), |s, (i, v)| {
        let i = i as u32;
        s.item(SetRow::Source(i), RowKind::Nav(Page::Source(i)), Act::Nothing, Row::new(v.source.name.clone()).value(status_text(v)))
    });
    let rows = rows.item(SetRow::Rescan, RowKind::Button, Act::Rescan, Row::new(msg::settings_library_rescan()));
    notes.lines.iter().fold(rows, |s, line| s.note(line.clone()))
}

/// The root: a section per area, each row reading out its value.
fn root_form(about: &[(String, String)], sources: &[SourceView], notes: &LibraryNotes) -> SettingsForm {
    let open = |s: SettingsSection, p: Pick| s.item(SetRow::Open(p), RowKind::Nav(Page::Pick(p)), Act::Nothing, Row::new(p.label()).value(p.value()));
    let subtitles = [Pick::Tone, Pick::Size, Pick::Position].into_iter().fold(FormSection::new(msg::settings_subtitles_section()), open);
    let playback = [Pick::Skip, Pick::SubtitleLanguage, Pick::AudioLanguage].into_iter().fold(FormSection::new(msg::settings_playback_section()), open);
    let cache = [Pick::ReadAhead, Pick::CacheSize, Pick::ReadFactor].into_iter().fold(FormSection::new(msg::settings_cache_section()), open);
    let about = about.iter().enumerate().fold(FormSection::new(msg::settings_about_section()), |s, (i, (label, value))| {
        s.item(SetRow::About(i as u32), RowKind::Button, Act::Nothing, Row::new(label.clone()).value(value.clone()))
    });
    Form::new().section(library_section(sources, notes)).section(subtitles).section(playback).section(cache).section(about)
}

/// A source's page: its facts, and the button that checks it again.
fn source_form(i: u32, view: &SourceView) -> SettingsForm {
    let mut facts: Vec<(&str, String)> = Vec::new();
    match &view.source.place {
        Place::Folder(path) => facts.push((msg::settings_library_location(), path.display().to_string())),
        Place::Share(share) => {
            facts.push((msg::settings_library_location(), share.uri.clone()));
            let user = share.user.clone().unwrap_or_else(|| msg::settings_library_user_anonymous().to_owned());
            facts.push((msg::settings_library_user(), user));
        }
    }
    facts.push((msg::settings_library_status(), status_text(view).to_owned()));
    match (&view.status, &view.source.place) {
        (Status::Ready(at), Place::Share(_)) => facts.push((msg::settings_library_mounted_at(), at.display().to_string())),
        (Status::Failed(why), _) => facts.push((msg::settings_library_reason(), why.clone())),
        _ => {}
    }
    let rows = facts.into_iter().enumerate().fold(SettingsSection::new(""), |s, (n, (label, value))| {
        s.item(SetRow::Fact(n as u32), RowKind::Button, Act::Nothing, Row::new(label).value(value))
    });
    let again = match view.source.place {
        Place::Share(_) => msg::settings_library_reconnect(),
        Place::Folder(_) => msg::settings_library_check(),
    };
    let busy = view.status == Status::Checking;
    Form::new().section(rows.item(SetRow::Reconnect, RowKind::Button, Act::Reconnect(i), Row::new(again)).disabled(busy))
}

/// A picker: one checked row per option.
fn pick_form(p: Pick) -> SettingsForm {
    let current = p.current() as u32;
    let rows = p.options().into_iter().enumerate().fold(FormSection::new(""), |s, (i, name)| {
        let i = i as u32;
        s.choice(SetRow::Option(i), Act::Choose(p, i), Row::new(name), |id| *id == SetRow::Option(current))
    });
    Form::new().section(rows)
}

/// The About section's facts: `(label, value)`.
fn about_facts() -> Vec<(String, String)> {
    let unknown = || "—".to_owned();
    vec![
        (msg::settings_about_version().to_owned(), plx_base::identity::version().to_owned()),
        (msg::settings_about_ffmpeg().to_owned(), plx_media::ff::library_version().unwrap_or_else(unknown)),
        (msg::settings_about_gstreamer().to_owned(), plx_media::player::gst_sink::gst_version().unwrap_or_else(unknown)),
    ]
}

pub struct SettingsScreen {
    entry: EntryId,
    instance: InstanceId,
    /// The page showing, `None` on the root.
    page: Option<Page>,
    form: FormTable<SetRow, Act, Page>,
    /// The settings and library revisions the form was last built at: a change made elsewhere (the
    /// player's Style page, a share connecting) re-reads the values.
    built_at: Option<(u64, u64)>,
    about: Vec<(String, String)>,
    /// The library's sources and notes as last read, so a page is built from one snapshot.
    sources: Vec<SourceView>,
    notes: LibraryNotes,
    tab_seat: super::tabs::TabSeat,
}

impl SettingsScreen {
    pub fn new(entry: EntryId, instance: InstanceId) -> Self {
        Self {
            entry,
            instance,
            page: None,
            form: FormTable::new(crate::registry::BAND),
            built_at: None,
            about: Vec::new(),
            sources: Vec::new(),
            notes: LibraryNotes::default(),
            tab_seat: Default::default(),
        }
    }

    /// The page showing.
    pub fn page(&self) -> Option<Page> {
        self.page
    }

    fn header(&self) -> Header<'_> {
        let layout = RouteLayout::screen();
        match self.page {
            None => Header::new(layout, None, msg::settings_title(), msg::settings_root_copy()),
            Some(Page::Pick(p)) => Header::new(layout, Some(msg::settings_title()), p.label(), p.copy()),
            Some(Page::Source(i)) => {
                let Some(v) = self.sources.get(i as usize) else {
                    return Header::new(layout, Some(msg::settings_title()), msg::settings_library_section(), "");
                };
                let copy = match v.source.place {
                    Place::Folder(_) => msg::settings_library_folder_copy(),
                    Place::Share(_) => msg::settings_library_share_copy(),
                };
                Header::new(layout, Some(msg::settings_title()), &v.source.name, copy)
            }
        }
    }

    /// Whether the subtitle preview stands under the copy: on the root and the subtitle pickers.
    fn previews(&self) -> bool {
        match self.page {
            None => true,
            Some(Page::Pick(p)) => p.previews(),
            Some(Page::Source(_)) => false,
        }
    }

    /// Generic over the host only so `focusable_via_view!` can name it; the table is host-free.
    fn view<H: ContentLike>(&self) -> TableScreen<'_> {
        TableScreen::new(self.header(), &self.form.table, TABLE, self.entry).keyed(&self.form).uncommitted(true)
    }

    /// Build the page showing, keeping the cursor on `keep` when it is one of its rows.
    fn build(&mut self, keep: Option<SetRow>) {
        self.sources = library::sources();
        self.notes = LibraryNotes::now();
        // a source a rescan removed has no page left: back to the root, on Rescan
        if let Some(Page::Source(i)) = self.page {
            if i as usize >= self.sources.len() {
                self.page = None;
            }
        }
        let form = match self.page {
            None => root_form(&self.about, &self.sources, &self.notes),
            Some(Page::Pick(p)) => pick_form(p),
            Some(Page::Source(i)) => source_form(i, &self.sources[i as usize]),
        };
        self.form.set(form, keep.as_ref());
        self.built_at = Some(Self::revisions());
    }

    fn revisions() -> (u64, u64) {
        (settings::revision(), library::revision())
    }

    /// Show `page` with the focus on `row`.
    fn go<H: ContentLike>(&mut self, page: Option<Page>, row: SetRow, fx: &mut Effects<'_, H>) {
        self.page = page;
        self.build(Some(row));
        let focus = FocusKey { entry: self.entry, elem: row.key().0 };
        fx.push(Fx::Deliver(MachineId::Instance(self.instance), Delivery::Screen(ScreenEvent::Enter(Enter::Fresh { focus: FocusTarget::Elem(focus) }))));
        fx.invalidate(Provenance::Input);
    }

    /// Back to the root, on the row of the page that was open.
    fn back<H: ContentLike>(&mut self, fx: &mut Effects<'_, H>) -> bool {
        let row = match self.page {
            None => return false,
            Some(Page::Pick(p)) => SetRow::Open(p),
            Some(Page::Source(i)) => SetRow::Source(i),
        };
        self.go(None, row, fx);
        true
    }

    fn activate<H: ContentLike>(&mut self, elem: u32, fx: &mut Effects<'_, H>) {
        if elem >= plx_ui::dispatch::STRIP_BASE {
            fx.push(Fx::App(AppFx::Content(ContentReq::SelectTab((elem - plx_ui::dispatch::STRIP_BASE) as usize))));
            return;
        }
        let Some(i) = self.form.index_of_key(RowKey(elem)) else { return };
        match self.form.activate(i) {
            Some(Activation::Push(Page::Pick(p))) => self.go(Some(Page::Pick(p)), SetRow::Option(p.current() as u32), fx),
            Some(Activation::Push(Page::Source(i))) => self.go(Some(Page::Source(i)), SetRow::Reconnect, fx),
            Some(Activation::Action(Act::Choose(p, i))) => {
                p.apply(i as usize);
                self.go(None, SetRow::Open(p), fx);
            }
            // both land through `library::revision`, which the tick rebuilds on
            Some(Activation::Action(Act::Rescan)) => library::rescan(),
            Some(Activation::Action(Act::Reconnect(i))) => library::reconnect(i as usize),
            Some(Activation::Action(Act::Nothing)) | None => {}
        }
    }
}

plx_ui::focusable_via_view!(SettingsScreen, H: [ContentLike], view);

impl LogicalState for SettingsScreen {
    fn write(&self, c: &mut Canon) {
        c.u32(self.entry.0)
            .u32(match self.page {
                None => 0,
                Some(Page::Pick(p)) => 1 + Pick::ALL.iter().position(|&q| q == p).unwrap_or(0) as u32,
                Some(Page::Source(i)) => 100 + i,
            })
            .u32(self.form.table.sel as u32);
    }
    fn probe(&self, out: &mut String) {
        out.push_str(&format!("settings page={:?}", self.page));
    }
}

impl<H: ContentLike> Machine<H> for SettingsScreen {
    type Ev = ScreenEvent<H>;
    fn step(&mut self, ev: &Self::Ev, cx: &Cx<'_, H>, fx: &mut Effects<'_, H>) -> Handled {
        self.tab_seat.on(&AppArg::Settings, self.entry, self.instance, ev, cx, fx);
        match ev {
            ScreenEvent::Mount => {
                self.about = about_facts();
                self.build(None);
                Handled::Yes
            }
            ScreenEvent::Tick(tick) => {
                if self.built_at != Some(Self::revisions()) {
                    let keep = self.form.selected_id().copied();
                    self.build(keep);
                    fx.invalidate(Provenance::Landing(fx.from()));
                }
                self.form.table.sel = cx
                    .focus
                    .current
                    .filter(|k| k.entry == self.entry)
                    .and_then(|k| self.form.index_of_key(RowKey(k.elem)))
                    .map_or(self.form.table.sel, |i| i as i32);
                self.form.table.update(tick.dt(), self.header().layout.sectioned_table().h);
                Handled::Yes
            }
            ScreenEvent::FocusMoved { to, .. } => {
                if let Some(i) = self.form.index_of_key(RowKey(to.elem)) {
                    self.form.table.sel = i as i32;
                }
                Handled::No
            }
            ScreenEvent::Activate(elem) => {
                self.activate(*elem, fx);
                Handled::Yes
            }
            ScreenEvent::PressCommit(_) => {
                if let Some(k) = cx.focus.current.filter(|k| k.entry == self.entry) {
                    self.activate(k.elem, fx);
                }
                Handled::Yes
            }
            ScreenEvent::Input(input) => {
                let InputKind::Key { key, edge: Edge::Down, .. } = input.kind else { return Handled::No };
                // BACK and LEFT leave a picker for the root; on the root they are the container's.
                let on_table = cx.focus.current.is_some_and(|k| k.entry == self.entry && k.elem < plx_ui::dispatch::STRIP_BASE);
                match key {
                    Key::Back | Key::Left if on_table && self.back(fx) => Handled::Yes,
                    _ => Handled::No,
                }
            }
            _ => Handled::No,
        }
    }
}

impl<H: ContentLike> Screen<H> for SettingsScreen {
    fn name(&self) -> &'static str {
        super::registry::word::SETTINGS
    }
    fn state(&self) -> &dyn LogicalState {
        self
    }
    fn crumb(&self, _cx: &Cx<'_, H>) -> Option<Cow<'_, str>> {
        None
    }
    fn prepare(&mut self, _budget: &mut plx_ui::frame::Budget, _cx: &Cx<'_, H>) {}
    /// DOWN from the tab bar lands on the first row, and UP from the first row reaches the bar.
    fn links(&self, out: &mut Vec<Link>) {
        use plx_ui::screen::Dir;
        let strip = plx_ui::containers::tabs::STRIP;
        out.push(Link { from: strip, dir: Dir::Down, to: TABLE });
        out.push(Link { from: TABLE, dir: Dir::Up, to: strip });
    }
    fn draw(&mut self, f: &mut DrawFrame<'_, '_, H>) {
        let p = f.painter.alpha(f.page_alpha);
        p.rrect(Rect::FULL, 0.0, 0.0, theme::SURFACE_APP);
        if self.previews() {
            // the preview stands at the foot of the narrative column, under the copy
            let col = self.header().layout.narrative;
            let h = col.w * 9.0 / 16.0;
            let card = Rect::new(col.x, col.y + col.h - h, col.w, h);
            p.rect(card, PREVIEW_RAD, theme::PANEL_TOP, theme::PANEL_BOT, 0.0);
            plx_appkit::player_hud::draw_caption_preview(p, card, msg::settings_subtitles_preview());
        }
        let mut view = self.view::<H>();
        Part::draw(&mut view, f, Rect::FULL);
    }
    fn render(&self) -> RenderStrategy {
        RenderStrategy::Page
    }
    fn focus_source(&self) -> FocusSource {
        FocusSource::Engine
    }
    fn hit_source(&self) -> HitSource {
        HitSource::Engine
    }
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_picker_lists_its_options_and_marks_one_current() {
        for p in Pick::ALL {
            let options = p.options();
            assert!(options.len() >= 2, "{p:?}");
            assert!(p.current() < options.len(), "{p:?}");
            assert!(options.iter().all(|o| !o.is_empty()), "{p:?}: {options:?}");
        }
    }

    #[test]
    fn choosing_an_option_makes_it_the_setting() {
        let _g = plx_base::testlock::serial();
        let dir = plx_base::testscratch::process_dir("settings-screen");
        plx_base::paths::redirect_persistent_state_root_for_test(Some(dir.join("state")));
        settings::load();
        for p in Pick::ALL {
            let before = p.current();
            let other = (before + 1) % p.options().len();
            p.apply(other);
            assert_eq!(p.current(), other, "{p:?}");
            p.apply(before);
            assert_eq!(p.current(), before, "{p:?}");
        }
        Pick::SubtitleLanguage.apply(1);
        assert_eq!(settings::subtitle_language(), settings::LANGUAGES[0]);
        Pick::SubtitleLanguage.apply(0);
        assert_eq!(settings::subtitle_language(), "", "the first option is Off");
        plx_base::paths::redirect_persistent_state_root_for_test(None);
    }

    #[test]
    fn the_root_reads_out_every_settings_value_and_a_picker_checks_the_current_option() {
        let _g = plx_base::testlock::serial();
        let mut t: FormTable<SetRow, Act, Page> = FormTable::new(crate::registry::BAND);
        t.set(root_form(&[("Version".into(), "1.0".into())], &[], &LibraryNotes::default()), None);
        for p in Pick::ALL {
            let i = t.index_of(&SetRow::Open(p)).unwrap_or_else(|| panic!("{p:?} has a row"));
            assert_eq!(t.activate(i), Some(Activation::Push(Page::Pick(p))), "{p:?} drills in");
        }
        t.set(pick_form(Pick::Size), None);
        let current = Pick::Size.current() as u32;
        let i = t.index_of(&SetRow::Option(current)).unwrap();
        assert_eq!(t.activate(i), Some(Activation::Action(Act::Choose(Pick::Size, current))));
    }

    fn share_view(status: Status) -> SourceView {
        let share = plx_data::library::Share { uri: "smb://nas/media".into(), user: None, domain: None, password: None };
        SourceView { source: plx_data::library::Source { name: "NAS".into(), place: Place::Share(share) }, status }
    }

    #[test]
    fn the_library_lists_each_source_with_its_status_then_rescan() {
        let folder = SourceView {
            source: plx_data::library::Source { name: "Films".into(), place: Place::Folder("/srv/Films".into()) },
            status: Status::Ready("/srv/Films".into()),
        };
        let sources = [folder, share_view(Status::Failed("Connection refused".into()))];
        let notes = LibraryNotes { lines: vec!["Folders and shares are listed in ~/x.".into()] };
        let mut t: FormTable<SetRow, Act, Page> = FormTable::new(crate::registry::BAND);
        t.set(root_form(&[], &sources, &notes), None);
        let films = t.index_of(&SetRow::Source(0)).unwrap();
        assert!(t.index_of(&SetRow::Source(1)).unwrap() > films);
        assert_eq!(t.activate(films), Some(Activation::Push(Page::Source(0))));
        assert_eq!(status_text(&sources[0]), msg::settings_library_status_ready());
        assert_eq!(status_text(&sources[1]), msg::settings_library_status_failed());
        let rescan = t.index_of(&SetRow::Rescan).unwrap();
        assert_eq!(t.activate(rescan), Some(Activation::Action(Act::Rescan)));
        assert!(t.index_of(&SetRow::Open(Pick::Tone)).unwrap() > rescan, "Library comes first");
    }

    #[test]
    fn a_share_page_says_why_it_failed_and_offers_reconnect_unless_it_is_busy() {
        let mut t: FormTable<SetRow, Act, Page> = FormTable::new(crate::registry::BAND);
        t.set(source_form(1, &share_view(Status::Failed("Connection refused".into()))), None);
        let reconnect = t.index_of(&SetRow::Reconnect).unwrap();
        assert_eq!(t.activate(reconnect), Some(Activation::Action(Act::Reconnect(1))));
        assert!(t.index_of(&SetRow::Fact(3)).is_some(), "location, user, status, reason");

        t.set(source_form(1, &share_view(Status::Checking)), None);
        let reconnect = t.index_of(&SetRow::Reconnect).unwrap();
        assert_eq!(t.activate(reconnect), None, "no second mount while one is running");
    }
}
