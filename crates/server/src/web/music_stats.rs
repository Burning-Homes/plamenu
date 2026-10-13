//! Native listening charts use ordinary GET filters and accessible count tables.
use super::{layout, session::WebUser};
use crate::{
    AppState,
    error::ApiError,
    music_stats::{self, Bucket, Group, ReleaseGroup, Request, TopEntry},
};
use axum::{
    extract::{Query, State},
    http::header,
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
fn ranked(l: super::i18n::Locale, key: &str, rows: &[TopEntry]) -> Markup {
    let max = rows.iter().map(|e| e.play_count).max().unwrap_or(1);
    html! {section.music-stats-card{h2{(l.text(key))}
        @if rows.is_empty(){p{(l.text("music-stats-empty"))}}
        @else{table{caption.visually-hidden{(l.text(key))}thead{tr{th scope="col"{(l.text("music-stats-name"))}th scope="col"{(l.text("music-stats-count"))}}}
            tbody{@for item in rows{tr{th scope="row"{
                @if let Some(image)=&item.image_url{img.music-stats-artwork src=(image) alt="" width="48" height="48" loading="lazy";}
                @if let Some(url)=&item.url{a href=(url){(item.name)}}@else{(item.name)}
                @if let Some(artist)=&item.artist_name{div.music-stats-credit{
                    @if let Some(url)=&item.artist_url{a href=(url){(artist)}}@else{(artist)}
                }}
            }td{(item.play_count)progress value=(item.play_count) max=(max) aria-label=(l.text("music-stats-count")){} }}}}
        }}
    }}
}
fn calendar_label(clock: &super::clock::ViewerClock, value: &str) -> String {
    if let Ok(date) = time::Date::parse(
        value,
        &time::macros::format_description!("[year]-[month]-[day]"),
    ) {
        return clock.calendar_date(date);
    }
    if value.len() == 7
        && let Ok(date) = time::Date::parse(
            &format!("{value}-01"),
            &time::macros::format_description!("[year]-[month]-[day]"),
        )
    {
        return clock.calendar_month(date);
    }
    value.into()
}
fn histogram(
    l: super::i18n::Locale,
    clock: &super::clock::ViewerClock,
    key: &str,
    rows: &[Bucket],
) -> Markup {
    let max = rows.iter().map(|e| e.count).max().unwrap_or(1).max(1);
    html! {section.music-stats-card{h2{(l.text(key))}
        table{caption.visually-hidden{(l.text(key))}thead{tr{th scope="col"{(l.text("music-stats-period"))}th scope="col"{(l.text("music-stats-count"))}}}
            tbody{@for item in rows{tr{th scope="row"{(calendar_label(clock,&item.bucket))}td{(item.count)progress value=(item.count) max=(max) aria-label=(l.text("music-stats-count")){} }}}}
        }
    }}
}
fn genre_timeline(
    l: super::i18n::Locale,
    clock: &super::clock::ViewerClock,
    report: &music_stats::Report,
) -> Markup {
    html! {section.music-stats-card{h2{(l.text("music-stats-genres-timeline"))}
        table{caption.visually-hidden{(l.text("music-stats-genres-timeline"))}
            thead{tr{th scope="col"{(l.text("music-stats-period"))}th scope="col"{(l.text("music-genre"))}th scope="col"{(l.text("music-stats-count"))}}}
            tbody{@for bucket in &report.genres_timeline{
                @if bucket.genres.is_empty(){tr{th scope="row"{(calendar_label(clock,&bucket.bucket))}td{(l.text("music-stats-empty"))}td{"0"}}}
                @else{@for genre in &bucket.genres{tr{th scope="row"{(calendar_label(clock,&bucket.bucket))}td{(genre.name)}td{(genre.count)}}}}
            }}
        }
    }}
}
pub async fn index(
    State(state): State<AppState>,
    user: WebUser,
    Query(mut input): Query<Request>,
) -> Result<Response, ApiError> {
    user.current.require_scope("read:statuses")?;
    let l = user.locale;
    let clock = super::pages::clock_for(Some(&user), l);
    if input.tz.as_deref().is_none_or(str::is_empty) {
        input.tz = Some(clock.name().into());
    }
    let report = music_stats::report(&state, user.current.account.id, &input).await?;
    let parsed = |value: &str| {
        time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
            .map_err(|e| ApiError::Internal(Box::new(e)))
    };
    let local = plamenu_db::tz::utc_to_local(
        &state.pool,
        &[parsed(&report.period.from)?, parsed(&report.period.to)?],
        &report.period.tz,
    )
    .await?;
    let hour_buckets = report
        .clock
        .iter()
        .map(|h| Bucket {
            bucket: format!("{:02}:00", h.hour),
            count: h.count,
        })
        .collect::<Vec<_>>();
    let body = html! {section.column.music-stats{
        h1{(l.text("music-stats-title"))}
        (super::music::workspace_navigation(l,true,"stats"))
        p{(l.text("music-stats-hint"))}
        nav.video-explore__links aria-label=(l.text("music-title")){a href="/music"{(l.text("music-title"))}a href="/music/history"{(l.text("music-history-title"))}}
        form.settings-form method="get" action="/music/stats"{
            label for="music-stats-from"{(l.text("music-stats-from"))}input id="music-stats-from" type="date" name="from" value=(local[0].date());
            label for="music-stats-to"{(l.text("music-stats-to"))}input id="music-stats-to" type="date" name="to" value=(local[1].date());
            label for="music-stats-zone"{(l.text("settings-preferences-time-zone"))}select id="music-stats-zone" name="tz"{
                @if !crate::time_zones::ZONES.contains(&report.period.tz.as_str()){option value=(report.period.tz) selected{(report.period.tz)}}
                @for zone in crate::time_zones::ZONES{option value=(zone) selected[*zone==report.period.tz]{(zone)}}
            }
            label for="music-stats-group"{(l.text("music-stats-group"))}select id="music-stats-group" name="group_by"{
                @for (kind,value,key) in [(Group::Day,"day","music-stats-day"),(Group::Week,"week","music-stats-week"),(Group::Month,"month","music-stats-month"),(Group::Year,"year","music-stats-year")]{option value=(value) selected[kind==report.period.group_by]{(l.text(key))}}
            }
            label for="music-stats-week-start"{(l.text("music-stats-week-start"))}select id="music-stats-week-start" name="week_start"{
                @for (value,key) in [(0,"music-stats-monday"),(1,"music-stats-tuesday"),(2,"music-stats-wednesday"),(3,"music-stats-thursday"),(4,"music-stats-friday"),(5,"music-stats-saturday"),(6,"music-stats-sunday")]{option value=(value) selected[value==report.period.week_start]{(l.text(key))}}
            }
            label for="music-stats-release-group"{(l.text("music-stats-release-group"))}select id="music-stats-release-group" name="release_group"{
                option value="year" selected[report.period.release_group==ReleaseGroup::Year]{(l.text("music-stats-year"))}option value="decade" selected[report.period.release_group==ReleaseGroup::Decade]{(l.text("music-stats-decade"))}
            }
            label for="music-stats-limit"{(l.text("music-stats-limit"))}input id="music-stats-limit" name="limit" type="number" min="1" max="100" value=(report.period.limit) required;
            button type="submit"{(l.text("music-stats-show"))}
        }
        p.music-stats-total data-count=(report.play_count){(l.text("music-stats-count")) ": " (report.play_count)}
        (ranked(l,"music-stats-top-tracks",&report.top.tracks))
        (ranked(l,"music-stats-top-artists",&report.top.artists))
        (ranked(l,"music-stats-top-albums",&report.top.albums))
        (ranked(l,"music-stats-top-genres",&report.top.genres))
        (histogram(l,&clock,"music-stats-plays",&report.plays))
        (genre_timeline(l,&clock,&report))
        (histogram(l,&clock,"music-stats-releases",&report.releases))
        (histogram(l,&clock,"music-stats-clock",&hour_buckets))
    }};
    Ok((
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Cookie"),
        ],
        layout::shell(&l.text("music-stats-title"), Some(&user), &body),
    )
        .into_response())
}
