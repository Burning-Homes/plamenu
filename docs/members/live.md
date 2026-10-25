# Live broadcasting

In **Video studio → Create live broadcast**, select a channel, title,
description, audience, category/licence and content warning. Choose the offered
DVR/latency and recording options. Keep the broadcast management page private:
encoder credentials allow someone to transmit as your broadcast.

## Camera or screen

Open the browser broadcast controls on a supported HTTPS browser, grant the
needed device permissions and preview your camera/screen. A preview alone does
not transmit. Choose **Go live** when ready. **Cancel preview** releases capture;
**Stop broadcasting** ends transmission. Browser capture requires JavaScript
and support for the selected device/format.

## External encoder

Copy the server URL and stream key from the management page into your encoder.
Use the advertised RTMP, RTMPS or HTTPS-FLV option supported by your operator and
encoder. Do not guess a public ingest address from the website URL: the operator
must configure network reachability and TLS. Treat stream keys as passwords;
rotate exposed credentials using the broadcast controls.

Check the waiting/player page from another browser, start the encoder, then
verify picture, sound, captions/alternatives and the chosen audience. Stop the
encoder deliberately when finished and wait for any recording finalization.
Channel permission changes or suspension can terminate broadcasting authority.

## Announcements and recordings

Planned starts announce a time; they do not start your camera or encoder.
The waiting/management page offers calendar subscription and reminder links.
Subscribe for later changes; a one-time calendar import may stay unchanged.
The calendar app delivers reminders. The channel calendar includes public
broadcasts; an unlisted broadcast's schedule requires its direct link.

A one-shot broadcast keeps its publication for the recording. A reusable
broadcast can create separate recordings per session. Check each recording's
warning, licence and alternatives: parent settings can change after it was
prepared. Waiting reusable broadcasts must be retired before a channel transfer.

DVR availability and latency depend on the chosen mode, source and server
resources. On interruption, reload the broadcast's state before reconnecting;
a restored server does not mean a previous encoder is still on air. Operators
should inspect [live recovery and storage](../operators/media.md).
Peer-assisted playback has the same [privacy choice](video.md#watch-privately)
as recorded video. See [known limits](../KNOWN_ISSUES.md) for qualification gaps.
