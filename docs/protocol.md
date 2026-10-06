# Control socket protocol

ytmfast takes commands over a Unix socket at `$XDG_RUNTIME_DIR/ytmfast/socket`. The folder is
0700 and the socket 0600, and the engine also checks that each client runs as the same user.

Messages are JSON, one per line (newline-terminated). A line may be at most 1 MiB; a longer
one gets a `bad_request` reply and the connection is closed.

## Requests and replies

A request:

```json
{"id": 7, "cmd": "seek", "args": {"seconds": 42.5}}
```

`id` is a whole number of the client's choosing and comes back in the reply. `args` is an
object, and may be left out when a command takes none.

A reply:

```json
{"id": 7, "ok": true, "data": {}}
{"id": 7, "ok": false, "error": {"code": "bad_request", "message": "seconds must be a number"}}
```

A request that isn't valid JSON, or has no usable `id`, gets a reply with `"id": null`. An
unknown command gets `bad_request` with the message `unknown command`. A bad request never
closes the connection.

`"ok": true` means the engine took the command. What comes of it (a song starting, or
failing) arrives as events.

## Commands

| Command        | Args                                                    | Reply data                  |
|----------------|---------------------------------------------------------|-----------------------------|
| `status`       | none                                                    | the state (below)           |
| `play`         | `videoId`, `playlistId`, `index`, `startSeconds` (all optional) | `{}`                |
| `pause`        | none                                                    | `{}`                        |
| `toggle`       | none                                                    | `{}`                        |
| `seek`         | `seconds` (a negative value seeks to the start)         | `{}`                        |
| `volume`       | `percent`, 0 to 100 (whole or decimal)                  | `{}`                        |
| `next`         | none                                                    | `{}`                        |
| `previous`     | none                                                    | `{}`                        |
| `queue.get`    | none                                                    | the queue (below)           |
| `queue.add`    | `songs` or `videoIds`, and `at` (optional)              | `{}`                        |
| `queue.remove` | `queueId`                                               | `{}`                        |
| `queue.jump`   | `queueId`                                               | `{}`                        |
| `queue.move`   | `queueId`, `index`                                      | `{}`                        |
| `shuffle`      | `on`: `true` or `false`                                 | `{}`                        |
| `repeat`       | `mode`: `"off"`, `"all"` or `"one"`                     | `{}`                        |
| `quit`         | none                                                    | `{}`, then the engine stops |

A `videoId` is 11 characters of `A-Z`, `a-z`, `0-9`, `_` and `-`. A `playlistId` is 1 to
256 of the same characters. A `queueId` and an `index` are whole numbers from 0 up.

### play

- With `playlistId` (an album or a playlist, such as `LM` for Liked songs): its songs
  become the queue. It starts at `videoId` when given (that song plays at once, before the
  list arrives), else at `index` (from 0; past the end starts at the first song). `index`
  is only taken with a `playlistId`.
- With `videoId` alone: that song plays at once, and its radio fills the queue behind it.
- With neither: it resumes a paused song, or plays the last song again once it has ended.
  With only `startSeconds` (above 0) it seeks the current song there, and resumes it if it
  was paused; once the song has ended, it plays it again from there. With nothing at all
  loaded or saved, it plays Liked songs.
- `startSeconds` (from 0) is where the first song starts.

When the queue runs out, the engine carries on with radio songs. When that would take the
queue past 1,000 songs, the engine first drops played songs from the front, keeping the
last 50 before the current one.

### next and previous

`next` plays the next song in the queue (with repeat `all`, the first one after the last).
`previous` starts the song again when it is more than 3 seconds in; else it plays the song
before (with repeat `all`, the last one from the first).

### The queue

Every song in the queue has a `queueId`, which the engine gives it when it joins the queue.
The ids start at 1 and are never reused while the engine runs, so an id names one queued
song even after the queue changes around it.

`queue.add` takes either `songs` or `videoIds`, not both, with 1 to 500 songs:

```json
{"id": 3, "cmd": "queue.add", "args": {"at": "next", "songs": [
  {"videoId": "dQw4w9WgXcQ", "title": "...", "artists": ["..."], "album": "...",
   "thumbnail": "https://...", "lengthSeconds": 213}]}}
{"id": 4, "cmd": "queue.add", "args": {"videoIds": ["dQw4w9WgXcQ"]}}
```

- In `songs`, only `videoId` is needed; the rest may be left out or `null`. `title`,
  `album` and each artist are at most 4 KiB, with at most 20 `artists`; longer is a
  `bad_request`. A `thumbnail` that isn't an https link on YouTube's or Google's image hosts
  is dropped (the song is still added). `lengthSeconds` is a whole number from 0 up.
- With `videoIds`, the songs join without details; the engine fills them in when they play.
- `at` is `"next"` (right after the current song) or `"end"` (the default).
- The queue holds at most 1,000 songs. An add that would take it past that is refused whole
  with `bad_request` and the message `the queue is full`; nothing is added. Remove songs
  first, or start a new queue with `play`.

