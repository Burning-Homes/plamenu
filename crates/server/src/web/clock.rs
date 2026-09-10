//! Viewer-local timestamp rendering for the web client.
//!
//! Every human-facing date or time in the web UI (including the admin console)
//! goes through [`ViewerClock`]. It carries the two things a reading depends on
//! — the viewer's zone and their locale — so a renderer takes one value instead
//! of threading a `(zone, locale)` pair through every call.
//!
//! Three rules shape the output:
//!
//! * the **body** of a timestamp is a plain wall-clock reading in the viewer's
//!   zone, with no offset suffix: the zone is stated once per surface (see the
//!   zone chip) and again in the tooltip, so repeating `UTC+02:00` on every row
//!   is noise;
//! * the **tooltip** carries both readings — viewer-local *and* UTC — so UTC is
//!   always one hover away and nobody has to do arithmetic;
//! * the **`datetime=` attribute** stays RFC 3339 UTC, always. It is
//!   machine-readable output: `app.js` parses it to upgrade the body to a
//!   relative label, and scrapers read it. Only human-facing text is localised.
//!
//! The offset shown is the zone's *at that instant*, not today's, so a
//! timestamp from the far side of a DST switch reads correctly.
//!
//! Scheduled posts use Postgres for wall-clock conversion. Event input uses
//! Jiff so nonexistent or ambiguous local times can be rejected before publishing.

use fluent_bundle::FluentArgs;
use maud::{Markup, html};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::time_zones;
use crate::web::i18n::Locale;

/// Everything needed to render a timestamp for one viewer: the zone their
/// wall-clock readings are in, its identifier (for `AT TIME ZONE` and for
/// display), and the catalog their month names and field order come from.
#[derive(Clone, Debug)]
pub struct ViewerClock {
    zone: jiff::tz::TimeZone,
    /// Always an entry from [`time_zones::ZONES`], never an arbitrary string —
    /// this value is interpolated into `AT TIME ZONE` queries, and the
    /// `&'static str` is what makes that safe by construction.
    name: &'static str,
    locale: Locale,
}

impl ViewerClock {
    /// The clock for a viewer with no stored zone: anonymous visitors, and
    /// signed-in users who never touched the preference.
    #[must_use]
    pub fn utc(locale: Locale) -> Self {
        Self {
            zone: jiff::tz::TimeZone::UTC,
            name: "UTC",
            locale,
        }
    }

    /// The clock for a stored `users.time_zone`. An unknown or unset
    /// identifier falls back to UTC, mirroring [`time_zones::normalize`]'s
    /// leniency — a zone that has since left the inventory must not 500 a page.
    #[must_use]
    pub fn of(name: Option<&str>, locale: Locale) -> Self {
        let Some(name) = name.and_then(time_zones::normalize) else {
            return Self::utc(locale);
        };
        Self {
            zone: time_zones::tz(name),
            name,
            locale,
        }
    }

    /// The identifier, for [`plamenu_db::tz`]'s `AT TIME ZONE` conversions.
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The viewer's locale, so a caller holding a clock needn't also carry the
    /// locale to render the surrounding copy.
    #[must_use]
    pub fn locale(&self) -> Locale {
        self.locale
    }

    /// Whether this viewer reads in UTC — the tooltip has nothing to add for
    /// them, so it stays single-reading.
    #[must_use]
    pub fn is_utc(&self) -> bool {
        self.name == "UTC"
    }

    /// The zone as `jiff` sees it, for the few callers still taking a zone.
    #[must_use]
    pub fn zone(&self) -> &jiff::tz::TimeZone {
        &self.zone
    }

    fn zoned(&self, at: OffsetDateTime) -> Option<jiff::Zoned> {
        let instant = jiff::Timestamp::from_second(at.unix_timestamp()).ok()?;
        Some(instant.to_zoned(self.zone.clone()))
    }

    fn month_abbr(&self, month: i8) -> String {
        self.locale
            .text(&format!("month-abbr-{}", month.clamp(1, 12)))
    }

    /// "Jul 3, 2026, 14:05" — the reading, no offset noise.
    #[must_use]
    pub fn stamp(&self, at: OffsetDateTime) -> String {
        let Some(zoned) = self.zoned(at) else {
            return at.to_string();
        };
        self.wall_clock(&zoned)
    }

