# Connect a Lemmy client

Plamenu provides a Lemmy-compatible API at `/api/v3`, targeting the
`lemmy-js-client` 0.19.11 contract. NodeInfo identifies the server as `plamenu`.
A client that chooses its API from that value must recognize Plamenu or offer
manual backend selection. Photon needs this recognition change.

## Supported operations

| Area | Operations |
| --- | --- |
| Accounts | Login, registration, password and profile changes, account deletion |
| Discovery | Site information, people, search, and object resolution |
| Communities | List, create, edit, follow, block, and moderate |
| Posts and comments | List, create, edit, delete, vote, and save |
| Inbox | Replies, mentions, private messages, unread counts, and read state |
| Reports | Post/comment reports, moderator listing, and resolution |
| Media | Pictrs upload, retrieval, and deletion; account and admin media lists |
| Custom emoji | Create, edit, and delete |
| Administration | Site settings, registration approval, suspension, administrator roles, and purges |

These operations use Plamenu's permissions, visibility, and federation rules.
Errors use Lemmy's `{ "error": "..." }` format. Requests for settings with no
Plamenu equivalent are rejected.

## Differences that affect clients

- Entity IDs are stable signed 32-bit aliases. Native Plamenu IDs and ActivityPub
  URLs remain unchanged; clients must use the IDs returned by `/api/v3`.
- Account deletion requires `delete_content=true`. Restore requests for deleted
  or removed content are unsupported.
- Editing a post cannot change its external URL or custom thumbnail.
- Post lists use chronological Plamenu timelines for every requested Lemmy
  sort value. Lemmy ranking modes are not implemented.
- Private messages are Plamenu direct conversations. Changing one message's
  read state changes the state reported for its conversation.
- The modlog API, private-message reports, and some notification, language,
  flair, poll, metadata, and vote-list APIs are not implemented.

## Testing a client

Router tests are in `crates/server/tests/lemmy_api.rs`. They cover account,
content, media, moderation, and ID round trips, including persistence across a
router restart. Run them with:

```sh
./dev test -p plamenu --test lemmy_api
```

For client testing, check backend selection first, then login, list/detail
navigation, posting and editing, inbox state, and any moderation actions the
client offers. Confirm the resulting state in Plamenu as well as the client's
response to API errors.
