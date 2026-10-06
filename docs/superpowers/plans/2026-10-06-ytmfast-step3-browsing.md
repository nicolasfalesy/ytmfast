# ytmfast step 3 ("browsing") Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** ytmfast can do everything the bar widget's panel needs: Home, the Library pages, album, playlist,
artist and podcast pages, search with filters, long lists that load as you scroll, playing any row or page,
like and dislike, mute, and YouTube Music's plain lyrics. With this, step 4 can switch the widget fully to
ytmfast in one go, and pear-desktop is no longer needed.

**Architecture:** A new `browse` module turns YouTube Music's `browse` and `search` answers into the same small
row, section and page shapes the widget gets from `Page.js` today. That way the widget's list code doesn't
change in step 4. `innertube` gains the WEB_REMIX `browse`, `search`, `like/*` requests. The engine and the
socket gain the matching commands. Playing a row takes the row's own `play` endpoint, so the widget never has
to know YouTube's endpoint rules.

**Tech Stack:** as steps 1 and 2. No new crates unless a task says so.

**Spec:** `docs/superpowers/specs/2026-10-04-ytmfast-design.md` (innertube section, feature parity checklist,
build step 3). Earlier decisions: `docs/superpowers/plans/2026-10-05-ytmfast-step1-rulings.md` and
`docs/superpowers/plans/2026-10-06-ytmfast-step2-rulings.md`, whose "Parked for step 3" list Task 7 closes.

## Global Constraints

- Everything in the step 1 and step 2 Global Constraints still holds: public-repo wording, noreply commits,
  gitleaks, never push red and watch CI, https and the host allowlist, size caps, no URLs, cookies or tokens in
  errors, logs or events, keyring-only session, queue cap 1,000.
- Browsing uses the WEB_REMIX client on music.youtube.com, the same as `next`.
- **Output shapes match `Page.js` exactly** (field names, types, empty strings rather than null, order). This
  is the contract the step 4 widget relies on:
  - Row: `{kind, title, subtitle, thumb, videoId, setId, playlistId, browseId, params, play, duration}`.
    `kind` is one of `song | album | artist | playlist | podcast | page | ""`. `play` is `null` or one endpoint
    `{watchEndpoint: {videoId?, playlistId?, index?, params?}}` or `{watchPlaylistEndpoint: {playlistId, params?}}`,
    holding only those fields.
  - Section: `{title, items: [Row], cont, more: null | {browseId, params}}`.
  - Page (browse): `{header: {title, subtitle, thumb, play}, sections: [Section], cont}`.
  - Search: `{sections: [Section], chips: [{label, params}]}`.
  - More (a continuation): `{items: [Row], sections: [Section], cont}`.
  - Lyrics: `{text, source}` or `{none: true}`.
- The `Page.js` rules carry over one for one, each with its "why" comment ported:
  - **Thumbnails:** pick near 120 px (the last one ≤ 226 px wide); turn `//` links into https; swap
    `i.ytimg.com/vi/…/hqdefault.jpg` without `sqp=` for `mqdefault.jpg`.
  - **Separator:** `" • "`.
  - **Rows that do nothing are dropped.** Rows are deduped by `kind:setId|videoId|browseId|playlistId:title`.
    Item caps: 30 per section for mixed search, 300 for filtered search and browse, 1,000 for a continuation.
  - **Next pages:** older `continuations[0].nextContinuationData`, or newer `continuationItemRenderer` (incl.
    the `commandExecutorCommand` form). Never `reloadContinuationData`. Never a playlist page's
    two-column Suggestions.
  - **"More" links:** browse-only, from `bottomEndpoint` or the header's `moreContentButton`. Title links are
    not used.
  - **Search:** untitled shelves are named "Top result" and "More results". The card shelf's title is
    "Top result".
  - **Album rows:** rows without art take the page header's thumbnail.
  - **Headers:** only `musicResponsiveHeaderRenderer`, `musicImmersiveHeaderRenderer`,
    `musicVisualHeaderRenderer`, `musicDetailHeaderRenderer` and `musicHeaderRenderer`.
