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

| Command  | Args                                        | Reply data                 |
|----------|---------------------------------------------|----------------------------|
| `status` | none                                        | the state (below)          |
| `play`   | `videoId` (optional), `startSeconds` (optional, from 0) | `{}`           |
| `pause`  | none                                        | `{}`                       |
| `toggle` | none                                        | `{}`                       |
| `seek`   | `seconds` (a negative value seeks to the start) | `{}`                   |
| `volume` | `percent`, 0 to 100 (whole or decimal)      | `{}`                       |
| `quit`   | none                                        | `{}`, then the engine stops |

`play` without `videoId` resumes a paused song, or plays the last song again once it has
ended. With only `startSeconds` (above 0) it seeks the current song there, and resumes it if
it was paused; once the song has ended, it plays it again from there. A `videoId` is 11 characters of `A-Z`, `a-z`, `0-9`, `_` and `-`.

## Events

Events have no `id`. Every connected client gets every event.

The state, on every change (and as the `status` reply's data, without `"event"`):

```json
{"event": "state", "state": "playing", "videoId": "dQw4w9WgXcQ", "title": "...",
 "artist": "...", "lengthSeconds": 213, "thumbnail": "https://...", "position": 12.5,
 "volume": 80}
```

- `state` is `playing`, `paused`, `buffering` or `stopped`.
- `videoId` is the current song, or the last one after it ended; `null` before any.
- `title`, `artist`, `lengthSeconds` and `thumbnail` are `null` until the song's details are
  known (`lengthSeconds` is also `null` when the length is unknown).
- `position` is in seconds; `volume` is a percent.

The position, once a second while playing and after every seek:

```json
{"event": "position", "seconds": 42.5}
```

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

- A client that falls behind on events gets a fresh `state` event in place of the ones it
  missed.
- A client that stops reading altogether is disconnected once its outgoing queue fills, so
  it can never hold up the engine or other clients.
- A client that closes its sending side still gets the replies to what it sent; then the
  connection closes.

## Lifecycle

With the systemd units in `dist/systemd`, the first connection starts the engine. It quits by
itself after 5 minutes with nothing playing (2 minutes on battery); a command or a song
starting resets the clock. A client should not reconnect automatically after the engine
quits, because the connection would start it again.

Without systemd, `ytmfast daemon` binds the socket itself. It refuses to start while another
engine answers on the socket, and replaces a socket left behind by one that crashed.