    /// [`ViewerClock::stamp`] for a value that arrives as RFC 3339 text (the
    /// shape `entities` stores). Falls back to the raw string on a parse
    /// failure rather than dropping the value.
    #[must_use]
    pub fn stamp_iso(&self, iso: &str) -> String {
        match parse_iso(iso) {
            Some(at) => self.stamp(at),
            None => iso.to_owned(),
        }
    }

    /// "Jul 3, 2026" — for surfaces where day resolution is enough.
    #[must_use]
    pub fn date(&self, at: OffsetDateTime) -> String {
        let Some(zoned) = self.zoned(at) else {
            return at.to_string();
        };
        let mut args = FluentArgs::new();
        args.set("month", self.month_abbr(zoned.month()));
        args.set("day", zoned.day());
        args.set("year", zoned.year());
        self.locale.text_with("datetime-date", &args)
    }

    /// [`ViewerClock::date`] from RFC 3339 text.
    #[must_use]
    pub fn date_iso(&self, iso: &str) -> String {
        match parse_iso(iso) {
            Some(at) => self.date(at),
            None => iso.to_owned(),
        }
    }

    /// "Jan 2026" — the profile join date.
    #[must_use]
    pub fn month_year_iso(&self, iso: &str) -> String {
        let Some(at) = parse_iso(iso) else {
            return iso.to_owned();
        };
        let Some(zoned) = self.zoned(at) else {
            return iso.to_owned();
        };
        let mut args = FluentArgs::new();
        args.set("month", self.month_abbr(zoned.month()));
        args.set("year", zoned.year());
        self.locale.text_with("datetime-month-year", &args)
    }

    /// The dual-reading tooltip: the viewer's wall clock with the zone and the
    /// delta in force *at that instant*, then the same moment in UTC.
    ///
    /// The UTC half is spelled as a bare clock when both readings fall on the
    /// same calendar day, and as a full stamp when they don't — a viewer at
    /// UTC+13 reading a timestamp near their midnight needs the date, or
    /// "· 22:30 UTC" silently names the wrong day.
    ///
    /// A UTC viewer gets a single reading; there is no second one to give.
    #[must_use]
    pub fn tooltip(&self, at: OffsetDateTime) -> String {
        let Some(local) = self.zoned(at) else {
            return at.to_string();
        };
        let mut args = FluentArgs::new();
        args.set("local", self.wall_clock(&local));
        if self.is_utc() {
            return self.locale.text_with("datetime-tooltip-utc", &args);
        }
        let utc = local.with_time_zone(jiff::tz::TimeZone::UTC);
        args.set(
            "zone",
            zone_with_offset(self.name, local.offset().seconds()),
        );
        args.set(
            "utc",
            if utc.date() == local.date() {
                clock_of(&utc)
            } else {
                self.wall_clock(&utc)
            },
        );
        self.locale.text_with("datetime-tooltip", &args)
    }

    /// [`ViewerClock::tooltip`] from RFC 3339 text.
    #[must_use]
    pub fn tooltip_iso(&self, iso: &str) -> String {
        match parse_iso(iso) {
            Some(at) => self.tooltip(at),
            None => iso.to_owned(),
        }
    }

    /// The canonical timestamp element, and the call every surface should
    /// make: a UTC `datetime` attribute for machines, the dual tooltip on
    /// `title`, and the viewer-local reading as the body.
    ///
    /// `app.js` may replace the body with a relative label ("3h"); because the
    /// server always sets `title` here, it never has to synthesise one.
    #[must_use]
    pub fn element(&self, at: OffsetDateTime) -> Markup {
        self.time_element(at, &self.stamp(at), false)
    }

    /// [`ViewerClock::element`] whose body is already a relative label ("3h")
    /// — what a timeline row shows before `app.js` refreshes it, and what it
    /// shows forever without scripting. The tooltip still carries both full
    /// readings, so the coarse body is never the only thing on offer.
    #[must_use]
    pub fn element_relative(&self, at: OffsetDateTime, label: &str) -> Markup {
        self.time_element(at, label, false)
    }