- Every thumbnail passes `net::allowed_host` or becomes `""`. Every id (video, playlist, browse) and every
  `params` or token is checked for shape (charset and length) before it is used or sent back out.
- Browsing never starts playback by itself, and never touches the queue unless a `play…` command says so.
- Answers stay small: no raw YouTube JSON ever reaches a client.

## Review Focus

1. **A page with nothing playable** (a "New playlist" tile, an empty library section) gives an empty list or
   a header-only page, never an error and never a crash (Task 2 test `empty_and_nothing_playable_pages`).
2. **Continuations stop at the end.** The last page gives `cont: ""`. A playlist's Suggestions and a
   `reloadContinuationData` never produce a next page (Task 2 test `no_suggestions_or_reload_continuations`).
3. **A row's play endpoint with extra or odd fields** (a huge `index`, an unknown key, a bad id) is cleaned
   or refused, never passed through raw (Task 4 test `play_endpoint_is_sanitised`).
4. **Like on a song that isn't the current one** (a row in a list) works by video id. A like while nothing
   plays needs an explicit id (Task 5 test `like_by_video_id`).
5. **The output shape never drifts from `Page.js`.** A golden test compares ytmfast's output on every
   fixture with the JSON `Page.js` itself produces on the same fixture. `Page.js` is run once by the
   controller in deno to make the expected files (Task 2 test `matches_page_js_golden`).

---

### Task 1: Capture and scrub real answers (controller, no code)

**Files:**
- Create: `tests/fixtures/browse/*.json`, `tests/fixtures/browse/BROWSE_FIXTURES.md`, `tests/fixtures/browse/expected/*.json`

- [ ] **Step 1:** With the user's session, capture WEB_REMIX answers for:
  - `browse` of `FEmusic_home` and one of its continuations;
  - `FEmusic_library_landing`, `FEmusic_liked_playlists`, `FEmusic_liked_albums`,
    `FEmusic_library_corpus_track_artists` (one continuation each where there is one);
  - a playlist `VL…` and one continuation;
  - an album `MPREb…`, an artist `UC…`, a podcast `MPSP…`;
  - `search` mixed, then filtered by Songs and Albums, and a filtered continuation;
  - the lyrics path: `next` for a song, then its `MPLYt…` browse;
  - the `next` answer's like status for a liked song and a not-liked one.
- [ ] **Step 2: Scrub.** Keep every key. Replace every string value with a made-up one (ids consistently,
  text as `Text N`, tokens as `faketoken`, links as one fake googleusercontent link and one fake
  `i.ytimg.com/vi/<fake>/hqdefault.jpg` link so the thumbnail rule is exercised). Keep only enum-like values
  (`^[A-Z0-9_]+$`), `m:ss` durations and separators. Drop `trackingParams` and `clickTrackingParams`. Prove
  that no original string of 4 or more characters survives.
- [ ] **Step 3: Golden output.** Run the widget's `Page.js` (the `collect`, `headerOf`, `search`, `browse`,
  `more` and `lyrics` logic) in deno over each scrubbed fixture, with a fake `fetch` that returns the fixture.
  Save the results as `expected/*.json`.
- [ ] **Step 4:** gitleaks, and a grep for real ids, both clean. Commit `test: scrubbed browse and search fixtures with Page.js golden output`.

### Task 2: The `browse` module (pure parsing)

**Files:**
- Create: `src/browse/mod.rs`, `src/browse/row.rs`, `src/browse/collect.rs`
- Test: `tests/browse_parse.rs`

