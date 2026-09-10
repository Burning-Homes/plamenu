# Manage remote profile history

Remote history fetches older public and unlisted posts when a signed-in member
opens a remote `Person` or `Service` profile. It is enabled by default, including
resolution of bare item IRIs.

## Enable or disable

With **Manage federation** permission, open **Administration → History** and
use **Enable remote history hydration** and **Resolve bare item IRIs** to
control fetching. Both are on by default. Bare-IRI resolution adds individual
object fetches for servers whose outbox pages contain links instead of embedded posts.

Open a known remote profile while signed in. Its history state should progress
from queued or fetching to partial or complete. Check the History page for
failures, queue growth, and storage by origin. Media is fetched when viewed;
loading history should not create notifications or outgoing activities.

To stop new work, clear **Enable remote history hydration**. An active request
may finish within its 30-second limit. Queued jobs remain and resume when the
feature is enabled again; cached posts remain readable.

## Limits

| Work | Limit |
| --- | --- |
| Items examined per page | 20 |
| Pages per member action | 1 |
| Collection documents fetched initially | 2 |
| Concurrent jobs per origin | 1 |
| Queued jobs per origin | 2 |
| JSON response | 2 MiB |
| Job duration | 30 seconds |
| Individual object fetches with bare-IRI resolution | 5 per page |

These limits are fixed. Operators can change retention from 1 to 3650 days;
its default is 90 days.

## Read the history state

| State | Meaning or next step |
| --- | --- |
| `idle` | No active fetch |
| `queued` / `fetching` | Work is pending or running |
| `partial` | More may be available; the member can choose **Load older posts** |
| `complete` | The observed collection had no unfetched next page |
| `unsupported` | The collection, attribution, authorization, or federation policy prevented this attempt |
| `backoff` | A transport error, timeout, or rate limit delayed retry; check `retry_at` |

The profile panel and API expose attempts, retry time, and item counts. Use the
admin failure and origin tables to narrow down a problem, then inspect worker
logs. Repeated requests coalesce. Restarting the server does not clear stored
backoff; expired job leases are reclaimed.

If images fail, check the media proxy and origin availability. History images
are cached on demand and videos when played. A member's **Allow direct remote
media** preference permits a direct-origin fallback after proxy failure.

## Retention and pruning

An hourly sweep removes up to 200 stale history posts. Viewing an actor's
profile refreshes its retention clock. Posts received through live delivery
or referenced by local bookmarks, favourites, pins, reactions, votes, replies,
boosts, quotes, or reports are preserved.

Review **Cold statuses**, **Stored text bytes**, and **Cold storage by origin**
before shortening retention. **Prune one batch now** applies the retention rule
to at most 200 rows and queues media cleanup. Avoid deleting status rows
manually, which bypasses those checks. Pruned posts can be fetched again only
while the remote server still provides them.

## Client API

The endpoints use the OAuth `read` scope:

- `GET /api/v1/accounts/:id/remote_history` reads local history state.
- `POST /api/v1/accounts/:id/remote_history/fetch` requests more history.
- An authenticated first-page `GET /api/v1/accounts/:id/statuses` can also queue
  the initial fetch. Anonymous, paginated, and pinned-only requests do not.

## Unexpected activity

Disable history fetching if cached history appears in home/public/list/tag
feeds, streaming, or full-text search, or if importing it creates notifications
or outgoing federation work. Retain the affected actor and post IDs, timestamps,
and logs for investigation. Cached history should remain confined to profile
history until it is received through normal delivery.
