# Media operator API

For deployment and recovery, see [media operations](../operators/media.md).

## Federation video copies

The operator API requires `MANAGE_FEDERATION` and the matching broad
`admin:read`/`admin:write` token scope. Use authenticated requests with these
JSON bodies; never place tokens in URLs or command arguments:

| Method/path | Request or response |
| --- | --- |
| `PUT /api/v1/admin/video_mirror_approvals/{domain}` | `{"allow_source":true,"allow_mirror":false}` approves an exact source; reverse the flags to approve an advertising server |
| `POST /api/v1/admin/video_mirrors` | `{"status_id":123,"consent_attestation":""}` requests an already resolved canonical Video |
| `GET /api/v1/admin/video_mirrors` | Up to 100 latest jobs, current charged bytes and private no-store response |
| `GET /api/v1/admin/video_mirrors/{id}` | State, generation, expiry, charge and failure status |
| `DELETE /api/v1/admin/video_mirrors/{id}` | Withdraws serving and atomically queues the signed Undo |

Creators grant or revoke copying using the first-party HTML media sharing
form's mirror-permission checkbox, or the `mirror_allowed` flag in the existing
distribution API. It defaults to false; merely making a video public or enabling
download does not grant permission. The API replaces all distribution flags:
include the desired download/embed flags when updating permission. The signed
Video carries Plamenu's full-IRI permission extension; a live announcement never
advertises finite-copy permission. For foreign creators without that vocabulary,
an operator must provide private explicit creator-consent evidence in
`consent_attestation` (20–2,000 bytes). This is an operator attestation, not
automatically verified consent or an inference from PeerTube's download flag.

No public CacheFile is advertised before every graph asset has a durable store
acknowledgement and current source authority still matches. Signed Create/Undo
and their state transitions use the same database transaction. Source updates,
withdrawal, expiry, suspension or domain-policy changes deny warm reads. Accepted
remote offers remain actor-bound and require explicit mirror-domain approval;
a replayed Create cannot undo an authenticated withdrawal. A copy never grants
access to a private/deleted original. Only the specialized mirror route may
serve its permanently private storage-key namespace.
Incoming offer bookkeeping currently admits at most 256 distinct known URIs
per advertising actor, including retained withdrawal tombstones. Reaching that
bound refuses new offers rather than forgetting withdrawals or growing without
limit; it is not a claim of unlimited foreign redundancy capacity.


## Music jobs

`GET /api/v1/admin/music/jobs` requires `admin:read` and `view_devops`.
`POST /api/v1/admin/music/jobs/cancel` requires `admin:write` and
`manage_settings`. Send `{kind,id}` or `{jobs:[{kind,id},...]}` with at most
100 unique tasks. Cancellation stops current work; it preserves saved folder
and output configuration. File recovery cannot be stopped halfway through.
