# OAuth for client developers

Use the authorization-code flow to connect a member's account. The member signs
in and approves access on Plamenu; your client receives a bearer token for API
requests.

Use the server's HTTPS hosting domain for every endpoint below. On a split-domain
installation this may differ from the domain in account handles. Replace the
example domains and uppercase placeholders with your own values.

## Register your application

Registration does not require authentication. Register separately on each server:

```sh
curl --request POST https://social.example.com/api/v1/apps \
  --data-urlencode 'client_name=Example client' \
  --data-urlencode 'redirect_uris=https://client.example/callback' \
  --data-urlencode 'scopes=profile read:statuses write:statuses'
```

Save `client_id` and `client_secret` from the JSON response. The secret is returned
only at creation. Plamenu requires both values in the POST body when exchanging
a code or revoking a token, including when using PKCE. HTTP Basic client
authentication and exchanges without a client secret are not supported.

`redirect_uris` accepts newline-separated URIs in a form request or an array in a
JSON request. Use an HTTPS callback for a web client; native clients can register
a custom URI scheme. Redirect URIs must match exactly at authorization and token
exchange, including any path, port, and query string. Fragments are not allowed.

For a manual command-line flow, register `urn:ietf:wg:oauth:2.0:oob` as the redirect
URI. Plamenu then displays the code in the browser for the member to copy.

## Request authorization

Generate a fresh PKCE verifier and `state` for each attempt. Keep both in the
client's pending login session. PKCE is optional for compatibility with older
clients; when used, its only supported method is `S256`.

This Python example generates the values and authorization URL:

```python
import base64
import hashlib
import secrets
from urllib.parse import urlencode

verifier = secrets.token_urlsafe(32)
challenge = base64.urlsafe_b64encode(
    hashlib.sha256(verifier.encode("ascii")).digest()
).rstrip(b"=").decode("ascii")
state = secrets.token_urlsafe(32)

parameters = {
    "response_type": "code",
    "client_id": "CLIENT_ID",
    "redirect_uri": "https://client.example/callback",
    "scope": "profile read:statuses write:statuses",
    "state": state,
    "code_challenge": challenge,
    "code_challenge_method": "S256",
}
authorization_url = "https://social.example.com/oauth/authorize?" + urlencode(parameters)
print("PKCE verifier:", verifier)
print("State:", state)
print("Authorization URL:", authorization_url)
```

Open that URL in the member's browser. After approval, Plamenu redirects to your
callback with `code` and the original `state`. Reject a callback with missing or
mismatched `state`. If the member declines, the callback contains
`error=access_denied` and `state`.

Scopes are space-separated. Request only what the client needs, within the scopes
registered for the application. This example uses `profile` to identify the signed-in
account, `read:statuses` to read timelines, and `write:statuses` to publish posts.
Add `write:media` to both requests if the client uploads files. A broad `read` or
`write` grant covers its granular scopes; a granular grant covers only that
resource. Omitting `scope` at authorization requests `read`, which
must be covered by the registration; it does not select all registered scopes.

## Exchange the code

Codes expire after ten minutes and can be used only once. Send the verifier from
the same authorization attempt, and the same redirect URI:

```sh
curl --request POST https://social.example.com/oauth/token \
  --data-urlencode 'grant_type=authorization_code' \
  --data-urlencode 'client_id=CLIENT_ID' \
  --data-urlencode 'client_secret=CLIENT_SECRET' \
  --data-urlencode 'redirect_uri=https://client.example/callback' \
  --data-urlencode 'code=AUTHORIZATION_CODE' \
  --data-urlencode 'code_verifier=PKCE_VERIFIER'
```

The response contains `access_token`, `token_type` (`Bearer`), `scope` (the granted
space-separated scopes), and `created_at` (Unix seconds). There is no
`refresh_token` or `expires_in`: API tokens have no fixed expiry. They can still be
revoked, and account or server restrictions can prevent their use.

## Use and revoke the token

Set `ACCESS_TOKEN` in your environment and send it in the `Authorization` header:

```sh
curl https://social.example.com/api/v1/accounts/verify_credentials \
  --header "Authorization: Bearer $ACCESS_TOKEN"
```

This returns the member's account, including its `id` and `username`. Store tokens
in the client's credential storage and keep them out of URLs and logs.

Revoke the token when disconnecting the account:

```sh
curl --request POST https://social.example.com/oauth/revoke \
  --data-urlencode 'client_id=CLIENT_ID' \
  --data-urlencode 'client_secret=CLIENT_SECRET' \
  --data-urlencode 'token=ACCESS_TOKEN'
```

A successful request returns HTTP 200 with `{}`, including when the token is
already revoked or unknown. Members can also revoke access in their account
settings.

## Other grants and errors

`/oauth/token` also accepts `grant_type=client_credentials` with `client_id`,
`client_secret`, and an optional `scope`. It creates an application token with no
member attached, so it cannot read a member's home timeline or post on their
behalf. An omitted scope uses the application's registered scopes. Password and
refresh-token grants are not supported.

| Error | What to check |
| --- | --- |
| `invalid_client` | Send the client ID and secret from this server in the POST body. |
| `invalid_scope` | Requested scopes must be recognized and covered by the application's registration. |
| `invalid_grant` | Check code age, exact redirect URI, application, and PKCE verifier. Start a new authorization attempt; a failed exchange may have consumed the code. |
| `unsupported_grant_type` | Use `authorization_code` or `client_credentials`. |

For API requests, a `401` can mean the token is no longer valid; let the member
authorize again. A `403` can mean insufficient scopes, account restrictions, or
server policy. Check the response's `error` field before retrying.
