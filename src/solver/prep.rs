//! The cold solve's preprocessing, in Rust: what yt-dlp-ejs 0.8.0's `core.min.js` does to a
//! new player version before its first solve (its function `u`), done with oxc's parser and
//! a few text splices instead of meriyah + astring running inside QuickJS.
//!
//! Why: in QuickJS that step parses the 3 MB player into a syntax tree and prints all of it
//! back out. It took 3.1 s and peaked at 205 MB on every new player version (measured
//! 2026-10-13, players 5203c085 and 22cb88bc), and YouTube served three new versions in 15
//! minutes that evening. oxc parses the same player natively, and splicing the original text
//! (instead of printing a new one) skips the printing step.
//!
//! What `u` does, and so what this does:
//! 1. Find the function the player's code lives in: the only statement is
//!    `(function(){...}).call(...)`, or the second of two is `(function(g){...})(...)`. In the
//!    second form the function's first statement is dropped (`var window=this;`), so the
//!    player sees the stand-in `window` from step 4.
//! 2. Drop that function's expression statements that are neither an assignment nor a literal.
//! 3. Find every function there whose own body calls `X.y("alr", "yes")`: the player's URL
//!    builder, which applies the `n` and `sig` transforms. Each becomes a candidate solver, and
//!    `_result.n` / `_result.sig` are set to code that tries every candidate and insists they
//!    agree (the template texts below are `core`'s own).
//! 4. Put `core`'s stand-ins for `window`, `document` and friends in front of everything.
//!
//! The answer is checked against `jsc`'s on real players (`prep_matches_jsc`). Anything this
//! doesn't recognise gives `None`, and the solver falls back to `jsc`, which stays the
//! reference: when the `ejs` scripts are updated, this needs the same update.

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, ArrowFunctionBody, AssignmentOperator, AssignmentTarget, Expression, FunctionBody,
    Statement,
};
use oxc_parser::{ParseOptions, Parser};
use oxc_span::{GetSpan, SourceType, Span};

/// `core`'s stand-ins for the browser globals the player touches (its `l`), verbatim.
const PRELUDE: &str = r#"
if (typeof globalThis.XMLHttpRequest === "undefined") {
    globalThis.XMLHttpRequest = { prototype: {} };
}
if (typeof URL === "undefined") {
    globalThis.location = {
        hash: "",
        host: "www.youtube.com",
        hostname: "www.youtube.com",
        href: "https://www.youtube.com/watch?v=yt-dlp-wins",
        origin: "https://www.youtube.com",
        password: "",
        pathname: "/watch",
        port: "",
        protocol: "https:",
        search: "?v=yt-dlp-wins",
        username: "",
    };
} else {
    globalThis.location = new URL("https://www.youtube.com/watch?v=yt-dlp-wins");
}
if (typeof globalThis.document === "undefined") {
    globalThis.document = Object.create(null);
}
if (typeof globalThis.navigator === "undefined") {
    globalThis.navigator = Object.create(null);
}
if (typeof globalThis.self === "undefined") {
    globalThis.self = globalThis;
}
if (typeof globalThis.window === "undefined") {
    globalThis.window = globalThis;
}
"#;

/// `core`'s wrapper around one URL builder (its `a`): builds a URL with the challenges, runs
/// the builder's transform, reads the answers back. `{NAME}` is the builder's name.
const BUILDER_CALL: &str = r#"({sig, n}) => {
  const url = ({NAME})("https://youtube.com/watch?v=yt-dlp-wins", "s", sig ? encodeURIComponent(sig) : undefined);
  url.set("n", n);
  const proto = Object.getPrototypeOf(url);
  const keys = Object.keys(proto).concat(Object.getOwnPropertyNames(proto));
  for (const key of keys) {
    if (!["constructor", "set", "get", "clone"].includes(key)) {
      url[key]();
      break;
    }
  }
  const s = url.get("s");
  return {
    sig: s ? decodeURIComponent(s) : null,
    n: url.get("n") ?? null,
  };
}"#;

