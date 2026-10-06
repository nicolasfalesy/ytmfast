# Step 2: decisions made during the build

Every ruling made while building step 2, in order, with what it costs if wrong; the user's own picks; and the known gaps parked for step 3.

## Rulings

- Ruling S1: T4 updates every existing caller of EngineCmd::Play / EngineEvent::Position in control and mpris just enough to compile and keep step 1 behaviour (seeked flag false on ticks, true on seek); T8 adds the new commands — why: each task must leave the build green — cost if wrong: small churn in T8.
- Ruling S2: T4 plays the next item through a normal load on Ended (not gapless); T5 adds AudioPlayer::preload and switches the engine to preload 10 s before the end — why: keeps T4 testable without the audio change — cost if wrong: none.
- Ruling S3: T7's spike (step 1) is run by the controller before T7 is dispatched; its findings go in this ledger and into the T7 dispatch — why: needs the real account and history — cost if wrong: none.
- Ruling S4: T9 revises step-1 ruling R26 (no-audio-format / different-video / TV-only refusal / bot check now fall back to yt-dlp; SignedOut never does) — why: parked step-1 findings and the plan — cost if wrong: a few extra yt-dlp runs.
- Ruling S5: T1, the T7 spike and T10 are controller-run; implementers never use the real account, keyring, speakers, session bus or systemd units — why: user's machine rules — cost if wrong: controller time.
- Ruling S6: artists = all names before the first " • " plus any later MUSIC_PAGE_TYPE_ARTIST link (covers unlinked "A & B" and user uploads) — why: the plan's rule dropped artists on real items — cost if wrong: an album/year run read as an artist on an odd item.
- Ruling S7: next body sends only isAudioOnly (matches how the fixtures were captured); production uses Innertube::production (per-client api_host); Innertube::new(…, base) is test-only; "no queue" -> Unavailable("YouTube sent no queue"), which radio refill treats as "no more songs" — why: implementer choices that fit the plan — cost if wrong: small.
- Ruling S8: Repeat::One + a manual skip at the queue end stops (like Off); move_to while shuffled only reorders the shuffled order; replace with a bad start index starts at 0; add to an empty queue leaves no current; re-enabling shuffle doesn't reshuffle — why: sensible defaults the plan didn't fix — cost if wrong: one-line changes each.
- Ruling S9: a play by id alone also fetches its radio (in parallel with the link); a radio page with no new songs = no more songs; a failed continuation of a non-radio list (e.g. Liked songs) does not fall back to radio — why: implementer choices consistent with the user's "radio at queue end" pick and the no-loop rule — cost if wrong: Liked songs end instead of continuing into radio.
- Ruling S11: state.json may hold public thumbnail links (re-validated on load); "never holds a URL" means signed stream links and anything credential-like — why: album art must survive a restart and thumbnails are public — cost if wrong: drop thumbnails before saving (one line).
- Ruling S12: Task 7 makes one extra WEB_REMIX player request per song (for its tracking URLs + visitorData), sent when the song really starts (not at prefetch), and sends the playback ping exactly per variant 2; watch-time pings follow the plan's cadence on videostatsWatchtimeUrl with the same headers (+ st/et/cmt/len/state as in the plan) but are best-effort — history only needs the playback ping — cost if wrong: one ~150 ms request per song, off the audio path.
- Ruling S13: Reporter::start(video_id, cpn, at, length) fetches its own tracking links in the play's task (not start(tracking, cpn)); Engine::download_from_test_base test hook (R7 pattern); an output-restart replay keeps its cpn; a play is dropped only if it ends with <1 s played before its links arrive — why: keeps the extra request off the engine loop; sensible edges — cost if wrong: small.
- Ruling S14: Previous >3 s restarts by seek(0) within the same play (same cpn, no new playback ping), like the official player's seek-to-zero — cost if wrong: a restarted song counts once.
- Ruling S15: the live queue is capped at 1,000 items: a socket/MPRIS add that would pass it is refused with bad_request "the queue is full"; radio/continuation refills first drop already-played items from the front (keeping 50 played items behind the current one) and stop appending at the cap — why: keeps every queue event well under the 1 MiB line cap (~375 B/item) and memory bounded — cost if wrong: very long sessions lose their oldest played history in the queue view.

## The user's picks

- User picks (2026-10-05): radio when the queue runs out; Previous restarts after 3 s; subagent-driven; helpers on Opus.
- User pick (2026-10-05 23:2x): with shuffle on, later batches (Liked songs pages, radio) are shuffled into the songs still to come. -> Ruling S10: Task 9 implements it in Queue::append_radio (shuffle new songs into the upcoming part of the shuffled order; original order keeps them appended) with tests — cost if wrong: none (user's pick).
- User pick (2026-10-06): MPRIS CanGoPrevious stays "only when there's a song before" (as built).
- User picks (2026-10-06 ~10:00): own "add to end" while shuffled stays MIXED into what's left (as built, no change); a shuffled play starts with a RANDOM song; listen test at 60% (not 15% as said): handover "We're All Gonna Die" -> "Caves" CLEAN, no click.

## Parked for step 3

- "This video is private" wording would still give SignedOut (match "private" alone) — Ruling: real, rare, step 3.
- adds during a list load can push the queue one page past 1,000 until the next refill trims — Ruling: real, self-healing, step 3.
- a failed unpicked-list fetch sets Stopped while a user-added song plays — Ruling: pre-existing, step 3.
- docs/benchmarks.md "Side by side" table still shows step 1 numbers — Ruling: step 3 doc pass.
