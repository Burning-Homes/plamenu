//! The selectable time-zone inventory. Mastodon restricts
//! `users.time_zone` to a fixed set (`ActiveSupport::TimeZone`); we mirror that
//! with a curated list of IANA identifiers, curated to cover *every* distinct
//! UTC offset the database produces (see [`ZONES`]) so nobody is silently
//! shifted off their own wall clock.
//!
//! An unknown identifier is treated as "unset" (the server default, UTC) rather
//! than an error, matching Rails' lenient `time_zone=` assignment. That
//! leniency is also what keeps `AT TIME ZONE` interpolation safe: every
//! identifier that reaches a query has been through [`normalize`] and is a
//! `&'static str` from this list.

/// Every zone the preferences dropdown offers, ordered roughly west-to-east.
/// IANA identifiers so displayed timestamps and (later) digest send-windows can
/// be localised without a friendly-name mapping table.
///
/// The list is curated rather than the whole IANA database (~600 entries, most
/// of them aliases), but it is curated to an invariant: **every distinct UTC
/// offset the database produces is represented here**, in both DST seasons —
/// including the half- and quarter-hour zones (`America/St_Johns` at −03:30,
/// `Asia/Kathmandu` at +05:45, `Australia/Eucla` at +08:45,
/// `Pacific/Chatham` at +12:45). An unlisted zone falls back to UTC
/// (see [`normalize`]), so a gap here is a user silently shifted off their
/// wall clock. `zone_inventory_covers_every_offset` holds the line.
pub static ZONES: &[&str] = &[
    "Pacific/Midway",
    "Pacific/Pago_Pago",
    "Pacific/Niue",
    "Pacific/Honolulu",
    "Pacific/Rarotonga",
    "Pacific/Tahiti",
    "Pacific/Marquesas",
    "America/Adak",
    "Pacific/Gambier",
    "America/Anchorage",
    "America/Juneau",
    "America/Nome",
    "Pacific/Pitcairn",
    "America/Los_Angeles",
    "America/Vancouver",
    "America/Tijuana",
    "America/Phoenix",
    "America/Denver",
    "America/Edmonton",
    "America/Mazatlan",
    "America/Whitehorse",
    "America/Chihuahua",
    "America/Regina",
    "America/Chicago",
    "America/Winnipeg",
    "America/Mexico_City",
    "America/Guatemala",
    "America/El_Salvador",
    "America/Tegucigalpa",
    "America/Managua",
    "America/Costa_Rica",
    "America/Belize",
    "Pacific/Galapagos",
    "Pacific/Easter",
    "America/New_York",
    "America/Toronto",
    "America/Detroit",
    "America/Indiana/Indianapolis",
    "America/Havana",
    "America/Nassau",
    "America/Jamaica",
    "America/Panama",
    "America/Cancun",
    "America/Bogota",
    "America/Lima",
    "America/Guayaquil",
    "America/Halifax",
    "America/Puerto_Rico",
    "America/Santo_Domingo",
    "America/Barbados",
    "America/Port_of_Spain",
    "America/Caracas",
    "America/La_Paz",
    "America/Manaus",
    "America/Asuncion",
    "America/Santiago",
    "America/St_Johns",
    "America/Sao_Paulo",
    "America/Argentina/Buenos_Aires",
    "America/Montevideo",
    "America/Cayenne",
    "America/Miquelon",
    "America/Noronha",
    "Atlantic/South_Georgia",
    "America/Nuuk",
    "Atlantic/Cape_Verde",
    "America/Scoresbysund",
    "Atlantic/Azores",
    "UTC",
    "Atlantic/Reykjavik",
    "Africa/Abidjan",
    "Africa/Accra",
    "Africa/Bissau",
    "Africa/Monrovia",
    "Atlantic/Canary",
    "Europe/London",
    "Europe/Dublin",
    "Europe/Lisbon",
    "Africa/Casablanca",
    "Europe/Madrid",
    "Europe/Paris",
    "Europe/Brussels",
    "Europe/Amsterdam",
    "Europe/Luxembourg",
    "Europe/Berlin",
    "Europe/Zurich",
    "Europe/Vienna",
    "Europe/Rome",
    "Europe/Malta",
    "Europe/Prague",
    "Europe/Budapest",
    "Europe/Zagreb",
    "Europe/Sarajevo",
    "Europe/Belgrade",
    "Europe/Skopje",
    "Europe/Tirane",
    "Europe/Warsaw",
    "Europe/Stockholm",
    "Europe/Oslo",
    "Europe/Copenhagen",
    "Europe/Andorra",
    "Europe/Monaco",
    "Europe/Gibraltar",
    "Africa/Algiers",
    "Africa/Tunis",
    "Africa/Lagos",
    "Africa/Kinshasa",
    "Africa/Luanda",
    "Africa/Ndjamena",
    "Europe/Helsinki",
    "Europe/Tallinn",
    "Europe/Riga",
    "Europe/Vilnius",
    "Europe/Kyiv",
    "Europe/Chisinau",
    "Europe/Bucharest",
    "Europe/Sofia",
    "Europe/Athens",
    "Europe/Kaliningrad",
    "Africa/Cairo",
    "Africa/Tripoli",
    "Africa/Khartoum",
    "Africa/Johannesburg",
    "Africa/Harare",
    "Africa/Maputo",
    "Africa/Windhoek",
    "Asia/Jerusalem",
    "Asia/Beirut",
    "Asia/Damascus",
    "Asia/Amman",
    "Asia/Gaza",
    "Europe/Istanbul",
    "Europe/Moscow",
    "Europe/Minsk",
    "Africa/Nairobi",
    "Africa/Addis_Ababa",
    "Asia/Riyadh",
    "Asia/Qatar",
    "Asia/Baghdad",
    "Asia/Tehran",
    "Europe/Samara",
    "Asia/Dubai",
    "Asia/Muscat",
    "Asia/Baku",
    "Asia/Tbilisi",
    "Asia/Yerevan",
    "Indian/Mauritius",
    "Asia/Kabul",
    "Asia/Yekaterinburg",
    "Asia/Karachi",
    "Asia/Tashkent",
    "Asia/Ashgabat",
    "Asia/Almaty",
    "Indian/Maldives",
    "Asia/Kolkata",
    "Asia/Colombo",
    "Asia/Kathmandu",
    "Asia/Dhaka",
    "Asia/Omsk",
    "Asia/Bishkek",
    "Asia/Thimphu",
    "Asia/Yangon",
    "Asia/Bangkok",
    "Asia/Jakarta",
    "Asia/Ho_Chi_Minh",
    "Asia/Novosibirsk",
    "Asia/Krasnoyarsk",
    "Asia/Shanghai",
    "Asia/Hong_Kong",
    "Asia/Macau",
    "Asia/Taipei",
    "Asia/Singapore",
    "Asia/Kuala_Lumpur",
    "Asia/Manila",
    "Asia/Brunei",
    "Asia/Ulaanbaatar",
    "Asia/Irkutsk",
    "Australia/Perth",
    "Australia/Eucla",
    "Asia/Tokyo",
    "Asia/Seoul",
    "Asia/Pyongyang",
    "Asia/Jayapura",
    "Asia/Yakutsk",
    "Asia/Dili",
    "Australia/Darwin",
    "Australia/Adelaide",
    "Australia/Brisbane",
    "Australia/Sydney",
    "Australia/Melbourne",
    "Australia/Hobart",
    "Asia/Vladivostok",
    "Pacific/Port_Moresby",
    "Pacific/Guam",
    "Australia/Lord_Howe",
    "Asia/Magadan",
    "Asia/Sakhalin",
    "Pacific/Guadalcanal",
    "Pacific/Noumea",
    "Pacific/Efate",
    "Asia/Kamchatka",
    "Pacific/Auckland",
    "Pacific/Fiji",
    "Pacific/Tarawa",
    "Pacific/Majuro",
    "Pacific/Chatham",
    "Pacific/Tongatapu",
    "Pacific/Apia",
    "Pacific/Fakaofo",
    "Pacific/Kiritimati",
];

