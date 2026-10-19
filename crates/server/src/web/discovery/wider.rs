use super::{
    ApiError, AppState, Form, IntoResponse, Markup, PageQuery, Redirect, Response, State, WebUser,
    browse, csrf_rejection, db, html, server_name,
};
use crate::discovery::wider as service;
use serde::Deserialize;

pub(super) fn location(q: &PageQuery, community: bool, notice: Option<&str>) -> String {
    let mut fields = vec![("q", q.q.as_deref().unwrap_or_default().to_owned())];
    if let Some(id) = q.search {
        fields.push(("search", id.to_string()));
    }
    if let Some(notice) = notice {
        fields.push(("notice", notice.to_owned()));
    }
    format!(
        "{}?{}&{}",
        if community {
            "/discover/communities"
        } else {
            "/discover"
        },
        serde_urlencoded::to_string(fields).unwrap_or_default(),
        q.options_query()
    )
}

pub(super) async fn controls(
    state: &AppState,
    user: &WebUser,
    q: &PageQuery,
    community: bool,
) -> Result<Markup, ApiError> {
    let l = user.locale;
    let term = service::term(q.q.as_deref().unwrap_or_default())?;
    let candidates = if term.is_empty() {
        Vec::new()
    } else {
        service::sources(
            state,
            user.current.account.id,
            if community { "community" } else { "profile" },
            q.source,
        )
        .await?
    };
    Ok(html! {
        div.discovery-search-tools {
            @if !term.is_empty() {
                details.discovery-wider {
                    summary {(l.text("discovery-search-wider"))}
                    @if candidates.is_empty() {p {(l.text("discovery-wider-no-sources"))}}
                    @else {
                        form method="post" action="/web/discover/search" data-wider-search {
                            input type="hidden" name="csrf" value=(user.csrf);
                            input type="hidden" name="q" value=(term);
                            input type="hidden" name="view" value=(if community {"communities"} else {"people"});
                            @if let Some(id)=q.source {input type="hidden" name="source_filter" value=(id);}
                            fieldset {
                                legend {(l.text("discovery-wider-send-to"))}
                                @for (n,source) in candidates.iter().enumerate() {
                                    label {input type="checkbox" name=(format!("source{}",n+1)) value=(source.id) checked; " " (server_name(&source.origin))}
                                }
                            }
                            p.discovery-wider__hint {(l.text("discovery-wider-limits"))}
                            button type="submit" {(l.text("discovery-search-selected"))}
                            p.discovery-wider__status role="status" aria-live="polite" data-searching=(l.text("discovery-wider-searching")) {}
                        }
                    }
                }
            }
            @if let Some(notice)=q.notice.as_deref().filter(|n|["wider_busy","wider_unavailable","wider_select"].contains(n)) {
                p role="status" {(l.text(&format!("discovery-{}",notice.replace('_',"-"))))}
            }
        }
    })
}

pub(super) struct Report {
    pub entries: Vec<db::Entry>,
    pub markup: Markup,
}

pub(super) async fn report(
    state: &AppState,
    user: &WebUser,
    q: &PageQuery,
    community: bool,
) -> Result<Report, ApiError> {
    let mut entries = Vec::new();
    let mut rows = Vec::new();
    let empty = || Report {
        entries: Vec::new(),
        markup: html! {},
    };
    let Some(id) = q.search else {
        return Ok(empty());
    };
    let settings = db::settings(&state.pool).await?;
    if !settings.enabled
        || settings
            .disabled_mechanisms
            .iter()
            .any(|m| m == "profile_listing")
    {
        return Ok(empty());
    }
    let kind = if community { "community" } else { "profile" };
    let Some(run) = db::search_run(
        &state.pool,
        user.current.account.id,
        id,
        &service::digest(kind, service::term(q.q.as_deref().unwrap_or_default())?),
    )
    .await?
    else {
        return Ok(empty());
    };
    let results: Vec<service::SourceResult> =
        serde_json::from_value(run.results).unwrap_or_default();
    let domains =
        plamenu_db::account_domain_block::all_domains(&state.pool, user.current.account.id).await?;
    for result in results {
        if q.source.is_some_and(|id| id != result.source) {
            continue;
        }
        let Some(source) = db::source(&state.pool, result.source).await? else {
            continue;
        };
        if !browse::source_visible(state, &source, &domains).await? {
            continue;
        }
        for id in result.entries {
            if let Some(entry) = browse::eligible(state, user.current.account.id, id).await?
                && entry.kind == kind
                && !entries.iter().any(|e: &db::Entry| e.id == id)
            {
                entries.push(entry);
            }
        }
        rows.push((
            server_name(&source.origin),
            match result.outcome.as_str() {
                "checked" => "discovery-wider-checked",
                "deadline" => "discovery-wider-deadline",
                _ => "discovery-wider-failed",
            },
        ));
    }
    let l = user.locale;
    Ok(Report {
        entries,
        markup: html! {
            @if !rows.is_empty() {
                details.discovery-wider-report open {
                    summary {(l.text("discovery-wider-results"))}
                    ul {@for (name,key) in rows {li {span {(name)} " · " (l.text(key))}}}
                }
            }
        },
    })
}

#[derive(Deserialize)]
pub struct SearchForm {
    csrf: String,
    q: String,
    view: String,
    source1: Option<i64>,
    source2: Option<i64>,
    source3: Option<i64>,
    source_filter: Option<i64>,
}

pub async fn search(
    State(state): State<AppState>,
    user: WebUser,
    Form(form): Form<SearchForm>,
) -> Result<Response, ApiError> {
    if !user.csrf_ok(&form.csrf) {
        return Ok(csrf_rejection());
    }
    if !["people", "communities"].contains(&form.view.as_str()) {
        return Err(ApiError::BadRequest("Invalid discovery view".into()));
    }
    let term = service::term(&form.q)?.to_owned();
    let community = form.view == "communities";
    let ids: Vec<_> = [form.source1, form.source2, form.source3]
        .into_iter()
        .flatten()
        .collect();
    let mut query = PageQuery {
        q: Some(term.clone()),
        source: form.source_filter,
        ..Default::default()
    };
    let notice = if ids.is_empty() {
        Some("wider_select")
    } else {
        match service::search(
            &state,
            user.current.account.id,
            if community { "community" } else { "profile" },
            &term,
            &ids,
        )
        .await?
        {
            db::SearchStart::Started(run) | db::SearchStart::Cached(run) => {
                query.search = Some(run.id);
                None
            }
            db::SearchStart::Busy => Some("wider_busy"),
            db::SearchStart::Disabled => Some("wider_unavailable"),
        }
    };
    Ok(Redirect::to(&location(&query, community, notice)).into_response())
}
