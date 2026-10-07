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
| `play`         | `videoId`, `playlistId`, `index`, `startSeconds` (all optional); or `endpoint` alone | `{}` |
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
| `like`         | `status`: `"like"`, `"dislike"` or `"none"`; `videoId` (optional) | `{}`, once YouTube took it |
| `mute`         | `on`: `true` or `false`                                 | `{}`                        |
| `watch`        | `queue`: `true` or `false`                              | `{}`                        |
| `quit`         | none                                                    | `{}`, then the engine stops |
| `browse`       | `browseId`, `params` (optional)                         | a page (below)              |
| `search`       | `query`, `params` (optional)                            | a search page (below)       |
| `more`         | `kind`: `"browse"` or `"search"`, `token`               | a next page (below)         |
| `playPage`     | `browseId`, `params` (optional)                         | `{}`, or `{"superseded": true}` |
| `lyrics`       | `videoId`                                               | `{text, source}` or `{"none": true}` (below) |

A `videoId` is 11 characters of `A-Z`, `a-z`, `0-9`, `_` and `-`. A `playlistId` is 1 to
256 of the same characters. A `queueId` and an `index` are whole numbers from 0 up.

### play

- With `playlistId` (an album or a playlist, such as `LM` for Liked songs): its songs
  become the queue. It starts at `videoId` when given (that song plays at once, before the
  list arrives), else at `index` (from 0; past the end starts at the first song). With
  neither and shuffle on, a random song of the list starts; with shuffle off, the first.
  `index` is only taken with a `playlistId`.
- With `videoId` alone: that song plays at once, and its radio fills the queue behind it.
- With neither: it resumes a paused song, or plays the last song again once it has ended.
  With only `startSeconds` (above 0) it seeks the current song there, and resumes it if it
  was paused; once the song has ended, it plays it again from there. With songs queued but
  none playing yet (songs added to an empty queue), it plays the first of them. With an
  empty queue and nothing saved, it plays Liked songs.
- `startSeconds` (from 0) is where the first song starts.

