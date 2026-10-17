//! The engine's clear-queue and sleep-timer behaviour (`clear.rs`, `sleep.rs`).
//!
//! Clear is checked on an engine that is not run (`idle_engine` / `playing`, the way the
//! queue tests drive it); the timer needs the run loop's clock, so it runs on a real
//! engine with the null sink, a 2 s song and a shortened fade (`Setup::fade_ms`).

use super::*;

fn ids(engine: &Engine) -> String {
    engine
        .queue
        .items()
        .iter()
        .map(|i| i.song.video_id.chars().next().unwrap())
        .collect()
}

/// An engine whose queue holds `songs`, with `current` (a letter) the current item.
async fn queued(songs: &str, current: char) -> (Built, Server) {
    let (mut built, server) = idle_engine().await;
    let engine = &mut built.engine;
    add(engine, songs, AddAt::End);
    let id = queue_ids(engine)
        .into_iter()
        .find(|(v, _)| *v == vid(current))
        .unwrap()
        .1;
    engine.queue.jump(id).unwrap();
    (built, server)
}

// ---- Clear queue ----

#[tokio::test]
async fn clear_takes_out_what_comes_after_the_current_song_and_keeps_the_rest() {
    let (mut built, _server) = queued("ABCDE", 'C').await;
    let engine = &mut built.engine;
    let current = engine.queue.current().unwrap().id;
    let mut events = built.events.subscribe();
    assert_eq!(engine.queue_clear(), 2);
    // The songs already heard stay (Previous still reaches them), and so does the current one.
    assert_eq!(ids(engine), "ABC");
    assert_eq!(
        engine.queue.current().unwrap().id,
        current,
        "same song, same id"
    );
    // The widgets are told: one queue event, showing the emptied queue.
    let mut queue_events = 0;
    while let Ok(e) = events.try_recv() {
        if let EngineEvent::Queue { items, .. } = e {
            queue_events += 1;
            assert_eq!(items.len(), 3);
        }
    }
    assert_eq!(queue_events, 1);
}

#[tokio::test]
async fn clearing_an_empty_up_next_does_nothing_and_offers_no_undo() {
    let (mut built, _server) = queued("ABC", 'C').await;
    let engine = &mut built.engine;
    assert_eq!(engine.queue_clear(), 0);
    assert_eq!(ids(engine), "ABC");
    assert!(
        engine.cleared.is_none(),
        "nothing was taken, nothing to undo"
    );
    assert_eq!(engine.queue_undo(), 0);
}

#[tokio::test]
async fn clear_with_no_song_picked_yet_empties_the_queue() {
    // Songs added to an empty queue wait for a play: all of them are still to come.
    let (mut built, _server) = idle_engine().await;
    let engine = &mut built.engine;
    add(engine, "ABC", AddAt::End);
    assert_eq!(engine.queue_clear(), 3);
    assert!(engine.queue.is_empty());
    assert_eq!(engine.queue_undo(), 3);
    assert_eq!(ids(engine), "ABC");
}

#[tokio::test]
async fn undo_puts_the_songs_back_after_the_current_one_with_their_ids() {
    let (mut built, _server) = queued("ABCDE", 'B').await;
    let engine = &mut built.engine;
    let before = queue_ids(engine);
    assert_eq!(engine.queue_clear(), 3);
    assert_eq!(ids(engine), "AB");
    assert_eq!(engine.queue_undo(), 3);
    assert_eq!(queue_ids(engine), before, "the same songs, order and ids");
    assert_eq!(engine.queue.current().unwrap().song.video_id, vid('B'));
    // Once is enough.
    assert_eq!(engine.queue_undo(), 0);
}

#[tokio::test]
async fn undo_keeps_the_shuffle_order() {
    let (mut built, _server) = queued("ABCDEF", 'A').await;
    let engine = &mut built.engine;
    engine.queue.set_shuffle(true);
    let before = order(engine);
    assert_eq!(engine.queue_clear(), 5);
    assert_eq!(engine.queue_undo(), 5);
    assert_eq!(order(engine), before, "the shuffled order comes back");
    // And turning shuffle off still finds every song in the original order.
    engine.queue.set_shuffle(false);
    assert_eq!(ids(engine), "ABCDEF");
}

