//! The challenge solver: yt-dlp's own `ejs` scripts in an embedded QuickJS.
//!
//! A stream link carries up to two challenges that only YouTube's player script can answer:
//! the `n` parameter (unsolved, the download is throttled to a crawl) and, for some formats,
//! the signature in a `signatureCipher` (unsolved, the link is refused). yt-dlp answers them by
//! running its `ejs` scripts (`assets/ejs`, yt-dlp-ejs 0.8.0, public domain) on the player
//! script in a JS runtime; this module does the same in QuickJS (`rquickjs`, which bundles
//! quickjs-ng), inside the engine instead of a separate process.
//!
//! How the scripts are driven follows yt-dlp's `jsc/_builtin/ejs.py`: run `lib`, then
//! `Object.assign(globalThis, lib)`, then `core`, then call `jsc(data)`, where `data` is
//! `{type: "player", player, requests, output_preprocessed: true}` the first time a player
//! version is seen and `{type: "preprocessed", preprocessed_player, requests}` after that.
//! The preprocessed player (the player cut down to the two solver functions) is cached in
//! `players/{id}.js`, so a warm call skips parsing the whole 3 MB player.
//!
//! The kept player: `core`'s `preprocessed` mode runs the whole 4 MB preprocessed player
//! again on every call (`Function("_result", player)(...)` inside `jsc`), which cost 172 ms
//! per song (measured 2026-10-13, player 5203c085, one performance core). So warm calls skip
//! `jsc`: `GLUE_JS` runs the preprocessed player once, keeps the two functions it leaves in
//! `_result`, and answers later calls by calling them (well under a millisecond). Only one
//! version is kept; a new one replaces it. `prime` loads it ahead of the solve, while the
//! `player` request is still in flight, so even the first song doesn't wait for it.
//!
//! Sandbox: the context has the base objects plus a short list of intrinsics the scripts need
//! (see `Js::new`) and no module loader, and quickjs-libc's `std`/`os` modules are never added,
//! so the scripts have no file, network or OS access. Each call gets a 5 s deadline (an
//! interrupt handler) and the runtime a 64 MiB memory limit, except the one call per player
//! version that preprocesses the whole player (see `COLD_DEADLINE`).
//!
//! Threading: an rquickjs `Runtime` is `!Send` (the crate's `parallel` feature would make it
//! `Send`, at the cost of a lock on every call), and a solve is a few hundred ms of CPU that
//! must not stall the tokio runtime. So the `Runtime` and its warm `Context` live on one
//! dedicated thread for the `Solver`'s lifetime, created there and never moved. Callers send
//! jobs over a channel and await a oneshot reply. One thread also serialises the calls, which
//! the single JS context needs anyway. The thread ends when the `Solver` is dropped.

pub mod player_js;
mod prep;

use std::cell::Cell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rquickjs::context::intrinsic;
use rquickjs::{CatchResultExt, CaughtError, Context, Ctx, Function, Object, Runtime};
use tokio::sync::oneshot;

use crate::error::Error;

/// yt-dlp-ejs 0.8.0, unmodified; their SHA3-512 hashes match yt-dlp 2026.08.19's `vendor`
/// list. Bundled so the solver needs nothing at run time and can't be swapped on disk.
const LIB_JS: &str = include_str!("../../assets/ejs/lib.min.js");
const CORE_JS: &str = include_str!("../../assets/ejs/core.min.js");

/// Keeps one preprocessed player run, for the warm calls (see the module docs). `load` and the
/// answer shape mirror `core.min.js` 0.8.0's own `preprocessed` branch: run the player with a
/// `_result` object, then answer each request with `_result[type]`, errors as `{type:
/// "error"}`. Recheck both when the `ejs` scripts are updated; `quickjs_matches_deno`
/// compares these answers with yt-dlp's.
const GLUE_JS: &str = r#"
var __ytmfast = (() => {
    let id = null, fns = null;
    return {
        loaded: () => id,
        load(playerId, player) {
            id = null; fns = null;
            const r = { n: null, sig: null };
            Function("_result", player)(r);
            id = playerId; fns = r;
        },
        answer(requests) {
            return {
                type: "result",
                responses: requests.map(q => {
                    const f = fns[q.type];
                    if (!f) return { type: "error", error: `Failed to extract ${q.type} function` };
                    try {
                        return { type: "result", data: Object.fromEntries(q.challenges.map(c => [c, f(c)])) };
                    } catch (e) {
                        return { type: "error", error: e instanceof Error ? `${e.message}\n${e.stack}` : `${e}` };
                    }
                }),
            };
        },
    };
})();
"#;

/// The spec's limits for one call: a script that loops or hogs memory is cut, not waited on.
const CALL_DEADLINE: Duration = Duration::from_secs(5);
const MEMORY_LIMIT: usize = 64 << 20;
/// Wider limits for the one call per player version that preprocesses the whole player (the
/// `lib` parser builds a syntax tree of the 3 MB script and prints it back out). Measured on
/// player 8ab5c328 (2026-10-04, release build): it needs between 176 and 192 MiB, and takes
/// 3.2 s on a performance core, 5.0 s on an efficiency core and 5.5 s on a low-power core.
/// With the spec's 64 MiB / 5 s it never finishes, so no player would ever be cached and the
/// own-code path would never work. Every later call for that version is warm and keeps the
/// spec's limits.
const COLD_DEADLINE: Duration = Duration::from_secs(15);
const COLD_MEMORY_LIMIT: usize = 256 << 20;
/// QuickJS's own stack limit, and the thread's (with room to spare for the Rust frames). The
/// `lib` parser is recursive and the player script nests deeply; QuickJS's 1 MiB default is
/// close to what the 3 MB player needs.
const JS_STACK: usize = 4 << 20;
const THREAD_STACK: usize = 16 << 20;

/// A made-up `n` challenge, for checking that a prepared player answers at all.
const PROBE_N: &str = "ytmfastProbeN0123";

