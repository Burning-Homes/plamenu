//! A collection page is admitted only as a child of its proven music index.
use super::document;
use crate::error::ApiError;
use plamenu_ap::activity::{PUBLIC, attributed_to_ids, id_of};
use serde_json::Value;
fn invalid(message: &str) -> ApiError {
    ApiError::Unprocessable(message.into())
}
pub struct Page {
    pub items: Vec<Value>,
    pub next: Option<String>,
}
pub fn parse(
    root: &str,
    controller: &str,
    expected: &str,
    value: &Value,
) -> Result<Page, ApiError> {
    let origin = document::url(root)?;
    if document::url(expected)?.origin() != origin.origin()
        || value["id"].as_str() != Some(expected)
        || !matches!(
            value["type"].as_str(),
            Some("CollectionPage" | "OrderedCollectionPage")
        )
        || id_of(&value["partOf"]) != Some(root)
    {
        return Err(invalid("Remote music page does not belong to its library"));
    }
    if attributed_to_ids(value)
        .chain(value.get("actor").and_then(id_of))
        .any(|owner| owner != controller)
    {
        return Err(invalid("Remote music page has a different controller"));
    }
    let recipients = ["to", "cc", "audience"]
        .into_iter()
        .flat_map(|key| {
            value
                .get(key)
                .into_iter()
                .flat_map(|value| {
                    value
                        .as_array()
                        .map_or_else(|| std::slice::from_ref(value), Vec::as_slice)
                })
                .filter_map(id_of)
        })
        .collect::<Vec<_>>();
    if !recipients.is_empty() && !recipients.contains(&PUBLIC) {
        return Err(invalid("Remote music page is not public"));
    }
    let items = value
        .get("items")
        .or_else(|| value.get("orderedItems"))
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("Remote music page has no items"))?;
    if items.len() > 500 {
        return Err(invalid("Remote music page is too large"));
    }
    let next = value
        .get("next")
        .and_then(id_of)
        .map(|next| {
            if document::url(next)?.origin() != origin.origin() || next == expected || next == root
            {
                return Err(invalid("Invalid remote music next page"));
            }
            Ok(next.to_owned())
        })
        .transpose()?;
    Ok(Page {
        items: items.clone(),
        next,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn exact_pages_require_parent_origin_controller_audience_and_finite_items() {
        let root = "https://music.example/library";
        let actor = "https://music.example/actors/alice";
        let uri = "https://music.example/library?page=1";
        let valid = json!({"id":uri,"type":"CollectionPage","partOf":root,"items":[],"next":"https://music.example/library?page=2"});
        assert_eq!(
            parse(root, actor, uri, &valid).unwrap().next.as_deref(),
            Some("https://music.example/library?page=2")
        );
        for (key, bad) in [
            ("partOf", json!("https://music.example/other")),
            ("actor", json!("https://music.example/actors/bob")),
            ("to", json!([actor])),
            ("next", json!("https://other.example/page")),
            ("next", json!(uri)),
            ("items", json!(vec![json!({}); 501])),
        ] {
            let mut value = valid.clone();
            value[key] = bad;
            assert!(parse(root, actor, uri, &value).is_err());
        }
    }
}