#[tokio::test]
async fn undo_is_refused_after_the_few_seconds_are_up() {
    let (mut built, _server) = queued("ABCD", 'A').await;
    let engine = &mut built.engine;
    assert_eq!(engine.queue_clear(), 3);
    engine.cleared.as_mut().unwrap().at -= clear::UNDO_FOR + Duration::from_secs(1);
    assert_eq!(engine.queue_undo(), 0);
    assert_eq!(ids(engine), "A");
    assert!(engine.cleared.is_none(), "an expired undo is let go");
}

#[tokio::test]
async fn any_other_change_to_the_queue_ends_the_undo() {
    for change in ["add", "remove", "move", "shuffle", "jump"] {
        let (mut built, _server) = queued("ABCD", 'A').await;
        let engine = &mut built.engine;
        assert_eq!(engine.queue_clear(), 3);
        match change {
            "add" => add(engine, "Z", AddAt::End),
            "remove" => {
                let id = engine.queue.current().unwrap().id;
                engine.handle(EngineCmd::QueueRemove(id + 100));
                // A removal of a song that is not there changes nothing: undo still works.
                assert_eq!(engine.queue_undo(), 3, "{change}");
                continue;
            }
            "move" => {
                let id = engine.queue.current().unwrap().id;
                engine.handle(EngineCmd::QueueMove { id, index: 0 });
                // Moving the only song to where it is changes nothing the user can see, but
                // it is still a queue change: the safe rule is no undo.
            }
            "shuffle" => engine.handle(EngineCmd::Shuffle(true)),
            "jump" => {
                let id = engine.queue.current().unwrap().id;
                engine.handle(EngineCmd::QueueJump(id));
            }
            _ => unreachable!(),
        }
        assert_eq!(engine.queue_undo(), 0, "{change}");
    }
}

#[tokio::test]
async fn a_cleared_queue_is_not_refilled_with_radio_at_once() {
    let (mut built, _server) = queued("ABCD", 'A').await;
    let engine = &mut built.engine;
    // Control: before the clear, three songs left and the radio waits...
    assert!(!engine.queue.needs_more());
    assert_eq!(engine.queue_clear(), 3);
    assert!(
        engine.queue.needs_more(),
        "an emptied queue is 'about to run out'"
    );
    engine.maybe_refill();
    assert!(
        engine.refilling.is_none(),
        "a clear must stay cleared, not fill again with autoplay songs"
    );
}

#[tokio::test]
async fn the_radio_is_asked_for_when_the_song_is_nearly_over() {
    let (built, _fake, _server) = playing(vec![long('A'), long('B')]).await;
    let mut engine = built.engine;
    assert_eq!(engine.queue_clear(), 1);
    // 100 s song: 11 s left, still held; 10 s left, the autoplay radio is asked for so the
    // next song starts without a gap.
    engine.maybe_preload(89.0);
    assert!(engine.refilling.is_none());
    engine.maybe_preload(90.0);
    assert!(
        engine.refilling.is_some(),
        "released at 10 s before the end"
    );
}

#[tokio::test]
async fn adding_a_song_or_skipping_ends_the_hold() {
    let (mut built, _server) = queued("ABCD", 'A').await;
    let engine = &mut built.engine;
    assert_eq!(engine.queue_clear(), 3);
    assert!(engine.refill_hold);
    add(engine, "Z", AddAt::End);
    assert!(
        !engine.refill_hold,
        "the user queued something: normal again"
    );

    let (mut built, _server) = queued("ABCD", 'A').await;
    let engine = &mut built.engine;
    assert_eq!(engine.queue_clear(), 3);
    engine.advance(false, false);
    assert!(
        !engine.refill_hold,
        "a skip with nothing next asks for the radio"
    );
}

#[tokio::test]
async fn undo_gives_back_the_lists_next_page_it_took() {
    let (mut built, _server) = queued("ABCD", 'A').await;
    let engine = &mut built.engine;
    engine.continuation = Some("faketoken".into());
    engine.exhausted = false;
    assert_eq!(engine.queue_clear(), 3);
    // The rest of the list is gone with the songs: nothing left to fetch pages of.
    assert_eq!(engine.continuation, None);
    assert_eq!(engine.queue_undo(), 3);
    assert_eq!(engine.continuation.as_deref(), Some("faketoken"));
    assert!(!engine.refill_hold);
}