/// The two challenges a stream link can carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChallengeKind {
    /// The `n` query parameter: unsolved, YouTube throttles the download to a crawl.
    N,
    /// The `s` of a `signatureCipher`: unsolved, the link is refused.
    Sig,
}

impl ChallengeKind {
    /// The request `type` in the `jsc` protocol.
    fn as_str(self) -> &'static str {
        match self {
            ChallengeKind::N => "n",
            ChallengeKind::Sig => "sig",
        }
    }
}

/// Challenge in, answer out, for one kind.
pub type Answers = HashMap<String, String>;

/// What `Streams` needs from a solver. `Solver` is the real one; tests fake it.
#[async_trait]
pub trait ChallengeSolver: Send + Sync {
    /// True when `player_id`'s preprocessed player is cached, so `solve_batch` can be called
    /// without the player code (which saves reading the 3 MB script).
    fn has_player(&self, player_id: &str) -> bool;

    /// Gets `player_id`'s cached preprocessed player ready ahead of a `solve_batch`, without
    /// waiting. Does nothing when it isn't cached (a cold solve needs the player code anyway).
    fn prime(&self, _player_id: &str) {}

    /// Preprocesses and loads a new player version ahead of its first solve, without waiting
    /// (the Rust `prep` only: `jsc`'s seconds of CPU are left to a real solve).
    fn prepare(&self, _player_id: &str, _player_code: String) {}

    /// Solves every request in one `jsc` call. The answers come back in the order of
    /// `requests`. `player_code` is only needed when `has_player` is false; without either the
    /// call fails. Any challenge left unanswered fails the whole call.
    ///
    /// Errors: `StreamFailed` only when the scripts failed on this player (a thrown error, a
    /// missing answer, the deadline or the memory limit), which `Streams` remembers for the
    /// player version; `Internal` for our own faults (the solver thread gone, no player
    /// script given), which say nothing about the player and are not remembered.
    async fn solve_batch(
        &self,
        player_id: &str,
        player_code: Option<String>,
        requests: Vec<(ChallengeKind, Vec<String>)>,
    ) -> Result<Vec<Answers>, Error>;
}

/// The QuickJS solver. Cheap to create: the thread starts at once, the JS runtime only on
/// the first call.
pub struct Solver {
    jobs: mpsc::Sender<Job>,
    players_dir: PathBuf,
}

enum Job {
    Solve {
        player_id: String,
        player_code: Option<String>,
        requests: Vec<(ChallengeKind, Vec<String>)>,
        reply: oneshot::Sender<Result<Vec<Answers>, Error>>,
    },
    /// Loads a cached preprocessed player (see `ChallengeSolver::prime`).
    Prime { player_id: String },
    /// Preprocesses a new one (see `ChallengeSolver::prepare`).
    Prepare {
        player_id: String,
        player_code: String,
    },
    /// Runs any code in the sandbox, for the sandbox tests.
    #[cfg(test)]
    Eval {
        code: String,
        reply: oneshot::Sender<Result<String, Error>>,
    },
}

impl Solver {
    /// A solver that caches preprocessed players in `{cache_dir}/players/`.
    pub fn new(cache_dir: PathBuf) -> Solver {
        Self::with_scripts(cache_dir, LIB_JS, CORE_JS)
    }

    /// `new` with stand-in scripts, so tests can check the `jsc` protocol.
    fn with_scripts(cache_dir: PathBuf, lib: &'static str, core: &'static str) -> Solver {
        let players_dir = cache_dir.join("players");
        let (jobs, inbox) = mpsc::channel();
        let dir = players_dir.clone();
        std::thread::Builder::new()
            .name("ytmfast-solver".into())
            .stack_size(THREAD_STACK)
            .spawn(move || {
                // Built here: the JS runtime it will hold is `!Send` and must never move.
                let mut worker = Worker {
                    lib,
                    core,
                    js: None,
                    players_dir: dir,
                };
                // Ends when every `Sender` is gone, that is when the `Solver` is dropped.
                while let Ok(job) = inbox.recv() {
                    worker.run(job);
                }
            })
            .expect("the solver thread should start");
        Solver { jobs, players_dir }
    }

    /// Solves `challenges` of one `kind` for player `player_id`. A shortcut for one-kind
    /// callers; `solve_batch` answers both kinds in one call.
    pub async fn solve(
        &self,
        player_id: &str,
        player_code: Option<&str>,
        kind: ChallengeKind,
        challenges: &[String],
    ) -> Result<Answers, Error> {
        let mut answers = self
            .solve_batch(
                player_id,
                player_code.map(str::to_owned),
                vec![(kind, challenges.to_vec())],
            )
            .await?;
        answers
            .pop()
            .ok_or_else(|| Error::Internal("the solver gave no answer".into()))
    }

    #[cfg(test)]
    async fn eval(&self, code: &str) -> Result<String, Error> {
        let (reply, answer) = oneshot::channel();
        self.send(Job::Eval {
            code: code.to_owned(),
            reply,
        })?;
        answer.await.map_err(|_| solver_gone())?
    }

    fn send(&self, job: Job) -> Result<(), Error> {
        self.jobs.send(job).map_err(|_| solver_gone())
    }
}

fn quickjs_failed() -> Error {
    Error::Internal("could not start QuickJS".into())
}

fn solver_gone() -> Error {
    Error::Internal("the challenge solver stopped".into())
}

#[async_trait]
impl ChallengeSolver for Solver {
    fn has_player(&self, player_id: &str) -> bool {
        player_js::cache_path(&self.players_dir, player_id, player_js::PREPROCESSED_SUFFIX)
            .is_some_and(|p| p.is_file())
    }

    fn prepare(&self, player_id: &str, player_code: String) {
        if player_js::valid_player_id(player_id) {
            // A gone thread is reported by the solve that follows.
            let _ = self.send(Job::Prepare {
                player_id: player_id.to_owned(),
                player_code,
            });
        }
    }

