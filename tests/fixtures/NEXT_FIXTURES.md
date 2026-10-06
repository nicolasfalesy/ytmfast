# `next` fixtures

Real YouTube Music `next` answers (WEB_REMIX client, signed in, 2026-10-05), reduced to an allowlist of the keys a
queue parser needs. Every string value was replaced: ids by `fake…` values (consistently, so cross-references
still match), free text by `Text N`, tokens by `faketoken`, image links by one fake googleusercontent link. Only
YouTube's own enum values (`MUSIC_PAGE_TYPE_…`) and durations (`m:ss`) are kept. Long queues are trimmed to 12 items.

| File | Request |
|---|---|
| `next_album.json` | `{"playlistId": <album playlist>}` (playlist id only) |
| `next_radio.json` | `{"videoId": <song>, "playlistId": "RDAMVM<song>"}` |
| `next_radio_continuation.json` | `{"continuation": <nextRadioContinuationData.continuation of the radio answer>}` |
| `next_liked.json` | `{"playlistId": "LM"}` |

Shapes worth knowing:

- Queue items come either as `playlistPanelVideoRenderer`, or wrapped as `playlistPanelVideoWrapperRenderer`
  with `primaryRenderer` (the song) and `counterpart[].counterpartRenderer` (the music-video version). Take the
  primary.
- A queue can end with an `automixPreviewVideoRenderer`, which is not a song.
- Asking for an album with `videoId` plus the album `playlistId` gave only the current song plus an automix
  preview. Asking with the album `playlistId` alone gave the album's tracks.
- A continuation answer sits under `continuationContents.playlistPanelContinuation`.