/// The stored form of a submitted zone: `Some(identifier)` if it's in the
/// inventory, `None` for the empty/"server default" choice or an unknown value.
#[must_use]
pub fn normalize(value: &str) -> Option<&'static str> {
    let value = value.trim();
    ZONES.iter().copied().find(|zone| *zone == value)
}

/// The zone as a `jiff` handle for in-process conversions (viewer-local
/// timestamp display). An unknown or unset identifier falls back to UTC,
/// mirroring [`normalize`]'s leniency.
#[must_use]
pub fn tz(name: &str) -> jiff::tz::TimeZone {
    jiff::tz::TimeZone::get(name.trim()).unwrap_or(jiff::tz::TimeZone::UTC)
}

/// A `"UTC+04:00"`-style label for an offset in seconds; plain `"UTC"` at
/// zero. Used beside zone names so the exact delta is always visible.
#[must_use]
pub fn offset_label(seconds: i32) -> String {
    if seconds == 0 {
        return "UTC".to_owned();
    }
    let sign = if seconds < 0 { '-' } else { '+' };
    let abs = seconds.abs();
    format!("UTC{sign}{:02}:{:02}", abs / 3600, (abs % 3600) / 60)
}

/// The zone's current UTC offset in seconds.
#[must_use]
pub fn current_offset_seconds(zone: &jiff::tz::TimeZone) -> i32 {
    zone.to_offset(jiff::Timestamp::now()).seconds()
}