    fn prime(&self, player_id: &str) {
        if player_js::valid_player_id(player_id) {
            // A gone thread is reported by the solve that follows.
            let _ = self.send(Job::Prime {
                player_id: player_id.to_owned(),
            });
        }
    }

    async fn solve_batch(
        &self,
        player_id: &str,
        player_code: Option<String>,
        requests: Vec<(ChallengeKind, Vec<String>)>,
    ) -> Result<Vec<Answers>, Error> {
        if !player_js::valid_player_id(player_id) {
            return Err(Error::Internal("not a player id".into()));
        }
        let (reply, answer) = oneshot::channel();
        self.send(Job::Solve {
            player_id: player_id.to_owned(),
            player_code,
            requests,
            reply,
        })?;
        answer.await.map_err(|_| solver_gone())?
    }
}

/// The solver thread's state. Never leaves that thread.
struct Worker {
    lib: &'static str,
    core: &'static str,
    /// The warm runtime, built on first use and rebuilt after a call fails (a cut call can
    /// leave half-built state behind, and a fresh context costs only the `lib` parse).
    js: Option<Js>,
    players_dir: PathBuf,
}

impl Worker {
    fn run(&mut self, job: Job) {
        match job {
            Job::Solve {
                player_id,
                player_code,
                requests,
                reply,
            } => {
                let result = self.solve(&player_id, player_code, &requests);
                if result.is_err() {
                    self.reset();
                }
                // The caller may have given up (dropped its future); nothing to do then.
                let _ = reply.send(result);
            }
            Job::Prepare {
                player_id,
                player_code,
            } => {
                if self.prepare(&player_id, &player_code).is_err() {
                    self.reset();
                }
            }
            Job::Prime { player_id } => {
                // Best effort: if this fails, the solve that follows loads it again and
                // reports the error.
                if self.load(&player_id).is_err() {
                    self.reset();
                }
            }
            #[cfg(test)]
            Job::Eval { code, reply } => {
                let result = self.js().and_then(|js| js.eval(&code));
                if result.is_err() {
                    self.reset();
                }
                let _ = reply.send(result);
            }
        }
        // After the reply, so the caller doesn't wait for it: each call leaves megabytes of
        // player text and bytecode behind, and QuickJS frees cycles only in a GC pass.
        if let Some(js) = &self.js {
            // The scripts queue nothing today; draining keeps a warm context from piling up
            // jobs (and the objects they hold) if a future player does.
            if !js.drain_jobs() {
                self.reset();
            } else {
                js.rt.run_gc();
            }
        }
        // QuickJS frees into malloc, and glibc keeps freed pages in the process: without this
        // a cold solve (about 190 MiB at its peak) leaves the engine at that size for good.
        #[cfg(target_env = "gnu")]
        // SAFETY: malloc_trim only returns free heap pages to the OS; no preconditions.
        unsafe {
            libc::malloc_trim(0);
        }
    }

    /// Throws the runtime away after a failed call, so the next call starts from a clean
    /// `lib` + `core` (a cut call can stop halfway through setting globals).
    fn reset(&mut self) {
        if let Some(js) = self.js.take() {
            js.dispose();
        }
    }

    fn js(&mut self) -> Result<&Js, Error> {
        if self.js.is_none() {
            self.js = Some(Js::new(self.lib, self.core)?);
        }
        Ok(self.js.as_ref().expect("just set"))
    }

    fn solve(
        &mut self,
        player_id: &str,
        player_code: Option<String>,
        requests: &[(ChallengeKind, Vec<String>)],
    ) -> Result<Vec<Answers>, Error> {
        // Warm: the kept player (primed, or left by an earlier song), or the cached one.
        if self.load(player_id)? {
            let answers = self.js()?.answer(requests)?;
            crate::trace::mark("solver answered from the kept player");
            return Ok(answers);
        }
        // Cold: the whole player through `jsc`, which hands back the preprocessed player.
        // Internal, not StreamFailed, without the code: the caller's slip (or the cache
        // pruned since it asked `has_player`), which says nothing about this player (see the
        // trait).
        let Some(code) = player_code else {
            return Err(Error::Internal(
                "the player script is needed but was not given".into(),
            ));
        };
        // The Rust prep first (`prep`: about 3 s and 190 MB less than `jsc`'s), then the kept
        // player answers. If either step fails, `jsc` below is the reference and gets a go.
        match prep::preprocess(&code) {
            Some(pre) => {
                crate::trace::mark("player preprocessed");
                let js = self.js()?;
                match js.load(player_id, &pre).and_then(|()| js.answer(requests)) {
                    Ok(answers) => {
                        crate::trace::mark("solver answered from the kept player");
                        self.store_preprocessed(player_id, &pre);
                        return Ok(answers);
                    }
                    Err(e) => {
                        eprintln!(
                            "ytmfast: the player prep's answer failed ({}); trying yt-dlp's",
                            e.code()
                        );
                        self.reset();
                    }
                }
            }
            None => eprintln!("ytmfast: the player prep did not know this player; trying yt-dlp's"),
        }
        let js = self.js()?;
        js.rt.set_memory_limit(COLD_MEMORY_LIMIT);
        let output = js.call_jsc(Input::Player(&code), requests, COLD_DEADLINE);
        js.rt.set_memory_limit(MEMORY_LIMIT);
        let output = output?;
        if let Some(pre) = &output.preprocessed_player {
            self.store_preprocessed(player_id, pre);
        }
        Ok(output.answers)
    }

    /// `Job::Prepare`: the Rust prep, kept and cached only once it answers a probe challenge,
    /// so a prep that runs but can't solve never reaches the cache (where a song would meet
    /// it as a failing player instead of falling back to `jsc`).
    fn prepare(&mut self, player_id: &str, code: &str) -> Result<(), Error> {
        if self.load(player_id)? {
            return Ok(());
        }
        let Some(pre) = prep::preprocess(code) else {
            return Ok(());
        };
        let js = self.js()?;
        js.load(player_id, &pre)?;
        js.answer(&[(ChallengeKind::N, vec![PROBE_N.into()])])?;
        self.store_preprocessed(player_id, &pre);
        Ok(())
    }

