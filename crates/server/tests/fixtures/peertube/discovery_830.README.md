# PeerTube video discovery contracts

`discovery_videos.rs` builds minimal local fixtures from PeerTube 8.3.0 source
commit `81bf3179bbdddbd6bfb431aeac4320e8f7c87c03`. No live server, credentials or
media are required. Values and names are fictional; fields are based on:

- `packages/models/src/videos/video.model.ts` and `video-details.model.ts`:
  canonical `url`, `uuid`, `isLocal`, `isLive`, `privacy.id`, `state.id`, `nsfw`,
  `duration`, `account.url`, and `channel.url`.
- `server/core/controllers/api/videos/index.ts`: anonymous public listing and
  individual video access checks; bounded `start`, `count`, `isLocal`, `isLive`,
  `sort` and `nsfw` query parameters.
- `server/core/models/video/formatter/video-api-format.ts` and
  `video-activity-pub-format.ts`: canonical identity, owner/channel attribution,
  public delivery, publication state, sensitive content and duration. Both
  classic two-actor attribution and owner-only attribution are exercised.
- `server/core/models/video/video-channel.ts`: canonical Group and its owner.
- `video-state.enum.ts` and `video-privacy.enum.ts`: published = 1 and public = 1;
  unlisted, private, internal and password access are excluded.

Source: <https://github.com/Chocobozzz/PeerTube/tree/81bf3179bbdddbd6bfb431aeac4320e8f7c87c03>.
The existing `create_video.json` fixture covers richer native ingestion/playback
metadata separately. Discovery deliberately fetches no media or thumbnails.