#[tokio::test]
async fn clear_through_the_command_channel_answers_how_many() {
    let mut r = rig(Setup {
        pages: vec![ok("PLlist", 0, "ABCDE", None)],
        ..Setup::default()
    })
    .await;
    r.play_list("PLlist", Some(1)).await;
    r.until_song(&vid('B'), PlayState::Playing).await;
    let (tx, rx) = oneshot::channel();
    r.send(EngineCmd::QueueClear { reply: tx }).await;
    assert_eq!(rx.await.unwrap(), 3);
    let q = r.queue().await;
    assert_eq!(q.items.len(), 2);
    // The song keeps playing.
    assert_eq!(r.status().await.state, PlayState::Playing);
    assert_eq!(r.status().await.video_id, Some(vid('B')));
    let (tx, rx) = oneshot::channel();
    r.send(EngineCmd::QueueUndo { reply: tx }).await;
    assert_eq!(rx.await.unwrap(), 3);
    assert_eq!(r.queue().await.items.len(), 5);
}

// ---- Sleep timer ----

#[test]
fn the_fade_goes_from_full_to_silent_and_never_back_up() {
    let len = Duration::from_secs(10);
    assert_eq!(sleep::fade_gain(Duration::ZERO, len), 1.0);
    assert_eq!(sleep::fade_gain(len, len), 0.0);
    assert_eq!(
        sleep::fade_gain(len * 2, len),
        0.0,
        "past the end stays silent"
    );
    let mut last = 1.0;
    for ms in (0..=10_000).step_by(250) {
        let g = sleep::fade_gain(Duration::from_millis(ms), len);
        assert!((0.0..=1.0).contains(&g));
        assert!(g <= last, "{ms} ms: {g} after {last}");
        last = g;
    }
    // Quieter than a straight line in the middle: the ear hears loudness, not amplitude.
    assert!(sleep::fade_gain(Duration::from_secs(5), len) < 0.5);
    // A fade of no length is silent at once, not a division by zero.
    assert_eq!(sleep::fade_gain(Duration::ZERO, Duration::ZERO), 0.0);
}

#[tokio::test]
async fn no_timer_means_nothing_to_wake_for() {
    let (built, _server) = idle_engine().await;
    assert_eq!(built.engine.sleep_wake(), None);
    assert_eq!(built.engine.sleep_view(), None);
}

#[tokio::test]
async fn the_state_shows_a_timer_and_its_time_left() {
    let r = rig(Setup::default()).await;
    r.send(EngineCmd::Sleep(SleepSet::In(Duration::from_secs(90))))
        .await;
    let view = r.status().await.sleep.expect("a timer is set");
    assert_eq!(view.kind, SleepKind::Timer);
    assert!(matches!(view.left, Some(89 | 90)), "{view:?}");
    assert!(!view.fading);
    // "End of this song" has no time to show.
    r.send(EngineCmd::Sleep(SleepSet::SongEnd)).await;
    let view = r.status().await.sleep.unwrap();
    assert_eq!(
        (view.kind, view.left, view.fading),
        (SleepKind::SongEnd, None, false)
    );
    // Setting again replaced the first; Off clears it.
    r.send(EngineCmd::Sleep(SleepSet::Off)).await;
    assert_eq!(r.status().await.sleep, None);
}

#[tokio::test]
async fn a_set_timer_is_told_to_the_clients_and_not_saved_as_a_pause() {
    let mut r = rig(Setup::default()).await;
    r.send(EngineCmd::Sleep(SleepSet::In(Duration::from_secs(60))))
        .await;
    let EngineEvent::State(s) = r.next().await else {
        panic!("wanted a state");
    };
    assert_eq!(s.sleep.map(|v| v.kind), Some(SleepKind::Timer));
    r.send(EngineCmd::Sleep(SleepSet::Off)).await;
    let EngineEvent::State(s) = r.next().await else {
        panic!("wanted a state");
    };
    assert_eq!(s.sleep, None);
}