/// `core`'s "try every candidate, they must agree" function (its `f`). `{LIST}` is the
/// candidates.
const ALL_AGREE: &str = r#"(_input) => {
  const _results = new Set();
  const errors = [];
  for (const _generator of [{LIST}]) {
    try {
      _results.add(_generator(_input));
    } catch (e) {
      errors.push(e);
    }
  }
  if (!_results.size) {
    throw `no solutions: ${errors.join(", ")}`;
  }
  if (_results.size !== 1) {
    throw `invalid solutions: ${[..._results].map(x => JSON.stringify(x)).join(", ")}`;
  }
  return _results.values().next().value;
}"#;

/// The preprocessed player, as `jsc` would make it (same behaviour, not the same text), or
/// `None` when the player doesn't have the shape `core` expects.
pub fn preprocess(player: &str) -> Option<String> {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, player, SourceType::script())
        .with_options(ParseOptions {
            // meriyah's tree has no parenthesis nodes, and `core`'s shape checks assume that.
            preserve_parens: false,
            ..ParseOptions::default()
        })
        .parse();
    let program = &parsed.program;
    // A syntax error, or anything meriyah would count as an extra statement (it lists
    // directives and a hashbang among them): leave it to `jsc`.
    if !parsed.diagnostics.is_empty()
        || !program.directives.is_empty()
        || program.hashbang.is_some()
    {
        return None;
    }
    let (body, drop_first) = player_function(&program.body)?;
    if player.as_bytes().get(body.span.end as usize - 1) != Some(&b'}') {
        return None;
    }

    // Byte ranges of the original to replace, in order, never overlapping.
    let mut edits: Vec<(Span, String)> = Vec::new();
    let mut statements: &[Statement] = &body.statements;
    if drop_first {
        // `splice(0, 1)` on meriyah's body, where directives come first.
        if let Some(first) = body.directives.first() {
            edits.push((first.span, ";".into()));
        } else {
            let (first, rest) = statements.split_first()?;
            edits.push((first.span(), ";".into()));
            statements = rest;
        }
    }
    let mut builders = Vec::new();
    for statement in statements {
        if let Statement::ExpressionStatement(s) = statement
            && !matches!(s.expression, Expression::AssignmentExpression(_))
            && !s.expression.is_literal()
        {
            // `;` rather than nothing: a statement list stays a statement list.
            edits.push((s.span, ";".into()));
            continue;
        }
        if let Some(name) = url_builder(statement) {
            builders.push(name.source_text(player));
        }
    }

    let candidates = |kind: &str| -> String {
        let list: Vec<String> = builders
            .iter()
            .map(|name| {
                let call = BUILDER_CALL.replace("{NAME}", name);
                format!("({kind}) => ({call})({{{kind}}}).{kind}")
            })
            .collect();
        ALL_AGREE.replace("{LIST}", &list.join(", "))
    };
    let end = body.span.end - 1;
    edits.push((
        Span::new(end, end),
        format!(
            "\n_result.n = {};\n_result.sig = {};\n",
            candidates("n"),
            candidates("sig")
        ),
    ));

    let mut out = String::with_capacity(PRELUDE.len() + player.len() + 8192);
    out.push_str(PRELUDE);
    let mut at = 0usize;
    for (span, text) in edits {
        out.push_str(&player[at..span.start as usize]);
        out.push_str(&text);
        at = span.end as usize;
    }
    out.push_str(&player[at..]);
    Some(out)
}