With `endpoint`, it plays a row's `play` (or a page header's), sent back exactly as a
`browse`, `search` or `more` answer gave it; nothing else may come with it. See "Playing a
row" below.

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
   "albumId": "MPREb_...", "thumbnail": "https://...", "lengthSeconds": 213}]}}
{"id": 4, "cmd": "queue.add", "args": {"videoIds": ["dQw4w9WgXcQ"]}}
```

- In `songs`, only `videoId` is needed; the rest may be left out or `null`. `title`,
  `album` and each artist are at most 4 KiB, with at most 20 `artists`; longer is a
  `bad_request`. A `thumbnail` that isn't an https link on YouTube's or Google's image hosts
  is dropped (the song is still added). `lengthSeconds` is a whole number from 0 up.
  `albumId` is the song's album `browseId` (for the cover click), checked as a `browseId`
  is (see Browsing); a malformed one is taken as `""` (no album), and the song is still
  added.
- With `videoIds`, the songs join without details; the engine fills them in when they play.
- `at` is `"next"` (right after the current song) or `"end"` (the default). While shuffle
  is on, `"end"` songs are shuffled into the songs still to come (they go at the end of the
  original order, which shuffle off brings back).
- The queue holds at most 1,000 songs. An add that would take it past that is refused whole
  with `bad_request` and the message `the queue is full`; nothing is added. Remove songs
  first, or start a new queue with `play`.

`queue.remove` takes a song out (if it was playing, the next one takes its place).
`queue.jump` plays a song. `queue.move` moves a song to `index` in the play order (the
shuffled order while shuffle is on); an `index` past the end moves it to the end. For a
`queueId` that isn't in the queue, the reply is still `"ok": true`, and an `unavailable`
error event says nothing was done.

`shuffle` with `on: true` mixes the queue, with the current song moved first; with `false`
it goes back to the original order, at the same song. Songs that join later while it is on
(a list's next page, radio songs) are shuffled into the songs still to come, never before
the current one. `repeat` is `"off"`, `"all"` (the whole queue again after the last
song) or `"one"` (the current song again when it ends; `next` and `previous` still move).

### like and mute

`like` sets a song's like status on the account: `"like"`, `"dislike"`, or `"none"` to take
either back. With `videoId` it is that song (a row in a list, whatever is playing); without,
or with `"videoId": null`, it is the song the state shows. With no `videoId` and no song
shown, it is `bad_request` (`bad request: nothing is playing: say which song (videoId)`).

Unlike the playback commands, the reply waits for YouTube: `{}` means YouTube took it, and
by then the state's `liked` already shows it when it is the song shown. With two likes for
one song on their way at once, the last one sent wins: the state shows only its answer, and
an older one's `{}` (whenever it comes) changes nothing there. A like runs
alongside the client's other requests, like a browse, and counts toward the same limit of 4
waiting at once (see Browsing). A refused like is answered with the error, to that client
only, never as an `error` event, with the codes of browsing's errors (see Browsing);
`signed_out` also covers YouTube refusing the account action.

`mute` with `on: true` silences the stream and keeps the volume; `on: false` puts that volume
back. Muting while muted does nothing. Setting the volume while muted unmutes, at the new
volume: `volume` from a client, MPRIS's `Volume`, or a mixer or desktop volume popup turning
the stream up (the user touched the volume, so they want to hear it). Mute is kept across a
restart, as the volume is.

## Browsing

`browse`, `search`, `more`, `playPage` and `lyrics` ask YouTube Music for pages, the ones
the bar widget lists. They answer only the client that asked (nothing is broadcast), and they never
start playback or change the queue, except `playPage`, whose job is to play.

Each one runs on its own: a client keeps getting replies and events while a page loads, and
replies may come in a different order from the requests (match them by `id`). A client may
have at most 4 browsing requests waiting at once; another one is answered at once with
`bad_request` and the message `busy`. A browsing request counts as activity for the idle
clock, like any command.

Ids and tokens are checked before anything is sent:

- `browseId` is 2 to 128 characters of `A-Z`, `a-z`, `0-9`, `_` and `-`.
- `params` and `token` are up to 4096 characters of those, plus `+`, `/`, `=` and `%`.
  An empty `params` (`""`, as rows carry it) is the same as none.
- `query` is trimmed, then must be 1 to 200 characters with no control characters, no line or paragraph
  separators and no invisible format characters (bidi controls, zero-width spaces and the like). Kept: the
  joiners ZWNJ and ZWJ (Persian, Indic scripts and emoji need them), the tag characters of subdivision flags, and
  the number signs that show in Arabic, Syriac and Kaithi text.

A bad one is `bad_request`, and nothing is asked of YouTube (or of the keyring).

Every text field is a string, `""` when there is nothing (never `null`); links are https
links on YouTube's or Google's image hosts, or `""`.

### browse

`{"browseId": "FEmusic_home"}` gives a page: Home (`FEmusic_home`), the library
(`FEmusic_library_landing`, `FEmusic_liked_playlists`, ...), a playlist (`VL...`), an album,
an artist or a podcast. A section's `more` link is opened the same way, with its `params`.

```json
{"id": 4, "ok": true, "data": {
  "header": {"title": "An Album", "subtitle": "Album • An Artist • 2024",
             "thumb": "https://lh3.googleusercontent.com/...=w226-h226",
             "play": {"watchPlaylistEndpoint": {"playlistId": "OLAK5uy_..."}}},
  "sections": [
    {"title": "", "items": [
      {"title": "A Song", "subtitle": "An Artist", "thumb": "https://...",
       "videoId": "dQw4w9WgXcQ", "setId": "", "playlistId": "", "browseId": "",
       "params": "", "play": {"watchEndpoint": {"videoId": "dQw4w9WgXcQ",
       "playlistId": "OLAK5uy_..."}}, "duration": "3:33", "kind": "song"}],
     "cont": "", "more": null}],
  "cont": ""}}
```

- `header.play` is the page's big play button (`null` when it has none).
- A section's `cont` is the token for its next rows (`""` at the end), for `more` with
  `kind: "browse"`. Its `more` is `null` or `{"browseId", "params"}`: a "Show all" page.
- The page's own `cont` is the token for more sections (Home, as it scrolls).
- A row's `kind` is `song`, `album`, `artist`, `playlist`, `podcast`, `page` or `""`.
  `videoId`, `browseId` and `playlistId` say what it opens; `setId` is its place in a
  playlist (a song can be there twice). `play` is `null` or one endpoint:
  `{"watchEndpoint": {"videoId"?, "playlistId"?, "index"?, "params"?}}` or
  `{"watchPlaylistEndpoint": {"playlistId", "params"?}}`, holding only those fields.
- A section holds at most 300 rows.

### search

`{"query": "some song"}` gives the mixed results; with a chip's `params` it gives that
filter (Songs, Albums, ...):

```json
{"id": 5, "ok": true, "data": {
  "sections": [{"title": "Top result", "items": [...], "cont": "", "more": null}],
  "chips": [{"label": "Songs", "params": "EgWKAQIIAWoKEAkQBRAKEAMQBA%3D%3D"}]}}