    fn store_preprocessed(&self, player_id: &str, pre: &str) {
        // A failed write only costs the next call a cold solve; the answer is good.
        if let Err(e) = player_js::store_cached(
            &self.players_dir,
            player_id,
            player_js::PREPROCESSED_SUFFIX,
            pre,
        ) {
            eprintln!("ytmfast: could not cache the preprocessed player: {e}");
        }
    }

    /// Makes `player_id` the kept player. True when it is kept (already, or loaded now from
    /// the disk cache); false when it isn't cached, which leaves a cold solve.
    fn load(&mut self, player_id: &str) -> Result<bool, Error> {
        let built = self.js.is_none();
        let js = self.js()?;
        if built {
            crate::trace::mark("solver runtime built");
        }
        if js.loaded()?.as_deref() == Some(player_id) {
            return Ok(true);
        }
        let dir = self.players_dir.clone();
        let Some(pre) = player_js::load_cached(&dir, player_id, player_js::PREPROCESSED_SUFFIX)
        else {
            return Ok(false);
        };
        crate::trace::mark("preprocessed player from the cache");
        self.js()?.load(player_id, &pre)?;
        crate::trace::mark("preprocessed player kept");
        Ok(true)
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.reset();
    }
}

enum Input<'a> {
    Player(&'a str),
}

struct JscOutput {
    answers: Vec<Answers>,
    preprocessed_player: Option<String>,
}

/// One QuickJS runtime with `lib` and `core` loaded.
struct Js {
    // Field order is drop order: the context goes before its runtime.
    ctx: Context,
    rt: Runtime,
    /// When the running call must stop. Read by the interrupt handler, on this same thread.
    deadline: Rc<Cell<Option<Instant>>>,
    /// Set by the interrupt handler when it cut a call, to tell a timeout from other errors.
    cut: Rc<Cell<bool>>,
}

impl Js {
    fn new(lib: &str, core: &str) -> Result<Js, Error> {
        // Internal: QuickJS failing to start is ours, not the player's (see the trait).
        let rt = Runtime::new().map_err(|_| quickjs_failed())?;
        rt.set_memory_limit(MEMORY_LIMIT);
        rt.set_max_stack_size(JS_STACK);
        let deadline: Rc<Cell<Option<Instant>>> = Rc::default();
        let cut: Rc<Cell<bool>> = Rc::default();
        {
            let (deadline, cut) = (deadline.clone(), cut.clone());
            rt.set_interrupt_handler(Some(Box::new(move || {
                let over = deadline.get().is_some_and(|d| Instant::now() >= d);
                if over {
                    cut.set(true);
                }
                over
            })));
        }
        // The base objects (Object, Array, String, Function, Error, Math, ...) plus what the
        // scripts use: the player builds RegExps and Maps, `core` calls `Function(...)` on the
        // preprocessed player (Eval), and `lib` and the player use JSON, Date and typed
        // arrays. Promise is not used by the scripts but must be there: without it an async
        // function or `import()` leaves QuickJS half-built objects that make freeing the
        // runtime abort (seen in the sandbox test). Left out: Proxy, WeakRef, Performance,
        // DOMException.
        let ctx = Context::custom::<(
            intrinsic::Eval,
            intrinsic::RegExpCompiler,
            intrinsic::RegExp,
            intrinsic::Json,
            intrinsic::MapSet,
            intrinsic::Date,
            intrinsic::TypedArrays,
            intrinsic::Promise,
        )>(&rt)
        .map_err(|_| quickjs_failed())?;
        let js = Js {
            ctx,
            rt,
            deadline,
            cut,
        };
        js.guarded(CALL_DEADLINE, |ctx| {
            ctx.eval::<(), _>(lib).catch(&ctx).map_err(|e| caught(&e))?;
            ctx.eval::<(), _>("Object.assign(globalThis, lib);")
                .catch(&ctx)
                .map_err(|e| caught(&e))?;
            ctx.eval::<(), _>(core)
                .catch(&ctx)
                .map_err(|e| caught(&e))?;
            ctx.eval::<(), _>(GLUE_JS)
                .catch(&ctx)
                .map_err(|e| caught(&e))
        })?;
        Ok(js)
    }

    /// Runs the jobs a script queued (promise callbacks, a dynamic `import()`, which fails:
    /// there is no module loader) to the end, under the 5 s deadline. Returns false if some
    /// are still queued.
    fn drain_jobs(&self) -> bool {
        self.cut.set(false);
        self.deadline.set(Some(Instant::now() + CALL_DEADLINE));
        // Bounded: a job that queues another job forever stops at the deadline or here.
        for _ in 0..10_000 {
            if !self.rt.is_job_pending() || self.cut.get() {
                break;
            }
            let _ = self.rt.execute_pending_job();
        }
        self.deadline.set(None);
        !self.rt.is_job_pending()
    }

    /// Frees the runtime, unless that could abort the process. QuickJS asserts, when a
    /// runtime is freed, that no JS object is left, and a queued job holds objects. So the
    /// queue is drained first; if it won't drain, the runtime is leaked (a few MB, once)
    /// rather than freed, because the failed assert would abort the whole engine.
    fn dispose(self) {
        if !self.drain_jobs() {
            eprintln!("ytmfast: the challenge solver left work queued; its memory is not freed");
            std::mem::forget(self);
        }
    }

