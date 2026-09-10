# Test Webxdc in a browser

Use this guide to check the invitation flow, app isolation, and realtime
exchange through the actual web interface. Record the server revisions,
browser versions, app package versions, and failed steps with the test results.

## Prepare

Use two Plamenu servers with separate databases and identities, working
federation, and trusted HTTPS for their social and wildcard app domains.
The [local DNS setup](peers/webxdc-dns/README.md) covers wildcard names for
`plamenu.local` and `plamenu2.local`. The second E2E instance normally exists
only during a test run; keep both instances running for the browser session.

Use separate browser profiles for an owner, another local account, and an
account on the second server. Use a signed-out profile for guest checks.
Have a realtime-capable `.xdc` package, such as Realtime Check or Realtime Chat,
and the [asset](fixtures/webxdc-assets/README.md) and
[pointer-lock](fixtures/webxdc-pointer-lock/README.md) fixtures ready to upload.

## Create, invite, and join

1. Create a session through Apps and open it from the session list. Check that
   the app loads on its own origin under the server's Webxdc domain.
2. Choose **Invite people**. Check that the composer contains an editable
   session link and an invitation card; preview and publish it with a mention
   of the remote account.
3. On the other server, open the invitation from notifications, continue to
   the session, join, and open the app. The cached app should run on the
   receiving server's Webxdc domain.
4. Create a session that requires guest approval. Request access from the
   signed-out profile, approve it as the owner, and open the app as the guest.
5. Repeat the session page, invitation composer, and player checks at a narrow
   mobile viewport. Check long session URLs, visible controls, and horizontal
   overflow. Test the player's fullscreen button and exit using browser controls.

## Realtime exchange

Run each pairing with both participants' apps open:

| Participants | Expected result |
| --- | --- |
| Accounts on different servers | Both discover each other and receive live messages. |
| Two accounts on one server | Both receive live messages. |
| Account and guest | Both receive live messages after any required approval. |

With Realtime Check, use its discovery and ping controls. In Realtime Chat,
type an unsubmitted draft to exercise ephemeral messages; Enter also submits
a durable update. Use keyboard input so the app's event handlers run.

Close one recipient's app, type a distinctive draft in the sender's app, and
clear it before reopening the recipient. The old draft should not arrive.
Type a new draft and check that it does arrive. Apps may send fresh presence
or draft state on reconnect, so distinguish that from replay of the cleared
message.

Remove a guest while their app is open, then close the session with a remote
participant still connected. Check that the affected players stop and display
an access-ended or session-ended notice.

For storage checks, compare durable update and delivery-queue rows before and
after live drafts. Ephemeral messages should not create durable updates or
`WebxdcEphemeral` delivery jobs. Allow for an app's own durable startup
metadata; Realtime Chat records participant names when joining.

## Files and browser permissions

Run the asset fixture and check all five results, including both local image
decoding and rejection of requests to the parent server. Repeat after opening
the session on the remote server.

Run the pointer-lock fixture in a foreground browser, as both an account and
a guest. Check capture, movement at screen edges, release, and fullscreen.
Background automation or an embedded browser may reject pointer capture;
record that limitation and repeat in a foreground standalone browser.

Upload a package representative of the configured size limit. Check creation,
remote fetching, and startup. If it is a game, test gameplay separately from
whether its menu loads. An invalid ZIP or oversized selection should show an
understandable error and preserve the form's other fields.

## Storage administration

On a disposable test server:

1. Create two sessions from the same package. Check that the storage view
   reports two references without counting the package bytes twice.
2. End and delete one session. Check that the other still opens its app.
3. Join a remote session, then remove its local cache through administration.
   Check that the coordinator no longer lists the local participant.
4. Change the package limit and total storage quota, check their effect on
   uploads, and restore the previous settings. Verify that an ordinary account
   cannot use the administrator's session-management actions.

Remove sessions and posts created for the check. Keep results outside this
procedure so another tester can follow it without reading earlier runs.