/// Volumes the output was set to, sampled until `done`, or 3 s.
async fn sample_volumes(r: &Rig, done: impl Fn() -> bool) -> Vec<f32> {
    let mut seen: Vec<f32> = Vec::new();
    let t = std::time::Instant::now();
    while !done() && t.elapsed() < Duration::from_secs(3) {
        let v = r.stats.volume();
        if seen.last() != Some(&v) {
            seen.push(v);
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    seen
}

#[tokio::test]
async fn a_timer_fades_the_music_down_then_pauses_and_puts_the_volume_back() {
    let mut r = rig(Setup {
        fade_ms: Some(600),
        ..Setup::default()
    })
    .await;
    r.play(&vid('A')).await;
    r.until(PlayState::Playing).await;
    r.send(EngineCmd::Sleep(SleepSet::In(Duration::from_millis(1000))))
        .await;
    let seen = sample_volumes(&r, || r.stats.paused()).await;
    assert!(
        r.stats.paused(),
        "it paused at the end of the timer: {seen:?}"
    );
    // Down in steps, never up, from the full volume towards silence.
    assert!(seen.len() >= 4, "a fade, not a jump: {seen:?}");
    assert_eq!(seen[0], 1.0);
    assert!(seen.windows(2).all(|w| w[1] <= w[0]), "{seen:?}");
    assert!(seen.last().copied().unwrap() < 0.2, "{seen:?}");
    // The paused song is back at the volume the user had, so the next play is not silent.
    eventually("the volume is back", || r.stats.volume() == 1.0).await;
    let s = r.status().await;
    assert_eq!(s.state, PlayState::Paused);
    assert_eq!(s.volume, 1.0, "the user's volume was never touched");
    assert_eq!(s.sleep, None, "the timer is spent");
}

#[tokio::test]
async fn the_state_says_when_the_fade_has_begun() {
    let mut r = rig(Setup {
        fade_ms: Some(800),
        ..Setup::default()
    })
    .await;
    r.play(&vid('A')).await;
    r.until(PlayState::Playing).await;
    r.send(EngineCmd::Sleep(SleepSet::In(Duration::from_millis(1200))))
        .await;
    eventually("the fade begins", || r.stats.volume() < 1.0).await;
    let view = r.status().await.sleep.expect("still set while fading");
    assert!(view.fading, "{view:?}");
}

#[tokio::test]
async fn a_timer_shorter_than_the_fade_fades_over_what_there_is() {
    let mut r = rig(Setup {
        fade_ms: Some(5000),
        ..Setup::default()
    })
    .await;
    r.play(&vid('A')).await;
    r.until(PlayState::Playing).await;
    r.send(EngineCmd::Sleep(SleepSet::In(Duration::from_millis(500))))
        .await;
    let seen = sample_volumes(&r, || r.stats.paused()).await;
    assert!(r.stats.paused(), "{seen:?}");
    assert!(seen.len() >= 3, "{seen:?}");
}

#[tokio::test]
async fn turning_the_timer_off_mid_fade_brings_the_volume_back_and_plays_on() {
    let mut r = rig(Setup {
        fade_ms: Some(1500),
        ..Setup::default()
    })
    .await;
    r.play(&vid('A')).await;
    r.until(PlayState::Playing).await;
    r.send(EngineCmd::Sleep(SleepSet::In(Duration::from_millis(1600))))
        .await;
    eventually("the fade begins", || r.stats.volume() < 1.0).await;
    r.send(EngineCmd::Sleep(SleepSet::Off)).await;
    eventually("the volume is back", || r.stats.volume() == 1.0).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!r.stats.paused(), "it plays on");
    assert_eq!(r.stats.volume(), 1.0, "and stays up");
    assert_eq!(r.status().await.sleep, None);
}

#[tokio::test]
async fn setting_the_volume_mid_fade_cancels_the_timer() {
    let mut r = rig(Setup {
        fade_ms: Some(1500),
        ..Setup::default()
    })
    .await;
    r.play(&vid('A')).await;
    r.until(PlayState::Playing).await;
    r.send(EngineCmd::Sleep(SleepSet::In(Duration::from_millis(1600))))
        .await;
    eventually("the fade begins", || r.stats.volume() < 1.0).await;
    r.send(EngineCmd::Volume(0.6)).await;
    eventually("the user's volume", || r.stats.volume() == 0.6).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(r.stats.volume(), 0.6, "no fade step lands after it");
    assert_eq!(r.status().await.sleep, None);
    assert!(!r.stats.paused());
}

#[tokio::test]
async fn a_mixer_turning_the_stream_mid_fade_cancels_the_timer_too() {
    let mut r = rig(Setup {
        fade_ms: Some(1500),
        ..Setup::default()
    })
    .await;
    r.play(&vid('A')).await;
    r.until(PlayState::Playing).await;
    r.send(EngineCmd::Sleep(SleepSet::In(Duration::from_millis(1600))))
        .await;
    eventually("the fade begins", || r.stats.volume() < 1.0).await;
    r.stats.mixer_volume(0.3);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(r.stats.volume(), 0.3, "the mixer's volume stands");
    assert_eq!(r.status().await.sleep, None);
    assert_eq!(r.status().await.volume, 0.3);
}

#[tokio::test]
async fn pausing_by_hand_mid_fade_ends_the_timer_and_restores_the_volume() {
    let mut r = rig(Setup {
        fade_ms: Some(1500),
        ..Setup::default()
    })
    .await;
    r.play(&vid('A')).await;
    r.until(PlayState::Playing).await;
    r.send(EngineCmd::Sleep(SleepSet::In(Duration::from_millis(1600))))
        .await;
    eventually("the fade begins", || r.stats.volume() < 1.0).await;
    r.send(EngineCmd::Pause).await;
    eventually("paused", || r.stats.paused()).await;
    eventually("the volume is back", || r.stats.volume() == 1.0).await;
    assert_eq!(r.status().await.sleep, None);
}

#[tokio::test]
async fn a_timer_that_runs_out_while_paused_just_ends() {
    let mut r = rig(Setup {
        fade_ms: Some(200),
        ..Setup::default()
    })
    .await;
    r.play(&vid('A')).await;
    r.until(PlayState::Playing).await;
    r.send(EngineCmd::Pause).await;
    r.until(PlayState::Paused).await;
    let sets = r.stats.volume_sets();
    r.send(EngineCmd::Sleep(SleepSet::In(Duration::from_millis(300))))
        .await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(r.status().await.sleep, None, "spent");
    assert_eq!(r.stats.volume_sets(), sets, "a paused song is never faded");
    assert_eq!(r.status().await.state, PlayState::Paused);
}

#[tokio::test]
async fn end_of_song_fades_over_the_last_part_and_pauses_at_the_next_song() {
    let mut r = rig(Setup {
        pages: vec![ok("PLlist", 0, "AB", None)],
        fade_ms: Some(1000),
        ..Setup::default()
    })
    .await;
    r.play_list("PLlist", None).await;
    r.until_song(&vid('A'), PlayState::Playing).await;
    r.send(EngineCmd::Sleep(SleepSet::SongEnd)).await;
    // The song is 2 s: the fade starts a second in and the volume goes down.
    let seen = sample_volumes(&r, || r.stats.paused()).await;
    assert!(r.stats.paused(), "paused at the song's end: {seen:?}");
    assert!(seen.len() >= 4, "{seen:?}");
    assert!(seen.windows(2).all(|w| w[1] <= w[0]), "{seen:?}");
    // The next song is the current one, waiting, and the volume is back for when it plays.
    let s = r.status().await;
    assert_eq!(s.state, PlayState::Paused);
    assert_eq!(s.video_id, Some(vid('B')));
    assert_eq!(s.sleep, None);
    eventually("the volume is back", || r.stats.volume() == 1.0).await;
}

#[tokio::test]
async fn end_of_song_follows_a_skip_to_the_new_song() {
    let mut r = rig(Setup {
        pages: vec![ok("PLlist", 0, "ABC", None)],
        fade_ms: Some(500),
        ..Setup::default()
    })
    .await;
    r.play_list("PLlist", None).await;
    r.until_song(&vid('A'), PlayState::Playing).await;
    r.send(EngineCmd::Sleep(SleepSet::SongEnd)).await;
    r.send(EngineCmd::Next).await;
    r.until_song(&vid('B'), PlayState::Playing).await;
    // Still set: it is now about B.
    assert_eq!(
        r.status().await.sleep.map(|v| v.kind),
        Some(SleepKind::SongEnd)
    );
    // And B's end pauses on C.
    r.until_song(&vid('C'), PlayState::Paused).await;
    assert_eq!(r.status().await.sleep, None);
}

#[tokio::test]
async fn the_timer_does_not_survive_a_stopped_engine_quietly() {
    // With nothing playing a timer has nothing to fade: it ends at its time, silently.
    let r = rig(Setup::default()).await;
    r.send(EngineCmd::Sleep(SleepSet::In(Duration::from_millis(200))))
        .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(r.status().await.sleep, None);
    assert_eq!(r.status().await.state, PlayState::Stopped);
    assert_eq!(r.stats.volume_sets(), 0);
}