    /// Runs `f` in the context with the 5 s deadline armed, and turns a cut into a timeout
    /// error whatever `f` made of it.
    fn guarded<R>(
        &self,
        limit: Duration,
        f: impl FnOnce(Ctx<'_>) -> Result<R, Error>,
    ) -> Result<R, Error> {
        self.cut.set(false);
        self.deadline.set(Some(Instant::now() + limit));
        let result = self.ctx.with(f);
        self.deadline.set(None);
        if self.cut.get() {
            return Err(Error::StreamFailed(format!(
                "the challenge solver took over {} s",
                limit.as_secs()
            )));
        }
        result
    }

    fn call_jsc(
        &self,
        input: Input<'_>,
        requests: &[(ChallengeKind, Vec<String>)],
        limit: Duration,
    ) -> Result<JscOutput, Error> {
        self.guarded(limit, |ctx| {
            let run = || -> rquickjs::Result<Result<JscOutput, Error>> {
                // Built as JS values, not JSON text spliced into source: the player is 3 MB and
                // would otherwise be escaped, then parsed, then copied again.
                let data = Object::new(ctx.clone())?;
                match input {
                    Input::Player(code) => {
                        data.set("type", "player")?;
                        data.set("player", code)?;
                        data.set("output_preprocessed", true)?;
                    }
                }
                data.set("requests", requests_value(&ctx, requests)?)?;
                let jsc: Function = ctx.globals().get("jsc")?;
                let out: Object = jsc.call((data,))?;
                read_output(&out, requests)
            };
            run().catch(&ctx).map_err(|e| caught(&e))?
        })
    }

    /// The id of the kept player, if any.
    fn loaded(&self) -> Result<Option<String>, Error> {
        self.guarded(CALL_DEADLINE, |ctx| {
            ctx.eval::<Option<String>, _>("__ytmfast.loaded()")
                .catch(&ctx)
                .map_err(|e| caught(&e))
        })
    }

    /// Runs the preprocessed player once and keeps its two functions (`GLUE_JS`).
    fn load(&self, player_id: &str, preprocessed: &str) -> Result<(), Error> {
        self.guarded(CALL_DEADLINE, |ctx| {
            let run = || -> rquickjs::Result<()> {
                let glue: Object = ctx.globals().get("__ytmfast")?;
                let load: Function = glue.get("load")?;
                load.call((rquickjs::function::This(glue), player_id, preprocessed))
            };
            run().catch(&ctx).map_err(|e| caught(&e))
        })
    }

    /// Answers `requests` with the kept player's functions, in `jsc`'s answer shape.
    fn answer(&self, requests: &[(ChallengeKind, Vec<String>)]) -> Result<Vec<Answers>, Error> {
        self.guarded(CALL_DEADLINE, |ctx| {
            let run = || -> rquickjs::Result<Result<Vec<Answers>, Error>> {
                let reqs = requests_value(&ctx, requests)?;
                let glue: Object = ctx.globals().get("__ytmfast")?;
                let answer: Function = glue.get("answer")?;
                let out: Object = answer.call((rquickjs::function::This(glue), reqs))?;
                Ok(read_output(&out, requests)?.map(|o| o.answers))
            };
            run().catch(&ctx).map_err(|e| caught(&e))?
        })
    }

    #[cfg(test)]
    fn eval(&self, code: &str) -> Result<String, Error> {
        self.guarded(CALL_DEADLINE, |ctx| {
            ctx.eval::<String, _>(code)
                .catch(&ctx)
                .map_err(|e| caught(&e))
        })
    }
}

/// `requests` as the `jsc` protocol's `[{type, challenges}]`.
fn requests_value<'js>(
    ctx: &Ctx<'js>,
    requests: &[(ChallengeKind, Vec<String>)],
) -> rquickjs::Result<rquickjs::Array<'js>> {
    let reqs = rquickjs::Array::new(ctx.clone())?;
    for (i, (kind, challenges)) in requests.iter().enumerate() {
        let r = Object::new(ctx.clone())?;
        r.set("type", kind.as_str())?;
        r.set("challenges", challenges.clone())?;
        reqs.set(i, r)?;
    }
    Ok(reqs)
}

/// Reads `jsc`'s answer: `{type: "result", preprocessed_player?, responses: [{type:
/// "result", data: {challenge: answer}} | {type: "error", error}]}` or `{type: "error",
/// error}`. The protocol's own errors come back as `Ok(Err(..))`; only JS faults are
/// `rquickjs` errors.
fn read_output(
    out: &Object<'_>,
    requests: &[(ChallengeKind, Vec<String>)],
) -> rquickjs::Result<Result<JscOutput, Error>> {
    let kind: Option<String> = out.get("type")?;
    if kind.as_deref() != Some("result") {
        let msg: Option<String> = out.get("error").ok().flatten();
        return Ok(Err(js_failed(msg.as_deref().unwrap_or("no result"))));
    }
    let preprocessed_player: Option<String> = out.get("preprocessed_player")?;
    let responses: Vec<Object> = out.get("responses")?;
    if responses.len() != requests.len() {
        return Ok(Err(js_failed("wrong number of answers")));
    }
    let mut answers = Vec::with_capacity(requests.len());
    for (resp, (_, challenges)) in responses.iter().zip(requests) {
        let kind: Option<String> = resp.get("type")?;
        if kind.as_deref() != Some("result") {
            let msg: Option<String> = resp.get("error").ok().flatten();
            return Ok(Err(js_failed(msg.as_deref().unwrap_or("no result"))));
        }
        // `Option` values: the sig function answers `null` when it can't solve one.
        let data: HashMap<String, Option<String>> = resp.get("data")?;
        let mut solved = Answers::with_capacity(challenges.len());
        for c in challenges {
            match data.get(c) {
                Some(Some(a)) => {
                    solved.insert(c.clone(), a.clone());
                }
                _ => return Ok(Err(js_failed("a challenge was left unanswered"))),
            }
        }
        answers.push(solved);
    }
    Ok(Ok(JscOutput {
        answers,
        preprocessed_player,
    }))
}

