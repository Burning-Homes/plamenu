//! Shared full emoji-reaction picker.
//!
//! Status and announcement cards link here as their no-JavaScript path. The
//! ordinary card contains no catalog data: only opening this page queries the
//! instance's custom emoji and expands the embedded Unicode Emoji 17.0 data.
//! JavaScript reads the exact same Unicode catalog lazily as a static asset.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use axum::response::{IntoResponse, Response};
use maud::{Markup, html};
use serde::Deserialize;

use super::actions::safe_return;
use super::session::WebUser;
use super::{assets, layout};
use crate::error::ApiError;
use crate::state::AppState;

#[derive(Deserialize)]
struct EmojiCatalog {
    version: String,
    groups: Vec<EmojiGroup>,
}

#[derive(Deserialize)]
struct EmojiGroup {
    name: String,
    subgroups: Vec<EmojiSubgroup>,
}

#[derive(Deserialize)]
struct EmojiSubgroup {
    name: String,
    emoji: Vec<(String, String)>,
}

static CATALOG: LazyLock<EmojiCatalog> = LazyLock::new(|| {
    serde_json::from_str(assets::EMOJI_CATALOG)
        .expect("the bundled Unicode emoji catalog must be valid")
});

/// Fields submitted by the catalog page's single form. A submit button adds
/// only its chosen `emoji`, so thousands of choices do not become thousands
/// of posted fields.
#[derive(Deserialize)]
pub(crate) struct PickerForm {
    pub(crate) csrf: String,
    pub(crate) return_to: Option<String>,
    pub(crate) emoji: String,
}

struct CustomChoice {
    shortcode: String,
    url: String,
    category: String,
}

fn subgroup_label(name: &str) -> String {
    name.replace('-', " ")
}

fn unicode_choice(emoji: &str, name: &str) -> Markup {
    html! {
        button.reaction-catalog__choice type="submit" name="emoji" value=(emoji)
            title=(name) aria-label=(format!("{emoji}: {name}")) {
            (emoji)
        }
    }
}

fn custom_choice(choice: &CustomChoice) -> Markup {
    let label = format!(":{}:", choice.shortcode);
    html! {
        button.reaction-catalog__choice type="submit" name="emoji"
            value=(choice.shortcode) title=(label) aria-label=(label) {
            img src=(choice.url) alt="" loading="lazy" decoding="async";
        }
    }
}

fn catalog_form(base: &str, user: &WebUser, return_to: &str, custom: &[CustomChoice]) -> Markup {
    let locale = user.locale;
    let mut custom_groups: BTreeMap<&str, Vec<&CustomChoice>> = BTreeMap::new();
    for choice in custom {
        custom_groups
            .entry(choice.category.as_str())
            .or_default()
            .push(choice);
    }
    let uncategorized = custom_groups.remove("");
    let personal = custom_groups.remove("\0personal");

    html! {
        form.reaction-catalog method="post" action=(format!("{base}/react")) {
            input type="hidden" name="csrf" value=(user.csrf);
            input type="hidden" name="return_to" value=(return_to);

            @if !custom.is_empty() {
                section.reaction-catalog__custom {
                    h2 { (locale.text("reaction-picker-custom")) }
                    @if let Some(choices) = &personal {
                        h3 { "Your emoji" }
                        div.reaction-catalog__grid {
                            @for choice in choices { (custom_choice(choice)) }
                        }
                    }
                    @for (category, choices) in &custom_groups {
                        h3 { (category) }
                        div.reaction-catalog__grid {
                            @for choice in choices { (custom_choice(choice)) }
                        }
                    }
                    @if let Some(choices) = &uncategorized {
                        h3 { (locale.text("reaction-picker-custom-uncategorized")) }
                        div.reaction-catalog__grid {
                            @for choice in choices { (custom_choice(choice)) }
                        }
                    }
                }
            }

            section.reaction-catalog__unicode {
                h2 {
                    (locale.text("reaction-picker-unicode"))
                    span.muted { " · " (CATALOG.version) }
                }
                @for (index, group) in CATALOG.groups.iter().enumerate() {
                    @let count: usize = group.subgroups.iter().map(|s| s.emoji.len()).sum();
                    details.reaction-catalog__category open[index == 0] {
                        summary {
                            span { (group.name) }
                            span.muted { (count) }
                        }
                        @for subgroup in &group.subgroups {
                            h3 { (subgroup_label(&subgroup.name)) }
                            div.reaction-catalog__grid {
                                @for (emoji, name) in &subgroup.emoji {
                                    (unicode_choice(emoji, name))
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Renders the on-demand picker page after the caller has confirmed its target
/// exists and is visible to the viewer.
pub(crate) async fn picker_page(
    state: &AppState,
    user: &WebUser,
    base: &str,
    requested_return: Option<&str>,
    fallback: &str,
) -> Result<Response, ApiError> {
    let return_to = safe_return(requested_return, fallback);
    let custom = plamenu_db::custom_emoji::listed_for_account(&state.pool, user.current.account.id)
        .await?
        .into_iter()
        .filter_map(|(emoji, personal)| {
            let entity = crate::emoji::custom_emoji_json(&state.config.domain, &emoji, false);
            Some(CustomChoice {
                shortcode: emoji.shortcode,
                url: entity.get("url")?.as_str()?.to_owned(),
                category: if personal {
                    "\0personal".into()
                } else {
                    emoji.category.unwrap_or_default()
                },
            })
        })
        .collect::<Vec<_>>();
    let title = user.locale.text("reaction-picker-title");
    let body = html! {
        section.column.reaction-catalog-page {
            a.reaction-catalog-page__back href=(return_to) {
                "← " (user.locale.text("reaction-picker-back"))
            }
            h1 { (title) }
            p.muted { (user.locale.text("reaction-picker-help")) }
            (catalog_form(base, user, &return_to, &custom))
        }
    };
    Ok(layout::shell(&title, Some(user), &body).into_response())
}

#[cfg(test)]
mod tests {
    use super::CATALOG;

    #[test]
    fn bundled_catalog_is_the_complete_unicode_17_rgi_set() {
        let count: usize = CATALOG
            .groups
            .iter()
            .flat_map(|group| &group.subgroups)
            .map(|subgroup| subgroup.emoji.len())
            .sum();
        assert_eq!(CATALOG.version, "17.0");
        assert_eq!(count, 3_953);
        assert!(CATALOG.groups.iter().any(|group| group.name == "Component"));
    }
}
