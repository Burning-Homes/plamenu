# Apps and Webxdc

**Apps** hosts shared Webxdc sessions: small packaged applications whose
participants exchange updates. A saved app and a running session are separate.

1. Open **Apps → Create session**.
2. Choose a personal or instance app, or upload a `.xdc` package.
3. Give the session a name and description. Under **Advanced settings**, choose
   whether anyone with the invitation link can join or you approve requests.
4. Create the session, open it and share its invitation link with participants.

The session page provides join, leave and owner management controls. Some
sessions allow guests; use only the access offered by that session. Membership
controls collaboration, not end-to-end encryption or secrecy of the package.
The app needs JavaScript; normal session management uses HTML forms.

## Keep reusable apps

Your app library lets you save packages and upload later versions. Choose a
saved app when starting another session. Each session pins the version it
started with: updating or removing a library entry does not upgrade or delete
existing sessions. Removing a session and removing an app are separate actions.

External catalogues are selected by administrators. Browsing a catalogue does
not install anything. Importing an entry validates and saves its package;
canonical metadata comes from that package. Catalogue refreshes and package
imports are deliberate actions, not automatic software updates.

Apps run on isolated Webxdc origins. Your server needs working DNS and TLS for
those origins; a blank or unreachable app can be an installation problem.
Use packages you trust, and report suspicious apps to your administrators.
Library and storage limits may prevent new uploads or updates; removing the
last reference to a package allows its storage to be reclaimed.

Administrators: see [app moderation and quotas](../operators/administration.md#webxdc-apps).
Integrators: see [catalogue and version semantics](../WEBXDC-LIBRARY.md) and the
[session protocol](../fep/fep-752d.md).