/// A thrown JS error as one of ours. The message is cut short, and dropped if it names a URL:
/// no error may carry one (ruling R6), and stack traces aren't for the user.
fn caught(e: &CaughtError<'_>) -> Error {
    let msg = match e {
        CaughtError::Exception(ex) => ex.message().unwrap_or_default(),
        // QuickJS throws `null` when it runs out of memory so badly it can't even build the
        // error object (seen with the memory limit set below what a cold solve needs).
        CaughtError::Value(v) if v.is_null() => "out of memory".into(),
        // A thrown non-Error (core throws template strings): its `String(...)` form.
        CaughtError::Value(v) => v
            .get::<rquickjs::convert::Coerced<String>>()
            .map(|c| c.0)
            .unwrap_or_else(|_| format!("a script threw a {}", v.type_name())),
        CaughtError::Error(e) => e.to_string(),
    };
    js_failed(&msg)
}

fn js_failed(msg: &str) -> Error {
    let msg: String = msg.lines().next().unwrap_or("").chars().take(200).collect();
    if msg.contains("://") || msg.is_empty() {
        return Error::StreamFailed("challenge solver failed".into());
    }
    Error::StreamFailed(format!("challenge solver: {msg}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn solver() -> (tempfile::TempDir, Solver) {
        let tmp = tempfile::tempdir().unwrap();
        let s = Solver::new(tmp.path().to_path_buf());
        (tmp, s)
    }

    #[tokio::test]
    async fn sandbox_has_no_io() {
        let (_tmp, s) = solver();
        let out = s
            .eval(
                "[typeof require, typeof fetch, typeof std, typeof os, typeof Deno, \
                  typeof process, typeof XMLHttpRequest, typeof import_meta].join(',')",
            )
            .await
            .unwrap();
        assert_eq!(out, ["undefined"; 8].join(","));
        // No module loader either: a dynamic import can't reach a file.
        let err = s.eval("import('/etc/passwd')").await;
        assert!(
            err.is_err() || !err.unwrap().contains("root"),
            "import must not load files"
        );
    }

    #[tokio::test]
    async fn infinite_loop_is_cut() {
        let (_tmp, s) = solver();
        let start = Instant::now();
        let r = s.eval("while(true){}").await;
        let took = start.elapsed();
        assert!(r.is_err());
        assert!(took < Duration::from_secs(6), "took {took:?}");
        assert!(took >= Duration::from_secs(4), "cut too early: {took:?}");
        // The solver still works after a cut.
        assert_eq!(s.eval("String(1 + 1)").await.unwrap(), "2");
    }

    #[tokio::test]
    async fn memory_hog_is_cut() {
        let (_tmp, s) = solver();
        // 200 MiB of strings, kept alive in an array.
        let r = s
            .eval("const a = []; for (let i = 0; i < 200; i++) a.push('x'.repeat(1 << 20) + i); String(a.length)")
            .await;
        assert!(r.is_err(), "{r:?}");
        assert_eq!(s.eval("String(1 + 1)").await.unwrap(), "2");
    }

    /// A stand-in `jsc` that answers each challenge with what it was called with, so the test
    /// can check the request shape the real `core.min.js` gets.
    const ECHO_LIB: &str = "var lib = { marker: 'lib-loaded' };";
    const ECHO_CORE: &str = r#"
        var jsc = function (d) {
            const code = d.type === "player" ? d.player : d.preprocessed_player;
            const out = {
                type: "result",
                responses: d.requests.map(r => ({
                    type: "result",
                    data: Object.fromEntries(r.challenges.map(c => [c,
                        [d.type, String(d.output_preprocessed), r.type, c, code, marker].join("|")]))
                })),
            };
            // A real (tiny) preprocessed player: `GLUE_JS` runs it, so it must set `_result`.
            // It counts its runs, so a test can tell a kept player from a reloaded one.
            if (d.type === "player" && d.output_preprocessed) out.preprocessed_player =
                `globalThis.runs = (globalThis.runs || 0) + 1;
                 _result.n = c => ["kept", "n", c, "${d.player}", runs].join("|");
                 _result.sig = c => ["kept", "sig", c, "${d.player}", runs].join("|");`;
            return out;
        };
    "#;

    #[tokio::test]
    async fn jsc_protocol() {
        let tmp = tempfile::tempdir().unwrap();
        let s = Solver::with_scripts(tmp.path().to_path_buf(), ECHO_LIB, ECHO_CORE);
        let id = "0000000a";
        assert!(!s.has_player(id));

        // Cold: the player code goes in, the preprocessed player comes back and is cached.
        let got = s
            .solve_batch(
                id,
                Some("CODE".into()),
                vec![
                    (ChallengeKind::N, vec!["n1".into(), "n2".into()]),
                    (ChallengeKind::Sig, vec!["s1".into()]),
                ],
            )
            .await
            .unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0]["n1"], "player|true|n|n1|CODE|lib-loaded");
        assert_eq!(got[0]["n2"], "player|true|n|n2|CODE|lib-loaded");
        assert_eq!(got[1]["s1"], "player|true|sig|s1|CODE|lib-loaded");
        assert!(s.has_player(id));
        let cached = tmp.path().join("players").join(format!("{id}.js"));
        assert!(
            std::fs::read_to_string(&cached)
                .unwrap()
                .contains("_result.n")
        );

        // Warm: no code needed; the cached preprocessed player is run once and kept.
        let got = s
            .solve(id, None, ChallengeKind::Sig, &["s2".to_string()])
            .await
            .unwrap();
        assert_eq!(got["s2"], "kept|sig|s2|CODE|1");
        // Kept: the next call doesn't run it again (still run 1), even with the file gone.
        std::fs::remove_file(&cached).unwrap();
        let got = s
            .solve(id, None, ChallengeKind::N, &["n3".to_string()])
            .await
            .unwrap();
        assert_eq!(got["n3"], "kept|n|n3|CODE|1");

        // Cold with no code is an error, not a guess. `internal`: the caller's slip (or the
        // cache pruned between `has_player` and the call), not a sign the scripts fail on
        // this player, so `Streams` doesn't mark the player as failed for it.
        let e = s
            .solve("0000000b", None, ChallengeKind::N, &[])
            .await
            .unwrap_err();
        assert_eq!(e.code(), "internal");
        // A malformed id never names a file.
        assert!(
            s.solve("../../etc", Some("x"), ChallengeKind::N, &[])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn prime_loads_the_cached_player_and_a_new_version_replaces_it() {
        let tmp = tempfile::tempdir().unwrap();
        let s = Solver::with_scripts(tmp.path().to_path_buf(), ECHO_LIB, ECHO_CORE);
        for (id, code) in [("0000000a", "A"), ("0000000b", "B")] {
            s.solve(id, Some(code), ChallengeKind::N, &["c".into()])
                .await
                .unwrap();
        }
        // Not cached: does nothing, and the solve after it still needs the code.
        s.prime("0000000c");
        let e = s
            .solve("0000000c", None, ChallengeKind::N, &["c".into()])
            .await;
        assert_eq!(e.unwrap_err().code(), "internal");
        // Primed, then answered from the kept player: one run (the counter is global).
        s.prime("0000000a");
        let got = s
            .solve("0000000a", None, ChallengeKind::N, &["x".into()])
            .await
            .unwrap();
        assert_eq!(got["x"], "kept|n|x|A|1");
        // A different version replaces the kept one, and switching back runs it again.
        let got = s
            .solve("0000000b", None, ChallengeKind::N, &["x".into()])
            .await
            .unwrap();
        assert_eq!(got["x"], "kept|n|x|B|2");
        let got = s
            .solve("0000000a", None, ChallengeKind::N, &["x".into()])
            .await
            .unwrap();
        assert_eq!(got["x"], "kept|n|x|A|3");
    }

    #[tokio::test]
    async fn prepare_caches_only_a_player_that_answers() {
        let tmp = tempfile::tempdir().unwrap();
        let s = Solver::with_scripts(tmp.path().to_path_buf(), ECHO_LIB, ECHO_CORE);
        // A tiny player in the two-statement shape whose URL builder reverses `n`.
        let good = r#"var p = {};
(function (g) {
    var window = this;
    g.U = function (b, sp, sig) { this.q = { s: sig }; };
    g.U.prototype.set = function (k, v) { this.q[k] = v; };
    g.U.prototype.get = function (k) { return this.q[k]; };
    g.U.prototype.go = function () { if (this.q.n) this.q.n = this.q.n.split("").reverse().join(""); };
    g.mk = function (a, b, c) { var u = new g.U(a, b, c); u.set("alr", "yes"); return u; };
})(p);"#;
        s.prepare("0000000a", good.into());
        let got = s
            .solve("0000000a", None, ChallengeKind::N, &["abc".into()])
            .await
            .unwrap();
        assert_eq!(got["abc"], "cba");
        assert!(s.has_player("0000000a"), "cached");
        // Its builder throws: it runs, but can't answer, so it is not cached.
        s.prepare(
            "0000000b",
            good.replace("u.set(\"alr\"", "throw 1; u.set(\"alr\""),
        );
        // Not a shape the prep knows: nothing happens.
        s.prepare("0000000c", "var x = 1;".into());
        // A solve waits behind the jobs above, so they are done by now.
        assert!(
            s.solve("0000000c", None, ChallengeKind::N, &[])
                .await
                .is_err()
        );
        assert!(!s.has_player("0000000b"));
        assert!(!s.has_player("0000000c"));
    }

    #[tokio::test]
    async fn kept_player_errors_become_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let players = tmp.path().join("players");
        std::fs::create_dir_all(&players).unwrap();
        // No sig function, and an n function that throws.
        std::fs::write(
            players.join("0000000a.js"),
            "_result.n = c => { throw new Error('boom ' + c); };",
        )
        .unwrap();
        // A player that doesn't even run.
        std::fs::write(players.join("0000000b.js"), "this is not javascript").unwrap();
        let s = Solver::with_scripts(tmp.path().to_path_buf(), ECHO_LIB, ECHO_CORE);
        let e = s
            .solve("0000000a", None, ChallengeKind::N, &["one".into()])
            .await
            .unwrap_err();
        assert!(e.to_string().contains("boom one"), "{e}");
        let e = s
            .solve("0000000a", None, ChallengeKind::Sig, &["s".into()])
            .await
            .unwrap_err();
        assert!(
            e.to_string().contains("Failed to extract sig function"),
            "{e}"
        );
        let e = s
            .solve("0000000b", None, ChallengeKind::N, &["c".into()])
            .await
            .unwrap_err();
        assert_eq!(e.code(), "stream_failed");
    }

    #[tokio::test]
    async fn jsc_errors_become_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let core = r#"var jsc = function (d) {
            if (d.requests[0].challenges[0] === "top") return { type: "error", error: "whole call failed" };
            return { type: "result", responses: [{ type: "error", error: "Failed to extract n function" }] };
        };"#;
        let s = Solver::with_scripts(tmp.path().to_path_buf(), ECHO_LIB, core);
        let e = s
            .solve("0000000a", Some("x"), ChallengeKind::N, &["top".into()])
            .await
            .unwrap_err();
        assert_eq!(e.code(), "stream_failed");
        let e = s
            .solve("0000000a", Some("x"), ChallengeKind::N, &["one".into()])
            .await
            .unwrap_err();
        assert!(
            e.to_string().contains("Failed to extract n function"),
            "{e}"
        );
    }

    #[tokio::test]
    async fn preprocessed_cache_keeps_three() {
        let tmp = tempfile::tempdir().unwrap();
        let s = Solver::with_scripts(tmp.path().to_path_buf(), ECHO_LIB, ECHO_CORE);
        let players = tmp.path().join("players");
        let ids = ["0000000a", "0000000b", "0000000c", "0000000d"];
        for (i, id) in ids.iter().enumerate() {
            s.solve(id, Some("CODE"), ChallengeKind::N, &["c".into()])
                .await
                .unwrap();
            // Distinct mtimes, oldest first, whatever the file system's clock resolution.
            let f = std::fs::File::options()
                .write(true)
                .open(players.join(format!("{id}.js")))
                .unwrap();
            f.set_modified(std::time::SystemTime::now() - Duration::from_secs(100 - i as u64 * 10))
                .unwrap();
        }
        // Storing the fourth pruned the folder: the first (oldest mtime) went.
        assert!(
            !players.join("0000000a.js").exists(),
            "the oldest is removed"
        );
        for id in &ids[1..] {
            assert!(players.join(format!("{id}.js")).exists(), "{id} is kept");
        }
        assert!(!s.has_player("0000000a"));
    }

    /// The real scripts on a real player give the same answers in QuickJS as in deno (the
    /// runtime yt-dlp prefers). The cold solve here goes through the Rust `prep`, so this is
    /// also the check that `prep` matches `jsc` (on 2026-10-13 it did on six players:
    /// 21090930, 22cb88bc, 5203c085, 7daa59ad, f2999a12 and the web app's ES6 22cb88bc;
    /// a "trying yt-dlp's" line in the output means it fell back and proved nothing). Needs `deno` and either `YTMFAST_TEST_PLAYER_JS` (a saved
    /// `base.js`, named `<player id>.base.js`) or the network for an unauthenticated download
    /// of the current player. Prints the cold and warm solve times.
    /// Run: `cargo test --release --lib quickjs_matches_deno -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "needs deno and a real player script"]
    async fn quickjs_matches_deno() {
        let (id, code) = match std::env::var_os("YTMFAST_TEST_PLAYER_JS") {
            Some(path) => {
                let path = std::path::PathBuf::from(path);
                let name = path.file_name().unwrap().to_str().unwrap().to_owned();
                let id = name.split('.').next().unwrap().to_owned();
                (id, std::fs::read_to_string(&path).unwrap())
            }
            None => {
                let http = crate::net::client("ytmfast-test/0");
                let id = player_js::current_player_id(&http).await.unwrap();
                let base = url::Url::parse(player_js::WEB_BASE).unwrap();
                let code = player_js::fetch_player(&http, &base, &id).await.unwrap();
                (id, code)
            }
        };
        let n: Vec<String> = ["ZdZIqFPQK-Ty8wId", "abcdefghijklmnop", "0123456789_-ABCD"]
            .map(String::from)
            .to_vec();
        let sig: Vec<String> = vec![
            "gN7a-hudCuAuPH6fByOk1_GNXN0yNMHShjZXS2VOgsEItAJz0tipeavEOmNdYN-wUtcEqD3bCXjc0iyKfAyZxCBGgIARwsSdQfJ2CJtt".into(),
            "AOq0QJ8wRAIgXmPlOPSBkkUs1bYFYlJCfe29xx8j7v1pDL0QwbdV96sCIEzpWqMGkFR20CFOg51Tp-7vj_EMu-m37KtXJ2OySqa0q".into(),
        ];
        let requests = vec![
            (ChallengeKind::N, n.clone()),
            (ChallengeKind::Sig, sig.clone()),
        ];

        let tmp = tempfile::tempdir().unwrap();
        let s = Solver::new(tmp.path().to_path_buf());
        let start = Instant::now();
        let cold = s
            .solve_batch(&id, Some(code.clone()), requests.clone())
            .await
            .unwrap();
        let cold_ms = start.elapsed().as_millis();
        // The GC and trim run after the reply; give them a moment.
        tokio::time::sleep(Duration::from_millis(300)).await;
        eprintln!("RSS after the cold solve: {} MiB", rss_mib());
        let mut warm_ms = Vec::new();
        for _ in 0..5 {
            // Calls are seconds apart in use; this keeps the clean-up after each one (GC,
            // trim) out of the next one's time.
            tokio::time::sleep(Duration::from_millis(100)).await;
            let start = Instant::now();
            let warm = s.solve_batch(&id, None, requests.clone()).await.unwrap();
            warm_ms.push(start.elapsed().as_millis());
            assert_eq!(warm, cold);
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        eprintln!("RSS after the warm solves: {} MiB", rss_mib());
        // A new solver (a restarted engine) with the preprocessed player on disk.
        drop(s);
        let s = Solver::new(tmp.path().to_path_buf());
        let start = Instant::now();
        let restarted = s.solve_batch(&id, None, requests.clone()).await.unwrap();
        let restart_ms = start.elapsed().as_millis();
        assert_eq!(restarted, cold);
        eprintln!(
            "player {id}: cold {cold_ms} ms, warm {warm_ms:?} ms, \
             first call after restart (warm disk cache) {restart_ms} ms"
        );

        // The same call in deno, the way yt-dlp makes it.
        let data = serde_json::json!({
            "type": "player",
            "player": code,
            "requests": [
                {"type": "n", "challenges": n},
                {"type": "sig", "challenges": sig},
            ],
            "output_preprocessed": false,
        });
        let script = format!(
            "{LIB_JS}\nObject.assign(globalThis, lib);\n{CORE_JS}\nconsole.log(JSON.stringify(jsc({data})));\n"
        );
        let mut child = std::process::Command::new("deno")
            .args([
                "run",
                "--ext=js",
                "--no-code-cache",
                "--no-prompt",
                "--no-remote",
                "--no-lock",
                "--node-modules-dir=none",
                "--no-config",
                "--no-npm",
                "--cached-only",
                "-",
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("deno is installed");
        {
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(script.as_bytes())
                .unwrap();
        }
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        let out: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let deno: Vec<Answers> = out["responses"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                assert_eq!(r["type"], "result", "{r}");
                serde_json::from_value(r["data"].clone()).unwrap()
            })
            .collect();
        assert_eq!(cold, deno);
        // Real answers, not echoes.
        for c in &n {
            assert_ne!(&cold[0][c], c);
        }
    }

    fn rss_mib() -> u64 {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let line = status.lines().find(|l| l.starts_with("VmRSS:")).unwrap();
        let kib: u64 = line.split_whitespace().nth(1).unwrap().parse().unwrap();
        kib / 1024
    }
}