/// The zone's UTC offset in seconds *at a given instant* — what R2 wants
/// beside a timestamp, so a reading from the far side of a DST switch carries
/// the delta that was actually in force then, not today's.
#[must_use]
pub fn offset_seconds_at(zone: &jiff::tz::TimeZone, at: jiff::Timestamp) -> i32 {
    zone.to_offset(at).seconds()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two fixed instants, one per DST season, so the coverage assertion below
    /// is deterministic: half the world's offsets only exist in July, the
    /// other half only in January.
    const PROBES: [&str; 2] = ["2026-01-15T12:00:00Z", "2026-07-15T12:00:00Z"];

    fn probe(iso: &str) -> jiff::Timestamp {
        iso.parse().expect("probe instant parses")
    }

    #[test]
    fn every_zone_is_unique_and_resolves() {
        let mut seen = std::collections::BTreeSet::new();
        for name in ZONES {
            assert!(seen.insert(*name), "{name} appears twice in ZONES");
            assert!(
                jiff::tz::TimeZone::get(name).is_ok(),
                "{name} is not in the bundled IANA database"
            );
            // `tz()` falling back to UTC would hide a typo, so check the
            // identifier survives the round-trip the dropdown relies on.
            assert_eq!(normalize(name), Some(*name), "{name} fails to normalize");
        }
    }

    /// The invariant behind the curation: a user in *any* zone the IANA
    /// database knows can pick an entry that puts them on the right wall
    /// clock. Without this, an unlisted zone silently degrades to UTC.
    ///
    /// If a tzdata update introduces a genuinely new offset this fails with
    /// the offset and a candidate zone to add — that is the intended signal,
    /// not a flake.
    #[test]
    fn zone_inventory_covers_every_offset() {
        for iso in PROBES {
            let at = probe(iso);
            let ours: std::collections::BTreeSet<i32> = ZONES
                .iter()
                .map(|name| offset_seconds_at(&tz(name), at))
                .collect();
            for name in jiff::tz::db().available() {
                let name = name.as_str();
                // Fixed-offset and legacy aliases aren't places anyone lives.
                if name.starts_with("Etc/") || !name.contains('/') {
                    continue;
                }
                let Ok(zone) = jiff::tz::TimeZone::get(name) else {
                    continue;
                };
                let offset = offset_seconds_at(&zone, at);
                assert!(
                    ours.contains(&offset),
                    "at {iso} no ZONES entry sits at {} — add {name} (or another zone at that offset)",
                    offset_label(offset)
                );
            }
        }
    }

    /// `local_to_utc` interpolates the identifier into `AT TIME ZONE`, so a
    /// zone the dropdown offers but Postgres rejects is a 500 waiting to
    /// happen. The two databases ship independently (jiff bundles its own,
    /// Postgres uses the host's), so their agreement is not free.
    #[sqlx::test(migrations = "../db/migrations")]
    async fn every_zone_resolves_in_postgres_too(pool: sqlx::PgPool) {
        let names: Vec<String> = ZONES.iter().map(|name| (*name).to_owned()).collect();
        let known: Vec<String> = sqlx::query_scalar(
            "SELECT z FROM unnest($1::text[]) AS z \
             WHERE EXISTS (SELECT 1 FROM pg_timezone_names n WHERE n.name = z)",
        )
        .bind(&names)
        .fetch_all(&pool)
        .await
        .expect("pg_timezone_names is readable");
        let known: std::collections::BTreeSet<&str> = known.iter().map(String::as_str).collect();
        let missing: Vec<&&str> = ZONES
            .iter()
            .filter(|name| !known.contains(**name))
            .collect();
        assert!(
            missing.is_empty(),
            "Postgres does not know these ZONES entries: {missing:?}"
        );
    }

    #[test]
    fn offset_label_spells_the_delta() {
        assert_eq!(offset_label(0), "UTC");
        assert_eq!(offset_label(7_200), "UTC+02:00");
        assert_eq!(offset_label(-12_600), "UTC-03:30");
        assert_eq!(offset_label(31_500), "UTC+08:45");
        assert_eq!(offset_label(50_400), "UTC+14:00");
    }

    #[test]
    fn offset_is_per_instant_not_per_zone() {
        let berlin = tz("Europe/Berlin");
        assert_eq!(offset_seconds_at(&berlin, probe(PROBES[0])), 3_600);
        assert_eq!(offset_seconds_at(&berlin, probe(PROBES[1])), 7_200);
    }

    #[test]
    fn unknown_and_blank_zones_fall_back_to_utc() {
        assert_eq!(normalize(""), None);
        assert_eq!(normalize("  "), None);
        assert_eq!(normalize("Mars/Olympus_Mons"), None);
        assert_eq!(normalize(" Europe/Berlin "), Some("Europe/Berlin"));
        assert_eq!(tz("Mars/Olympus_Mons"), jiff::tz::TimeZone::UTC);
    }
}