/// Step 1: the body of the function the player lives in, and whether its first statement is
/// dropped.
fn player_function<'a>(program: &'a [Statement<'a>]) -> Option<(&'a FunctionBody<'a>, bool)> {
    match program {
        // `(function(){...}).call(this)`.
        [Statement::ExpressionStatement(s)] => {
            let Expression::CallExpression(call) = &s.expression else {
                return None;
            };
            let Expression::FunctionExpression(f) = call.callee.get_member_expr()?.object() else {
                return None;
            };
            Some((f.body.as_deref()?, false))
        }
        // `var _yt_player = {}; (function(g){...})(_yt_player);`
        [_, Statement::ExpressionStatement(s)] => {
            let Expression::CallExpression(call) = &s.expression else {
                return None;
            };
            let Expression::FunctionExpression(f) = &call.callee else {
                return None;
            };
            Some((f.body.as_deref()?, true))
        }
        _ => None,
    }
}

/// Step 3 for one statement (`core`'s `p`): the span of the name of the first function it
/// defines whose own body calls `X.y("alr", "yes")`.
fn url_builder(statement: &Statement<'_>) -> Option<Span> {
    let defs: Vec<(Span, &[Statement])> = match statement {
        // `name = function () {...}` (not async).
        Statement::ExpressionStatement(s) => {
            let Expression::AssignmentExpression(a) = &s.expression else {
                return None;
            };
            let Expression::FunctionExpression(f) = &a.right else {
                return None;
            };
            let named = matches!(
                a.left,
                AssignmentTarget::AssignmentTargetIdentifier(_)
                    | AssignmentTarget::StaticMemberExpression(_)
                    | AssignmentTarget::ComputedMemberExpression(_)
                    | AssignmentTarget::PrivateFieldExpression(_)
            );
            if a.operator != AssignmentOperator::Assign || f.r#async || !named {
                return None;
            }
            vec![(a.left.span(), &f.body.as_deref()?.statements)]
        }
        // `function name() {...}` (not async).
        Statement::FunctionDeclaration(f) => {
            if f.r#async {
                return None;
            }
            vec![(f.id.as_ref()?.span, &f.body.as_deref()?.statements)]
        }
        // `var a = function () {...}, b = ...`: once one declarator is a (not async) function
        // expression, `core` looks inside every declarator with a block body.
        Statement::VariableDeclaration(v) => {
            let any_function = v
                .declarations
                .iter()
                .any(|d| matches!(&d.init, Some(Expression::FunctionExpression(f)) if !f.r#async));
            if !any_function {
                return None;
            }
            v.declarations
                .iter()
                .filter_map(|d| {
                    let statements: &[Statement] = match d.init.as_ref()? {
                        Expression::FunctionExpression(f) => &f.body.as_deref()?.statements,
                        Expression::ArrowFunctionExpression(a) => match &a.body {
                            ArrowFunctionBody::FunctionBody(b) => &b.statements,
                            _ => return None,
                        },
                        _ => return None,
                    };
                    Some((d.id.span(), statements))
                })
                .collect()
        }
        _ => return None,
    };
    defs.into_iter()
        .find(|(_, body)| body.iter().any(is_alr_call))
        .map(|(name, _)| name)
}