**Interfaces:**
- Produces: `Row`, `Section`, `PageHeader`, `Page`, `SearchPage`, `MorePage`, `Chip` and `Endpoint` (an enum of the two endpoint kinds), all serde with the exact field names and shapes in Global Constraints.
- Produces: `parse_browse(&Value) -> Page`, `parse_search(&Value, filtered: bool) -> SearchPage`, `parse_more(&Value) -> MorePage`, `parse_lyrics_tab(&Value) -> Option<String>` (the `MPLYt` browse id from a `next` answer) and `parse_lyrics(&Value) -> Option<(String, String)>`.
- Produces: `parse_like_status(&Value) -> Option<LikeStatus>`, with `LikeStatus` = `Like | Dislike | Indifferent`.

- [ ] **Step 1: Failing tests:**
  - `matches_page_js_golden`: every fixture, output equal to `expected/*.json` (Review Focus 5).
  - `empty_and_nothing_playable_pages` (Review Focus 1).
  - `no_suggestions_or_reload_continuations` (Review Focus 2).
  - `thumbnail_rules`.
  - `dedupe_keeps_playlist_repeats_by_set_id`.
  - `search_names_untitled_shelves`.
  - `album_rows_take_header_art`.
  - `ids_and_tokens_are_shape_checked`.
- [ ] **Step 2: Run.** FAIL. **Step 3: Implement** by porting `Page.js` rule for rule, with its comments. **Step 4: Run.** PASS. **Commit** `feat: browse and search parsing`.

### Task 3: Requests (`innertube`)

**Files:**
- Modify: `src/innertube/mod.rs`
- Create: `src/innertube/browse.rs`
- Test: `tests/innertube_browse.rs` (wiremock)

**Interfaces:**
- Produces: `Innertube::browse(browse_id, params: Option<&str>) -> Result<Page, Error>`, `search(query, params: Option<&str>) -> Result<SearchPage, Error>`, `more(kind: MoreKind /* Browse | Search */, token) -> Result<MorePage, Error>`, `like(video_id, LikeStatus) -> Result<(), Error>` and `lyrics(video_id) -> Result<Option<(String, String)>, Error>`.

- [ ] **Step 1: Failing tests:**
  - `browse_request_shape`: WEB_REMIX headers, body `{browseId, params?}`.
  - `search_request_shape`: body `{query, params?}`.
  - `continuation_request_shape`: body `{continuation}`.
  - `like_endpoints`: `like/like`, `like/dislike` and `like/removelike` with body `{target: {videoId}}`.
  - `lyrics_two_requests`.
  - `query_length_capped`: 1 to 200 characters, trimmed.
  - `answer_size_capped`.
  - `errors_carry_no_ids_or_tokens`.
- [ ] **Step 2: Run.** FAIL. **Step 3: Implement** with the step 2 shared posting path (`post`, `read_capped`, Set-Cookie, allowlist). **Step 4: Run.** PASS. **Commit** `feat(innertube): browse, search, like and lyrics requests`.

### Task 4: Engine and socket commands

**Files:**
- Modify: `src/engine.rs`, `src/control/protocol.rs`, `src/control/mod.rs`, `docs/protocol.md`
- Test: `tests/control_browse.rs`, plus engine unit tests

**Interfaces:**
- Socket commands (camelCase, replies carry the shapes above):
  - `browse {browseId, params?}` gives a Page.
  - `search {query, params?}` gives a SearchPage.
  - `more {kind: "browse" | "search", token}` gives a MorePage.
  - `play {endpoint}` starts playback from a row's endpoint. A `watchEndpoint` with a `playlistId` plays the list at that song. A `watchPlaylistEndpoint` plays the list, using `params` (for example an artist's shuffle).
  - `playPage {browseId, params?}` plays a page's header button, or else its first playable row.
  - `lyrics {videoId}` gives Lyrics.
- Engine: `Play` gains `params: Option<String>`, passed to `next` (NextRequest gains `params`).
- Browsing runs off the engine loop, in the same way the queue source does. Answers go straight back to the client that asked. Nothing is broadcast.