```

Rows are as in `browse`. Mixed results keep 30 rows a section and have no next page; a
filtered search keeps up to 300 and pages with its section's `cont` (`more` with
`kind: "search"`).

### more

`{"kind": "browse", "token": "..."}` gives the next page of a list, with a `cont` from a
`browse` answer (`kind: "browse"`) or a filtered `search` (`kind: "search"`):

```json
{"id": 6, "ok": true, "data": {"items": [...], "sections": [], "cont": "..."}}
```

A list's next rows come in `items` (at most 1,000); Home's next shelves come in `sections`.
`cont` is the token for the page after, `""` at the end.

### Playing a row

`play` with `endpoint` plays a row's `play` as it came:

```json
{"id": 7, "cmd": "play", "args": {"endpoint":
  {"watchEndpoint": {"videoId": "dQw4w9WgXcQ", "playlistId": "PL..."}}}}
```

- A `watchEndpoint` with a `playlistId` plays that list, starting at its `videoId` (that
  song plays at once), else at its `index`, with its `params`.
- A `watchEndpoint` with only a `videoId` plays the song and its radio, as `play` with a
  `videoId` does.
- A `watchPlaylistEndpoint` plays the list from the start, using its `params` (an artist's
  shuffle, for example).

Other fields in the endpoint are ignored. In an endpoint, `videoId` is as above,
`playlistId` is 2 to 128 characters of `A-Z`, `a-z`, `0-9`, `_` and `-` (the rule rows are
made with, not the plain `play`'s 1 to 256), `index` is a whole number from 0 to 4294967295,
and `params` is as in browsing. A malformed one is `bad_request`, as is an endpoint that
plays nothing, and one holding both a `watchEndpoint` and a `watchPlaylistEndpoint` (rows
carry one or the other, so which was meant is not guessed). The reply is `{}`; what comes
of the play arrives as events, as with any `play`.

### playPage

`{"browseId": "UC..."}` loads the page and plays its header's button (an artist's
shuffle, an album's play), or else its first playable row (of the first 5 rows of each
section). For a tile that has no play of its own, such as an artist. The reply is `{}` once
the play went to the engine. A page with nothing to play is `bad_request` with the message
`Nothing here can be played.`

A newer choice wins: when a command that picks what plays reaches the engine after this
`playPage` and before its page has loaded, the page's play is dropped, and the reply is
`{"id": ..., "ok": true, "data": {"superseded": true}}`. The commands that pick what plays,
from any client or MPRIS, are `play`, `playPage`'s own play, `queue.jump`, `next`,
`previous`, a `seek` at or past the song's end (it plays the next song), and a
`queue.remove` of the song playing (the next one takes its place). A `play` with no id
and a `toggle` (and MPRIS Play and PlayPause) count only when the state is `stopped`: then
they start something. Otherwise they only resume or pause the song, and the page still
plays.

### lyrics

`{"videoId": "dQw4w9WgXcQ"}` gives the song's lyrics as YouTube Music shows them: plain text,
no timings.

```json
{"id": 8, "ok": true, "data": {"text": "First line\nSecond line\n\nChorus",
                                 "source": "Source: Musixmatch"}}
```

- `text` is as YouTube gives it, newlines kept. It can run to a few KB; past 256 KiB it is
  cut there, at a character's end (no real lyrics come close).
- `source` is the line YouTube shows under them (`"Source: ..."`), or `""`.
- A song with no lyrics is `{"id": 8, "ok": true, "data": {"none": true}}`.

Lyrics take two requests to YouTube: the song's `next` (whose Lyrics tab names the lyrics
page), then that page. The engine reads the same `next` for the song playing (its queue's,
or its like lookup's) and keeps the tab with the like status, for its last 100 songs. So
lyrics for the song playing take one request, and a `next` made for lyrics gives the engine
the song's like status in turn.

The daemon keeps the last 20 answers, for every client: asking again for one of those songs
(reopening the Lyrics tab, or another widget asking) is answered at once, with nothing sent.
A song with no lyrics is asked about again after an hour (YouTube adds lyrics to songs
later); found lyrics are kept while the daemon runs. A failure is never kept.

### Errors

A failed browsing request is answered with the error, to that client only, never as an
`error` event:

```json
{"id": 4, "ok": false, "error": {"code": "network", "message": "network error: timed out"}}
```

The code is `bad_request` (a malformed request, or `busy`), `signed_out` (no session, or
YouTube refused it), `network`, `unavailable` or `internal`. Messages are fixed text: they
never hold what was sent, a link or a token.

## Events

Events have no `id`. Every connected client gets every event, except `queue` events for a
client that turned them off (see `watch` below).

The state, on every change (and as the `status` reply's data, without `"event"`):

```json
{"event": "state", "state": "playing", "videoId": "dQw4w9WgXcQ", "title": "...",
 "artist": "...", "lengthSeconds": 213, "thumbnail": "https://...", "position": 12.5,
 "volume": 80, "muted": false, "album": "...", "albumId": "MPREb_...", "queueId": 7,
 "shuffle": false, "repeat": "off", "liked": "like"}