    /// [`ViewerClock::element_relative`] from RFC 3339 text.
    #[must_use]
    pub fn element_relative_iso(&self, iso: &str, label: &str) -> Markup {
        if let Some(at) = parse_iso(iso) {
            self.element_relative(at, label)
        } else {
            html! { time { (label) } }
        }
    }

    fn time_element(&self, at: OffsetDateTime, body: &str, absolute: bool) -> Markup {
        html! {
            time datetime=(rfc3339(at)) title=(self.tooltip(at)) data-absolute[absolute] {
                (body)
            }
        }
    }

    /// [`ViewerClock::element`] from RFC 3339 text.
    #[must_use]
    pub fn element_iso(&self, iso: &str) -> Markup {
        if let Some(at) = parse_iso(iso) {
            self.element(at)
        } else {
            html! { time { (iso) } }
        }
    }

    /// [`ViewerClock::element`] that keeps its absolute body: `data-absolute`
    /// tells `app.js` not to swap in a relative label. For surfaces where the
    /// exact reading is the point (edit history, scheduled queues).
    #[must_use]
    pub fn element_absolute(&self, at: OffsetDateTime) -> Markup {
        self.time_element(at, &self.stamp(at), true)
    }

    /// [`ViewerClock::element_absolute`] from RFC 3339 text.
    #[must_use]
    pub fn element_absolute_iso(&self, iso: &str) -> Markup {
        if let Some(at) = parse_iso(iso) {
            self.element_absolute(at)
        } else {
            html! { time { (iso) } }
        }
    }

    /// A day-resolution [`ViewerClock::element_absolute`]: the body is just the
    /// date, but the tooltip still carries the full instant in both readings —
    /// which is how a viewer near midnight can tell why a row landed on the
    /// day it did.
    #[must_use]
    pub fn element_date(&self, at: OffsetDateTime) -> Markup {
        self.time_element(at, &self.date(at), true)
    }

    /// A `datetime-local` input value ("2026-07-03T14:05") in this zone — what
    /// the browser's picker expects, and what a submitted wall-clock reading
    /// round-trips back to.
    #[must_use]
    pub fn input_value(&self, at: OffsetDateTime) -> String {
        let Some(zoned) = self.zoned(at) else {
            return String::new();
        };
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}",
            zoned.year(),
            zoned.month(),
            zoned.day(),
            zoned.hour(),
            zoned.minute()
        )
    }

    /// "Europe/Berlin (UTC+02:00)" — for hints and the zone chip. Plain "UTC"
    /// stays bare. The delta is today's, which is what a *standing* statement
    /// about the viewer's zone should say.
    #[must_use]
    pub fn label(&self) -> String {
        zone_with_offset(self.name, time_zones::current_offset_seconds(&self.zone))
    }

    /// "Jul 3, 2026, 14:05" from an already-zoned value.
    fn wall_clock(&self, zoned: &jiff::Zoned) -> String {
        let mut args = FluentArgs::new();
        args.set("month", self.month_abbr(zoned.month()));
        args.set("day", zoned.day());
        args.set("year", zoned.year());
        args.set("clock", clock_of(zoned));
        self.locale.text_with("datetime-wall-clock", &args)
    }
}

/// The standing "which zone am I reading?" statement (R2), for pages dense in
/// timestamps or carrying a time input: the audit log, the reports index, the
/// announcement and scheduled-post forms, sessions, applications, export.
///
/// Deliberately *not* for timelines — the per-timestamp tooltip already covers
/// those, and a persistent chip there is clutter.
#[must_use]
pub fn zone_chip(clock: &ViewerClock) -> Markup {
    let locale = clock.locale();
    let mut args = FluentArgs::new();
    args.set("zone", clock.label());
    html! {
        p.zone-chip {
            span.zone-chip__icon aria-hidden="true" { "⏱" }
            span { (locale.text_with("datetime-zone-chip", &args)) }
            " · "
            a href="/settings/preferences" { (locale.text("datetime-zone-chip-change")) }
        }
    }
}