- [ ] **Step 1: Failing tests:**
  - `play_endpoint_is_sanitised` (Review Focus 3).
  - `play_watch_endpoint_with_playlist_plays_at_song`.
  - `play_watch_playlist_endpoint_uses_params`.
  - `play_page_uses_header_button_then_first_row`.
  - `browse_reply_goes_only_to_the_asker`.
  - `browse_doesnt_touch_the_queue`.
  - `bad_ids_are_bad_request`.
- [ ] **Step 2: Run.** FAIL. **Step 3: Implement.** **Step 4: Run.** PASS. **Commit** `feat: browse, search and play from rows over the socket`.

### Task 5: Like, dislike and mute

**Files:**
- Modify: `src/engine.rs`, `src/control/*`, `src/mpris.rs` (none needed beyond status), `docs/protocol.md`
- Test: engine unit tests, `tests/control.rs`

**Interfaces:**
- Socket: `like {status: "like" | "dislike" | "none", videoId?}`. Without a `videoId` it applies to the current song.
- Socket: `mute {on: bool}`. Mute keeps the volume and silences the stream; unmute restores it.
- Status gains `liked: "like" | "dislike" | "none" | null`, read from the current song's `next` answer (the queue fetch already makes that request; capture it there). An answered `like` updates it at once.
- Status gains `muted: bool`.

- [ ] **Step 1: Failing tests:**
  - `like_by_video_id` (Review Focus 4).
  - `like_current_updates_status`.
  - `like_without_song_needs_id`.
  - `liked_comes_from_next_answer`.
  - `mute_keeps_volume_and_restores`.
  - `mute_survives_a_restart` (saved in `state.json`).
- [ ] **Step 2: Run.** FAIL. **Step 3: Implement.** **Step 4: Run.** PASS. **Commit** `feat: like, dislike and mute`.

### Task 6: Lyrics

**Files:**
- Modify: `src/engine.rs` or `src/control/mod.rs` (lyrics needs no engine state), `docs/protocol.md`
- Test: `tests/control_browse.rs`

- [ ] **Step 1: Failing tests:**
  - `lyrics_found`: text and source (the shelf footer, for example "Source: Musixmatch").
  - `lyrics_none`.
  - `lyrics_cached_per_video` (the last 20, so reopening the tab costs nothing).
- [ ] **Step 2: Run.** FAIL. **Step 3: Implement.** **Step 4: Run.** PASS. **Commit** `feat: plain lyrics`.

### Task 7: Step 2 parked fixes

**Files:**
- Modify: `src/innertube/player.rs`, `src/engine.rs`, `src/queue.rs`, `docs/benchmarks.md`

- [ ] **Step 1: Failing tests:**
  - `this_video_is_private_skips`: match "private" alone.
  - `adds_during_load_respect_the_cap`.
  - `failed_unpicked_list_keeps_a_playing_song`.
- [ ] **Step 2: Implement.** Also refresh the "Side by side" table in `docs/benchmarks.md` to step 2's numbers.
- [ ] **Step 3: Run.** PASS. **Commit** `fix: step 2 parked items`.

### Task 8: Live checks, numbers, publish (controller)

- [ ] **Step 1:** Live, against the user's account, at volume 0. Each result must match what pear-desktop's
  panel shows for the same page: open Home and scroll two pages; open Library, Liked songs (scroll to the end),
  an album, an artist and its "Show all"; search "noah kahan" mixed and filtered by Songs; play a row from each;
  play an artist page; like then un-like a song and check it in YouTube Music; mute and unmute; plain lyrics for
  a song.
- [ ] **Step 2:** Time each request: browse, search and more, cold and warm. Measure the memory added by a big
  list. Add these to `docs/benchmarks.md`.
- [ ] **Step 3:** Checks green, gitleaks clean, push, and watch CI to green. Then the final whole-branch review.