/// `X.y("alr", "yes");` (or `X[y](...)`), no optional chaining: the player's URL builder
/// marks its URLs with it.
fn is_alr_call(statement: &Statement<'_>) -> bool {
    let Statement::ExpressionStatement(s) = statement else {
        return false;
    };
    let Expression::CallExpression(call) = &s.expression else {
        return false;
    };
    let Some(member) = call.callee.get_member_expr() else {
        return false;
    };
    let string = |a: &Argument<'_>, want: &str| matches!(a, Argument::StringLiteral(l) if l.value.as_str() == want);
    !call.optional
        && !member.optional()
        && matches!(member.object(), Expression::Identifier(_))
        && call.arguments.len() == 2
        && string(&call.arguments[0], "alr")
        && string(&call.arguments[1], "yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny player in the two-statement shape, with one URL builder (an `n` transform that
    /// reverses, a `sig` transform that uppercases) and the statements `core` drops.
    const TINY: &str = r#"var _yt_player = {};
(function (g) {
    var window = this;
    console.log("dropped: not an assignment");
    g.U = function (base, sp, sig) {
        this.q = { n: null, s: sig };
        this.set = function (k, v) { this.q[k] = v; };
        this.get = function (k) { return this.q[k]; };
    };
    g.U.prototype.go = function () {
        if (this.q.n) this.q.n = this.q.n.split("").reverse().join("");
        if (this.q.s) this.q.s = decodeURIComponent(this.q.s).toUpperCase();
    };
    g.mk = function (a, b, c) {
        var u = new g.U(a, b, c);
        u.set("alr", "yes");
        return u;
    };
    function notABuilder() { return 1; }
})(_yt_player);
"#;

    fn run(pre: &str, kind: &str, challenge: &str) -> String {
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            let code = format!(
                "const _r = {{}}; Function('_result', {})(_r); _r.{kind}({challenge:?})",
                serde_json::to_string(pre).unwrap()
            );
            ctx.eval::<String, _>(code).unwrap()
        })
    }

    #[test]
    fn tiny_player_solves_both_kinds() {
        let pre = preprocess(TINY).unwrap();
        assert!(pre.starts_with(PRELUDE));
        assert!(!pre.contains("console.log"), "dropped statement");
        assert!(
            !pre.contains("var window = this"),
            "first statement dropped"
        );
        assert!(
            pre.contains("const url = (g.mk)(\"https://youtube.com"),
            "{pre}"
        );
        assert_eq!(run(&pre, "n", "abc"), "cba");
        assert_eq!(run(&pre, "sig", "x/y"), "X/Y");
    }

    #[test]
    fn one_statement_shape_keeps_its_first_statement() {
        let player = "(function () { var keep = 1; g = {}; g.mk = function (a) { a.x(\"alr\", \"yes\"); }; }).call(this);";
        let pre = preprocess(player).unwrap();
        assert!(pre.contains("var keep = 1;"));
        assert!(pre.contains("(g.mk)(\"https://youtube.com"));
    }

    #[test]
    fn builder_detection_follows_core() {
        let found = |src: &str| -> Vec<String> {
            let player = format!("var p = {{}};\n(function (g) {{ var w = this; {src} }})(p);");
            let pre = preprocess(&player).unwrap();
            let mut names = Vec::new();
            for line in pre.lines() {
                if let Some(rest) = line.trim().strip_prefix("const url = (")
                    && let Some((name, _)) = rest.split_once(")(\"https")
                {
                    names.push(name.to_string());
                }
            }
            // Each builder is listed twice: once for `n`, once for `sig`.
            names.truncate(names.len() / 2);
            names
        };
        let alr = r#"u.set("alr", "yes");"#;
        assert_eq!(found(&format!("g.a = function () {{ {alr} }};")), ["g.a"]);
        assert_eq!(found(&format!("function b() {{ {alr} }}")), ["b"]);
        assert_eq!(
            found(&format!("var c = 1, d = function () {{ {alr} }};")),
            ["d"]
        );
        // An arrow in a declaration counts once a function expression is there too.
        assert_eq!(
            found(&format!("var e = () => {{ {alr} }}, f = function () {{}};")),
            ["e"]
        );
        // Not builders: async, nested deeper, optional call, other strings, `+=`.
        assert!(found(&format!("g.h = async function () {{ {alr} }};")).is_empty());
        assert!(found(&format!("g.i = function () {{ if (1) {{ {alr} }} }};")).is_empty());
        assert!(found(r#"g.j = function () { u.set?.("alr", "yes"); };"#).is_empty());
        assert!(found(r#"g.k = function () { u.set("alr", "no"); };"#).is_empty());
        assert!(found(&format!("g.l += function () {{ {alr} }};")).is_empty());
    }

    #[test]
    fn unexpected_shapes_are_left_to_jsc() {
        assert!(preprocess("var a = 1; var b = 2; var c = 3;").is_none());
        assert!(preprocess("(function () {").is_none(), "syntax error");
        assert!(preprocess("\"use strict\"; (function () {})();").is_none());
        assert!(preprocess("f();").is_none());
    }
}