/// The zone inventory as `(identifier, "(UTC+02:00) Europe/Berlin")` pairs,
/// ordered west to east by current offset so the list reads like a map and the
/// exact delta is visible before choosing. The label is deliberately not
/// localised: IANA identifiers are the same in every language, and the offset
/// is a number.
///
/// Shared by the preferences dropdown and the sign-up form, so a zone offered
/// in one is offered in the other.
#[must_use]
pub fn zone_options() -> Vec<(&'static str, String)> {
    let mut zones: Vec<(&'static str, i32)> = time_zones::ZONES
        .iter()
        .map(|name| {
            (
                *name,
                time_zones::current_offset_seconds(&time_zones::tz(name)),
            )
        })
        .collect();
    zones.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(b.0)));
    zones
        .into_iter()
        .map(|(name, seconds)| {
            (
                name,
                format!("({}) {name}", time_zones::offset_label(seconds)),
            )
        })
        .collect()
}

/// Zone labels at the entered event date, including seasonal offset changes.
pub fn event_zone_options(start: &str) -> Vec<(&'static str, String)> {
    let Ok(local) = start.parse::<jiff::civil::DateTime>() else {
        return zone_options();
    };
    time_zones::ZONES
        .iter()
        .map(|&name| {
            let zone = time_zones::tz(name);
            let seconds = zone.to_ambiguous_zoned(local).compatible().map_or_else(
                |_| time_zones::current_offset_seconds(&zone),
                |z| z.offset().seconds(),
            );
            (
                name,
                format!("({}) {name}", time_zones::offset_label(seconds)),
            )
        })
        .collect()
}

/// Resolve an event's local time in the selected zone without silently moving it
/// through a clock change. Both skipped and repeated readings need correction.
pub(super) fn event_instant(
    raw: &str,
    zone: &str,
    locale: Locale,
) -> Result<OffsetDateTime, crate::error::ApiError> {
    let invalid =
        || crate::error::ApiError::Unprocessable(locale.text("compose-event-invalid-time"));
    let local = raw
        .parse::<jiff::civil::DateTime>()
        .map_err(|_| invalid())?;
    let instant = time_zones::tz(zone)
        .to_ambiguous_zoned(local)
        .unambiguous()
        .map_err(|_| {
            crate::error::ApiError::Unprocessable(locale.text("compose-event-clock-change"))
        })?;
    OffsetDateTime::from_unix_timestamp(instant.timestamp().as_second()).map_err(|_| invalid())
}

/// Parses a `datetime-local` submission (`YYYY-MM-DDTHH:MM`, seconds
/// optional) into the wall-clock reading it spells — the inverse of
/// [`ViewerClock::input_value`], and the one implementation both the composer's
/// Schedule field and the admin announcement form use.
///
/// The result is deliberately a `PrimitiveDateTime`: a wall-clock reading is
/// not yet an instant. Resolving it needs a zone *and* the IANA rules, which
/// is [`plamenu_db::tz::local_to_utc`]'s job — this only reads the digits.
#[must_use]
pub fn parse_datetime_local(value: &str) -> Option<time::PrimitiveDateTime> {
    use time::{Date, Month, Time};

    let (date, clock) = value.trim().split_once('T')?;
    let mut parts = date.splitn(3, '-');
    let year: i32 = parts.next()?.parse().ok()?;
    let month: u8 = parts.next()?.parse().ok()?;
    let day: u8 = parts.next()?.parse().ok()?;
    let date = Date::from_calendar_date(year, Month::try_from(month).ok()?, day).ok()?;
    let mut parts = clock.splitn(3, ':');
    let hour: u8 = parts.next()?.parse().ok()?;
    let minute: u8 = parts.next()?.parse().ok()?;
    let second: u8 = match parts.next() {
        Some(seconds) => seconds.parse().ok()?,
        None => 0,
    };
    Some(time::PrimitiveDateTime::new(
        date,
        Time::from_hms(hour, minute, second).ok()?,
    ))
}

/// "14:05".
fn clock_of(zoned: &jiff::Zoned) -> String {
    format!("{:02}:{:02}", zoned.hour(), zoned.minute())
}

/// A zone name with a UTC delta spelled out; plain "UTC" stays bare rather
/// than becoming the tautological "UTC (UTC)".
fn zone_with_offset(name: &str, offset_seconds: i32) -> String {
    if name == "UTC" {
        return name.to_owned();
    }
    format!("{name} ({})", time_zones::offset_label(offset_seconds))
}

