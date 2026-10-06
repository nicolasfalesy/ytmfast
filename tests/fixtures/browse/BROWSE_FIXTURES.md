# Browse and search fixtures

Real YouTube Music answers (WEB_REMIX client, signed in, 2026-10-06), scrubbed for a public repo:

- Every key is kept. Every string value is replaced: ids keep their YouTube prefix (`MPREb_`, `UC`, `VL`,
  `OLAK5uy_`, `MPSP`, `MPLYt`…) followed by `fake…`, so the "kind" rules still work; text becomes `Text N`;
  tokens and params become `faketoken`; picture links become one fake googleusercontent link, or one fake
  `i.ytimg.com/vi/…/hqdefault.jpg` link (with `?sqp=fake` when the original had it) so the thumbnail rules
  are exercised.
- Kept as they were: YouTube's own labels (values with an underscore such as `MUSIC_PAGE_TYPE_ALBUM`, and the
  values of label fields such as `iconType`, `likeStatus`, `status`, `privacy`), `m:ss` durations, separators,
  and small numbers outside text.
- Dropped: `trackingParams`, `clickTrackingParams`, `responseContext`, `frameworkUpdates`, logging blocks.
- Lists are trimmed to 12 entries.

`expected/` holds what the widget's `Page.js` returns for each fixture. It is made by `golden.mjs`, which runs
the `SOURCE` string of `Page.js` (from the omarchy-youtube-music repo) in deno with a fake `fetch`:

```sh
deno run --allow-read --allow-write golden.mjs . expected path/to/pagejs_source.js
```

`pagejs_source.js` is the text between `String.raw\`` and the closing backtick in `Page.js`.

| Fixture | Request | Golden call |
|---|---|---|
| `browse_home`, `browse_library_landing`, `browse_liked_playlists`, `browse_liked_albums`, `browse_library_corpus_track_artists`, `browse_playlist`, `browse_album`, `browse_artist`, `browse_podcast` | `browse {browseId}` | `browse(id)` |
| `*_cont` (browse) | `browse {continuation}` | `more("/browse", token)` |
| `search_mixed` | `search {query}` | `search(q)` |
| `search_songs`, `search_albums`, `search_podcasts` | `search {query, params}` | `search(q, params)` |
| `search_songs_cont` | `search {continuation}` | `more("/search", token)` |
| `next_song_for_lyrics` + `browse_lyrics` | `next {videoId}` then `browse {MPLYt…}` | `lyrics(videoId)` |
| `next_liked_song`, `next_not_liked_song` | `next {videoId}` | like status at `playerOverlays.playerOverlayRenderer.actions[].likeButtonRenderer.likeStatus` (`LIKE` / `INDIFFERENT`) |