`queue.remove` takes a song out (if it was playing, the next one takes its place).
`queue.jump` plays a song. `queue.move` moves a song to `index` in the play order (the
shuffled order while shuffle is on); an `index` past the end moves it to the end. For a
`queueId` that isn't in the queue, the reply is still `"ok": true`, and an `unavailable`
error event says nothing was done.

`shuffle` with `on: true` mixes the queue, with the current song moved first; with `false`
it goes back to the original order, at the same song. `repeat` is `"off"`, `"all"` (the whole queue again after the last
song) or `"one"` (the current song again when it ends; `next` and `previous` still move).

## Events

Events have no `id`. Every connected client gets every event.

The state, on every change (and as the `status` reply's data, without `"event"`):

```json
{"event": "state", "state": "playing", "videoId": "dQw4w9WgXcQ", "title": "...",
 "artist": "...", "lengthSeconds": 213, "thumbnail": "https://...", "position": 12.5,
 "volume": 80, "album": "...", "queueId": 7, "shuffle": false, "repeat": "off"}
```

- `state` is `playing`, `paused`, `buffering` or `stopped`.
- `videoId` is the current song, or the last one after it ended; `null` before any.
- `title`, `artist`, `lengthSeconds` and `thumbnail` are `null` until the song's details are
  known (`lengthSeconds` is also `null` when the length is unknown). A song queued with its
  details shows them at once. `artist` names every artist, joined with `", "`.
- `album` is the current song's album, from its queue item; `null` when it has none.
- `queueId` is the current song's id in the queue; `null` when there is none.
- `position` is in seconds; `volume` is a percent. `shuffle` and `repeat` are as in the
  queue.

The position, once a second while playing and after every seek:

```json
{"event": "position", "seconds": 42.5, "seeked": false}
```

`seeked` is `true` only for the event a seek sends (from any client, MPRIS too), so a
widget can tell a jump from the clock moving on.

The queue, on every change (songs added, removed or moved, a new current song, shuffle or
repeat), and as the `queue.get` reply's data, without `"event"`:

```json
{"event": "queue", "items": [
  {"queueId": 7, "videoId": "dQw4w9WgXcQ", "title": "...", "artists": ["..."],
   "album": "...", "thumbnail": "https://...", "lengthSeconds": 213}],
 "currentId": 7, "shuffle": false, "repeat": "off"}
```

- `items` are in play order: the shuffled order while shuffle is on.
- A song added by id alone has `title`, `album`, `thumbnail` and `lengthSeconds` `null`
  and `artists` empty, until it plays.
- `currentId` is the current song's `queueId`; `null` when there is none (songs added to
  an empty queue wait for `queue.jump`, `next` or `play`).
- The whole queue comes every time. The queue holds at most 1,000 songs, so with
  real-sized details the line is at most about 375 KB.

An error:

```json
{"event": "error", "code": "network", "message": "network error: timed out"}
```

Error codes: `signed_out`, `unavailable`, `network`, `stream_failed`, `internal`.

When the sound server restarts under a song, the engine sends `internal` with the message
`internal error: the audio output restarted` at once, then loads the same song again from
where it was: a playing song plays on (`stopped`, then `buffering`, then `playing`), a paused
one stays paused there (`stopped`, `buffering`, `paused`). It does this once per play; a
second restart in the same song leaves it stopped.

## Connections

- A client that falls behind on events gets a fresh `state` event and a fresh `queue`
  event in place of the ones it missed.
- A client that stops reading altogether is disconnected once its outgoing queue fills, so
  it can never hold up the engine or other clients.
- A client that closes its sending side still gets the replies to what it sent; then the
  connection closes.

## MPRIS

The engine also shows as a media player on the session bus, as
`org.mpris.MediaPlayer2.ytmfast`, for media keys, `playerctl` and desktop widgets. It drives
the same engine as the socket, so a change from either side shows on both.

- `Next` and `Previous` are the socket's `next` and `previous`. `CanGoNext` and
  `CanGoPrevious` follow the queue: true when there is a song after (or before) the current
  one, or with repeat `all`. With songs queued but none current, `CanGoNext` is true (Next
  starts the first one) and `CanGoPrevious` is false.
- `Shuffle` (true or false) is the socket's `shuffle`. `LoopStatus` is the socket's
  `repeat`: `"None"` is `off`, `"Track"` is `one` and `"Playlist"` is `all`.
- `Metadata` holds the title, the artist, the length, `xesam:album` and `mpris:artUrl`
  (from the song's queue item when it has them).
- `Seeked` comes once for every seek, from any client, with where the song landed.
- `SetPosition` at or past the song's end is ignored, as the spec says.
- There is no track list (`HasTrackList` is false): the socket's `queue.get` has the queue.

## Lifecycle

With the systemd units in `dist/systemd`, the first connection starts the engine. It quits by
itself after 5 minutes with nothing playing (2 minutes on battery); a command or a song
starting resets the clock. A client should not reconnect automatically after the engine
quits, because the connection would start it again.

Without systemd, `ytmfast daemon` binds the socket itself. It refuses to start while another
engine answers on the socket, and replaces a socket left behind by one that crashed.