/// RFC 3339 UTC — the machine-readable half of every timestamp we emit.
fn rfc3339(at: OffsetDateTime) -> String {
    at.to_offset(time::UtcOffset::UTC)
        .format(&Rfc3339)
        .unwrap_or_else(|_| at.to_string())
}

fn parse_iso(iso: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(iso, &Rfc3339).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-07-03T12:05:00Z — a summer instant, so the DST-observing zones
    /// below are on their summer offsets.
    fn summer() -> OffsetDateTime {
        OffsetDateTime::parse("2026-07-03T12:05:00Z", &Rfc3339).unwrap()
    }

    fn clock(name: &str) -> ViewerClock {
        ViewerClock::of(Some(name), Locale::default())
    }

    /// Fluent wraps every interpolated argument in bidi isolation marks. They
    /// are invisible in a browser but not to `assert_eq!`; the same helper
    /// guards `view::tests::timestamps_take_their_shape_from_the_catalog`.
    fn visible(value: &str) -> String {
        value.replace(['\u{2068}', '\u{2069}'], "")
    }

    #[test]
    fn stamp_is_the_wall_clock_reading_with_no_offset_suffix() {
        assert_eq!(visible(&clock("UTC").stamp(summer())), "Jul 3, 2026, 12:05");
        assert_eq!(
            visible(&clock("Europe/Berlin").stamp(summer())),
            "Jul 3, 2026, 14:05"
        );
        assert_eq!(
            visible(&clock("America/New_York").stamp(summer())),
            "Jul 3, 2026, 08:05"
        );
    }

    /// The half- and quarter-hour zones the extended inventory exists for.
    #[test]
    fn fractional_offsets_render_exactly() {
        assert_eq!(
            visible(&clock("Asia/Kolkata").stamp(summer())),
            "Jul 3, 2026, 17:35"
        );
        assert_eq!(
            visible(&clock("Asia/Kathmandu").stamp(summer())),
            "Jul 3, 2026, 17:50"
        );
        assert_eq!(
            visible(&clock("Australia/Eucla").stamp(summer())),
            "Jul 3, 2026, 20:50"
        );
        assert_eq!(
            visible(&clock("America/St_Johns").stamp(summer())),
            "Jul 3, 2026, 09:35"
        );
    }

    /// The offset in the tooltip is the one in force at that instant, not
    /// today's — the whole reason R2 asks for a per-instant delta.
    #[test]
    fn the_offset_follows_the_dst_switch() {
        let berlin = clock("Europe/Berlin");
        let winter = OffsetDateTime::parse("2026-01-15T12:00:00Z", &Rfc3339).unwrap();
        let summer = OffsetDateTime::parse("2026-07-15T12:00:00Z", &Rfc3339).unwrap();
        assert!(
            berlin.tooltip(winter).contains("UTC+01:00"),
            "{}",
            berlin.tooltip(winter)
        );
        assert!(
            berlin.tooltip(summer).contains("UTC+02:00"),
            "{}",
            berlin.tooltip(summer)
        );
        assert_eq!(visible(&berlin.stamp(winter)), "Jan 15, 2026, 13:00");
        assert_eq!(visible(&berlin.stamp(summer)), "Jul 15, 2026, 14:00");
    }

    #[test]
    fn the_tooltip_carries_both_readings() {
        assert_eq!(
            visible(&clock("Europe/Berlin").tooltip(summer())),
            "Jul 3, 2026, 14:05 Europe/Berlin (UTC+02:00) · 12:05 UTC"
        );
    }

    /// When the two readings land on different days the UTC half must spell
    /// its date, or it names the wrong one.
    #[test]
    fn the_tooltip_dates_the_utc_half_across_a_day_boundary() {
        let late = OffsetDateTime::parse("2026-07-03T22:30:00Z", &Rfc3339).unwrap();
        assert_eq!(
            visible(&clock("Pacific/Auckland").tooltip(late)),
            "Jul 4, 2026, 10:30 Pacific/Auckland (UTC+12:00) · Jul 3, 2026, 22:30 UTC"
        );
        let early = OffsetDateTime::parse("2026-07-03T02:30:00Z", &Rfc3339).unwrap();
        assert_eq!(
            visible(&clock("America/Los_Angeles").tooltip(early)),
            "Jul 2, 2026, 19:30 America/Los_Angeles (UTC-07:00) · Jul 3, 2026, 02:30 UTC"
        );
    }

    /// A UTC viewer has no second reading to be shown.
    #[test]
    fn a_utc_viewer_gets_a_single_reading() {
        assert_eq!(
            visible(&clock("UTC").tooltip(summer())),
            "Jul 3, 2026, 12:05 UTC"
        );
        assert!(ViewerClock::utc(Locale::default()).is_utc());
        assert!(ViewerClock::of(None, Locale::default()).is_utc());
        assert!(ViewerClock::of(Some("Mars/Olympus_Mons"), Locale::default()).is_utc());
    }

    /// R4: the machine-readable half never localises.
    #[test]
    fn the_element_keeps_utc_in_the_datetime_attribute() {
        let markup = visible(&clock("Europe/Berlin").element(summer()).into_string());
        assert!(
            markup.contains(r#"datetime="2026-07-03T12:05:00Z""#),
            "{markup}"
        );
        assert!(markup.contains("Jul 3, 2026, 14:05"), "{markup}");
        assert!(markup.contains("12:05 UTC"), "{markup}");
        assert!(!markup.contains("data-absolute"), "{markup}");
        assert!(
            clock("Europe/Berlin")
                .element_absolute(summer())
                .into_string()
                .contains("data-absolute")
        );
    }

    #[test]
    fn input_value_round_trips_the_wall_clock() {
        assert_eq!(
            clock("Europe/Berlin").input_value(summer()),
            "2026-07-03T14:05"
        );
        assert_eq!(clock("UTC").input_value(summer()), "2026-07-03T12:05");
    }

    #[test]
    fn label_names_the_zone_and_its_delta() {
        assert_eq!(clock("UTC").label(), "UTC");
        assert!(
            clock("Europe/Berlin")
                .label()
                .starts_with("Europe/Berlin (")
        );
    }

    #[test]
    fn unparseable_input_survives_as_itself() {
        let clock = clock("Europe/Berlin");
        assert_eq!(clock.stamp_iso("not a timestamp"), "not a timestamp");
        assert_eq!(clock.date_iso("not a timestamp"), "not a timestamp");
        assert_eq!(clock.month_year_iso("nope"), "nope");
        assert_eq!(clock.tooltip_iso("nope"), "nope");
    }

    /// Field order and month names are catalog-owned, not baked into the
    /// formatter — the new tooltip keys must honour that as the older ones do.
    /// Russian writes the day first and abbreviates its own months.
    #[test]
    fn the_new_keys_take_their_shape_from_the_catalog() {
        let ru = ViewerClock::of(Some("Europe/Berlin"), Locale::negotiate(Some("ru"), None));
        assert_eq!(visible(&ru.stamp(summer())), "3 июл. 2026, 14:05");
        assert_eq!(
            visible(&ru.tooltip(summer())),
            "3 июл. 2026, 14:05 Europe/Berlin (UTC+02:00) · 12:05 UTC"
        );
        assert_eq!(visible(&ru.date(summer())), "3 июл. 2026");
        assert_eq!(
            visible(&ru.month_year_iso("2026-07-03T12:05:00Z")),
            "июл. 2026"
        );
        let ru_utc = ViewerClock::utc(Locale::negotiate(Some("ru"), None));
        assert_eq!(visible(&ru_utc.tooltip(summer())), "3 июл. 2026, 12:05 UTC");
    }

    #[test]
    fn the_zone_chip_names_the_zone_and_links_to_the_preference() {
        let markup = visible(&zone_chip(&clock("Europe/Berlin")).into_string());
        assert!(markup.contains("Europe/Berlin ("), "{markup}");
        assert!(
            markup.contains(r#"href="/settings/preferences""#),
            "{markup}"
        );
    }

    #[test]
    fn the_name_stays_an_inventory_entry() {
        // What keeps `AT TIME ZONE` interpolation safe by construction.
        assert_eq!(clock("Europe/Berlin").name(), "Europe/Berlin");
        assert_eq!(
            ViewerClock::of(Some("'; DROP TABLE users --"), Locale::default()).name(),
            "UTC"
        );
    }
}
