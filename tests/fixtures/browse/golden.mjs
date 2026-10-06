// Throwaway: run the widget's Page.js over each scrubbed fixture with a fake fetch, write the expected output.
const dir = Deno.args[0], out = Deno.args[1];
const SOURCE = await Deno.readTextFile(Deno.args[2]);
const load = async (n) => JSON.parse(await Deno.readTextFile(`${dir}/${n}.json`));
let routes = {};
globalThis.window = { __nicYtmLen: 1, location: { pathname: "/" } };
const app = { networkManager: { fetch: (path, body) => Promise.resolve(routes[path](body)) } };
globalThis.document = { querySelector: (s) => (s === "ytmusic-app" ? app : s === "#movie_player" ? {} : null) };
globalThis.navigator = { onLine: true };
(0, eval)(SOURCE);
const Y = window.__nicYtm;
const cases = [];
const add = (name, fn) => cases.push([name, fn]);
for (const f of ["browse_home","browse_library_landing","browse_liked_playlists","browse_liked_albums","browse_library_corpus_track_artists","browse_playlist","browse_album","browse_artist","browse_podcast"])
  add(f, async () => { const d = await load(f); routes = { "/browse": () => d }; return Y.browse("x"); });
for (const f of ["browse_home_cont","browse_liked_playlists_cont","browse_library_corpus_track_artists_cont","browse_playlist_cont"])
  add(f, async () => { const d = await load(f); routes = { "/browse": () => d }; return Y.more("/browse", "t"); });
add("search_mixed", async () => { const d = await load("search_mixed"); routes = { "/search": () => d }; return Y.search("q"); });
for (const f of ["search_songs","search_albums","search_podcasts"])
  add(f, async () => { const d = await load(f); routes = { "/search": () => d }; return Y.search("q", "p"); });
add("search_songs_cont", async () => { const d = await load("search_songs_cont"); routes = { "/search": () => d }; return Y.more("/search", "t"); });
add("lyrics", async () => { const n = await load("next_song_for_lyrics"), b = await load("browse_lyrics"); routes = { "/next": () => n, "/browse": () => b }; return Y.lyrics("x"); });
await Deno.mkdir(out, { recursive: true });
for (const [name, fn] of cases) {
  try { const r = await fn(); await Deno.writeTextFile(`${out}/${name}.json`, JSON.stringify(r, null, 1)); console.log(name, JSON.stringify(r).length); }
  catch (e) { console.log(name, "ERROR", String(e)); }
}