```

- `state` is `playing`, `paused`, `buffering` or `stopped`.
- `videoId` is the current song, or the last one after it ended; `null` before any.
- `title`, `artist`, `lengthSeconds` and `thumbnail` are `null` until the song's details are
  known (`lengthSeconds` is also `null` when the length is unknown). A song queued with its
  details shows them at once. `artist` names every artist, joined with `", "`.
- `album` is the current song's album, from its queue item; `null` when it has none.
- `albumId` is that album's `browseId` (`MPREb_...`), from the same queue item, for opening
  the album with `browse` (the cover, clicked). It is a string, `""` when there is none: a
  song with no album link (a user upload), one added with `queue.add` without an `albumId`,
  or nothing current. It is checked as a `browseId` is (see Browsing); a malformed one
  is `""`.
- `queueId` is the current song's id in the queue; `null` when there is none.
- `position` is in seconds; `volume` is a percent. `shuffle` and `repeat` are as in the
  queue.
- `volume` follows the stream's volume wherever it is changed: a change in a mixer or a
  desktop volume popup sends a new `state` with it, and the engine keeps it (a later song,
  or a restart, plays at it).
- `muted` is `true` while the stream is silenced by `mute`. `volume` then still shows the
  volume unmuting goes back to, so a slider keeps its place.
- `liked` is the shown song's like status on the account: `"like"`, `"dislike"` or `"none"`;
  `null` until known. It is known from the song's queue answer when the queue was asked for
  with that song (a song played by id), else from one small request when the song starts,
  and at once after a `like` of it. A song whose status could not be read stays `null` until
  it starts again.

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
   "album": "...", "albumId": "MPREb_...", "thumbnail": "https://...", "lengthSeconds": 213},
  {"queueId": 8, "videoId": "...", "title": "...", "artists": ["..."], "album": null,
   "albumId": "", "thumbnail": "https://...", "lengthSeconds": 187, "radio": true}],
 "currentId": 7, "shuffle": false, "repeat": "off"}
```

- `items` are in play order: the shuffled order while shuffle is on.
- A song added by id alone has `title`, `album`, `thumbnail` and `lengthSeconds` `null`
  and `artists` empty, until it plays.
- `albumId` is as in the state: the song's album `browseId`, or `""` (never `null`).
- `radio: true` marks a song from the radio the engine starts by itself once the queue's own
  songs run out (the radio of the last song, and that radio's next pages): a widget's
  "Autoplay" divider goes before the first one. Other songs have no `radio` key, including
  the songs of a radio the user started (a song played by `videoId` alone, or a radio
  playlist): that radio is the queue itself. The mark is kept across a restart.
- `currentId` is the current song's `queueId`; `null` when there is none (songs added to
  an empty queue wait for `queue.jump`, `next` or `play`).
- The whole queue comes every time. The queue holds at most 1,000 songs, so with
  real-sized details the line is at most about 405 KB.

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

### Choosing events: watch

`watch` with `{"queue": false}` stops `queue` events to this client; every other event still
comes, and other clients are not affected. `{"queue": true}` turns them back on (they are on
when a client connects): the reply is followed at once by a `queue` event with the queue as
it is now, so nothing changed while they were off is missed. Turning them on while on sends
nothing extra. A bar that shows only the song can leave them off, and turn them on while its
panel shows the queue: a 1,000-song queue is about 400 KB of JSON on every change.
`queue.get` answers as always, whatever the setting.

## Connections

- A client that falls behind on events gets a fresh `state` event and a fresh `queue`
  event (without the `queue` event while it has them off, see `watch`) in place of the ones
  it missed.
- A client that stops reading altogether is disconnected once its outgoing queue fills
  (256 lines or 4 MiB, whichever comes first), so it can never hold up the engine or other
  clients, nor much memory.
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
- `Volume` is the socket's `volume`. MPRIS has no mute: while muted, `Volume` shows the kept
  volume, and setting it unmutes, as the socket's `volume` does.
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
