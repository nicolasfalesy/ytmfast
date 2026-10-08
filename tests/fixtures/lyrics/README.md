# Lyrics golden cases

Every lyric in here is made up. Real lyrics are copyrighted and are never committed; the live
comparison (10 real songs) reads answers recorded on the user's machine instead
(`YTMFAST_LYRICS_GOLDEN`, see `tests/lyrics_golden.rs`).

`<name>.case.json` is one song and what each lyrics service answers for it:

- `song`: what the bar widget passed its lookups (`title`, `artist`, `album`, `songDuration`).
- `answers`: by link, `{"json": body}` for a 2xx, `{"status": n}` for another status, or
  `{"fail": true}` for no answer at all.
- `youtube`: YouTube Music's plain lyrics step: `{"text", "source"}`, `"none"`, or `"fail"`.

A KuGou `content` was built the way KuGou's are read: the KRC text, zlib-compressed, XORed with
the 16-byte key in `src/lyrics/krc.rs`, after a 4-byte `krc1` tag, in base64.

`<name>.expected.json` is what the bar widget's own JavaScript made of the same answers: its
`loadLyrics` and every function it calls, taken unchanged from the widget's `Widget.qml` and
run by deno with the network calls answered from the case. `res` is the object the widget
showed (`{"none": true}` for none), `kept` whether it kept it, `asked` the links it asked for.
The Rust chain must match all three, field by field.
