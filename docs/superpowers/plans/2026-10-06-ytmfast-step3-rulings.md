# ytmfast step 3 (browsing): rulings

Every ruling made while building step 3, in order, with what it costs if wrong; then the small findings that were
deferred or parked, so step 4 can pick them up. Step 1 and step 2 rulings are in their own files next to this one.

## Rulings

- Ruling P1: the liked status comes from a dedicated WEB_REMIX `next {videoId}` for the current song when the queue fetch didn't return it (one small request per song start, off the engine loop, cached per video id) — why: playlist-only queue fetches don't carry the current song's like state — cost if wrong: one extra request per song.
- Ruling P2: T1 (capture, scrub, golden) and T8 (live checks) are controller-run; implementers never use the real account — why: user's machine rules.
- Ruling P3: token/params charset must allow standard base64 too: [A-Za-z0-9_%=+/-]{1,4096} (YouTube continuation tokens and params can hold + and /; blanking them would silently stop paging and filters) — goes into Task 2's fix round — cost if wrong: none.
- Ruling P4: the golden test normalises only the expected side (play endpoints trimmed to videoId/playlistId/index/params; loose search sections get cont "" and more null) — the widget only uses those fields and checks them for truth — cost if wrong: a widget field we dropped.
- Ruling P5: a client-sent endpoint with a bad videoId/playlistId/index/params is refused (bad_request), not silently cleaned; unknown keys are still dropped — why: cleaning could turn "this song in this list" into "the whole list from the top" — cost if wrong: a stricter widget contract.
- Ruling P6: lyrics {videoId} over the socket belongs to Task 6 — plan split.
- Ruling P7: playPage must not override a newer play: if any play-type command arrives after a playPage was issued (from any client), the playPage's own play is dropped when its browse lands (engine-side play generation check) — latest user action wins — goes into Task 4's fix round — cost if wrong: none.
- Ruling P8: a lone-song watchEndpoint drops its params/index and plays the song with its radio (step-2 behaviour) — why: search rows carry song-level params that mean nothing for a radio; the engine supports params when a list is given — cost if wrong: a lone-song play ignores YouTube's hint.
- Ruling P9: next/previous also cancel a pending playPage play (any user transport action after the click means they moved on) — cost if wrong: a slow artist page press followed by Next doesn't start the artist.
- Ruling P10: like counts toward the 4-per-client browsing limit — fine.
- Ruling P11: Task 7 also takes the cheap, clear deferred minors from Tasks 3-6 (task-7-extras.md, 10 items); parked: in-flight next dedupe, tab-only Likes slots, like refetch in cache, PlayPage helper, full-disconnect, panic-branch test — why: they're small and the final review would ask anyway; the parked ones need step 4's widget design or are cosmetic — cost if wrong: a bigger Task 7 diff.
- Ruling P12: search queries keep ZWNJ, ZWJ and the flag tag characters (Persian, Indic scripts and emoji need them); all other Cf, Zl and Zp are refused — why: refusing them would block real searches — cost if wrong: an invisible joiner can still reach a query (harmless, YouTube gets the same text the user typed).
- Ruling P13: a `play {}` resume counts toward the playPage epoch only from Stopped (it starts something there, like a Toggle from Stopped); from Paused/Playing/Buffering it doesn't — cost if wrong: a resume from Stopped cancels a pending artist-page play.
- Ruling P14: allow thumbnails on exactly https://www.gstatic.com/youtube/media/ytm/images/ (YouTube Music's own tile art: Liked songs, podcast queue) — thumbnail check only, NOT net::allowed_host (ytmfast never requests them; the widget does) — why: Page.js shows them and the live parity check found them blank — cost if wrong: one more image host the widget loads.
- Ruling P15: add `albumId` (the byline's MUSIC_PAGE_TYPE_ALBUM browse id, shape-checked, "" when none) to queue items and status, kept in state.json with a serde default — why: the user's pick "cover click opens the album" needs it in step 4 and step 4 is meant to switch in one go — cost if wrong: one more field.
- Ruling P16: final fix wave = README status, benchmarks parity list (18 pages), P15 albumId, minors 1 (seek past the end and removing the current song count as picks for the playPage epoch), 2 (lyrics text capped before caching), 5 (an over-long continuation token is logged by code), 7 (refuse an endpoint carrying both keys). Parked: minor 3 (a failed newest like after a succeeded older one shows the old state until the song restarts; rare, asker gets the error), 4 (nothing-to-play stays bad_request, documented), 6 (doubled log line, cosmetic) — cost if wrong: small.

## Deferred and parked findings

Most of the Task 2-6 items below were closed in Task 7 or the final fix wave (see the git log); the final review
triaged every one. What is still open is marked parked.

- Task 2: minor (deferred): width:null thumbnail and non-string simpleText edge cases differ from JS (harmless); kind last in Row (Page.js order)
- Task 3: minor (deferred -> Task 7): refuse Zl/Zp and bidi/zero-width format chars in search queries (browse.rs:318); add a wiremock test that a Set-Cookie on browse/like reaches the session
- Task 4: minor (deferred -> Task 7): count Toggle-from-Stopped as a play, and don't count a no-id resume (play {} / MPRIS Play) for the playPage epoch; full client disconnect looks like half-close (browsing tasks run to completion, ≤~20 s); untested panic branch
- Task 5: carry to Task 7 (consider): two in-flight likes on one song -> last answer wins (should be last request); like status not refetched within the 100-song cache (phone changes show late); a mixer change racing a mute can mark unmuted while silent.
- Task 5: minor (deferred -> Task 7): second like lookup after an output-restart replay (keep liked_asked across a replay); close the mixer-vs-mute race by re-sending set_volume(v) in the muted branch; make a_late_like_status_is_dropped deterministic (gate the fake); track/abort the like task at quit; document "videoId": null = the song shown
- Task 6: carry to Task 7 (consider): lyrics asked before the song's first `next` lands -> two `next` requests (no in-flight dedupe); two clients asking the same uncached song both fetch; a newer "no tab" replaces an older tab. QueueSource::like_status is now song_next -> SongNext {like, lyrics_tab}.
- Task 6: minor (deferred -> Task 7, consider): tab-only entries take slots in the 100-song Likes list; keep a known tab over a fresh "no tab"; optional helper for the PlayPage arm to undo the rustfmt reindent; dedupe in-flight `next` matters only if step 4 asks for lyrics right at song change
- Task 7: minor (deferred): if the newest like fails while an older one succeeded, the shown status lags until the next lookup (follows newest-wins); two tests keep 50 ms negative waits (module pattern). Fix round 1 base 95ef946.
- Final review: fix wave re-review: all 7 addressed. Parked (no second wave): the lyrics cap's "room for JSON escapes" comment holds only to ~4 bytes/char (all-control-char lyrics could still exceed MAX_LINE; freak case); removing the current song while Stopped counts as a pick and drops a pending playPage (rare); one 153-char doc line in engine.rs:151; a non-string continuation token ends a list without a log line; the lyrics source line is uncapped (bounded by the 32 MiB answer cap). — Ruling: real, rare, step 4 polish.
