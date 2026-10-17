# Explore and Related fixtures

These three answers are **hand-made**, not captured from YouTube Music. They follow the shapes the
real `browse` answers use (the same renderers the scrubbed fixtures in `../browse` hold:
`musicCarouselShelfRenderer`, `musicTwoRowItemRenderer`, `musicResponsiveListItemRenderer`,
`gridRenderer`), with invented text and `fake…` ids that keep YouTube's prefixes (`MPREb_`, `UC`,
`VL`, `OLAK5uy_`, `FEmusic_…`). Nothing in them is a real song, link or token.

| File | What YouTube Music call it stands for |
|---|---|
| `browse_explore.json` | `browse {browseId: "FEmusic_explore"}`: three navigation buttons (New releases, Charts, Moods & genres), then shelves of albums, songs and videos. |
| `browse_moods.json` | `browse {browseId: "FEmusic_moods_and_genres"}`: grids of navigation buttons that carry a `params` (a mood or genre page). |
| `browse_related.json` | `browse {browseId: <the Related tab's id from a song's next>}`: shelves of similar songs, playlists and artists, then an "About the artist" text block that is not a row. |

The Related tab itself (the `MUSIC_PAGE_TYPE_TRACK_RELATED` tab in a song's `next` answer) is read
from the real, scrubbed `../browse/next_song_for_lyrics.json`.

Because they are hand-made, they prove the parser reads these shapes; they cannot prove that YouTube
Music still sends exactly these. If a real answer is ever saved, scrub it as `../browse/BROWSE_FIXTURES.md`
says and add it beside these.
