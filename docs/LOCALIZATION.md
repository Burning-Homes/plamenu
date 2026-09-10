# Translate the interface

The web interface and mail use Fluent catalogs embedded in the binary:

- `crates/server/src/web/locales/en-US/main.ftl`: English source and fallback.
- `crates/server/src/web/locales/ru/main.ftl`: Russian translation.

The staff administration console is currently English-only. The interface
uses the member's language setting, then the browser's `Accept-Language`, then
English. Mail uses the recipient's saved setting, falling back to English.

## Edit a translation

Find the message in the English catalog and edit the matching identifier in
the translation. Preserve variable names such as `$count`, and use Fluent
selectors for the language's plural forms. Translate each message as a whole;
keep links and interpolated values where the sentence needs them.

When adding a message, add the same identifier to both catalogs. Use a name
that describes its purpose and group it with the related feature. Add a
comment when translators need context about a variable or where text appears.

Check syntax and matching message identifiers with:

```sh
cargo test -p plamenu --lib web::i18n
```

Rebuild and restart the server to see catalog edits. Select the language in
account settings and check the affected page, including plural forms and any
layout affected by longer text.

## Add a language

Create `crates/server/src/web/locales/<tag>/main.ftl` using the English catalog
as the message inventory. Then register it in `crates/server/src/web/i18n.rs`:

1. Include the catalog and add a `Language` variant and Fluent bundle.
2. Add its code to `Locale::AVAILABLE` and handle it in `language_from_tag`,
   `bundle`, `tag`, and `direction`. The picker uses names from
   `crates/server/src/languages.rs`; its inventory must contain the code.
3. Extend the catalog tests to check its syntax, message identifiers,
   negotiation, and plural forms.

Regional browser tags currently select the base language: `ru-RU` selects
Russian. A separate regional translation would also need negotiation changes.

## Use messages in code

Use `Locale::text` or `text_with` inside Maud templates; Maud escapes the
returned string. `Locale::plain` and `plain_with` remove Fluent's directional
isolation characters for mail and text passed to JavaScript through `data-*`
attributes.

For a sentence containing links or styled text, pass rendered Maud `Markup`
to `Locale::markup`. `markup_with` also accepts Fluent variables, such as a
numeric plural selector. These helpers return trusted HTML: render user text
through Maud before including it, and never pass it as a raw string variable.

Use the shared `web::view` helpers for dates. Pass numeric counts to Fluent
so it can select plural forms. Resolve mail language with `Locale::for_user`;
web routes should map typed errors or stable redirect codes to catalog messages.
