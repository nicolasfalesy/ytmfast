//! The lyrics services' HTTP client (`lyrics::HttpWeb`) against a local wiremock server: how
//! each kind of answer is sorted, the 2 MiB cap, no redirects, the timeout, and that the real
//! client refuses any host but the three lyrics hosts before sending anything. Never the real
//! services.

use std::time::Duration;

use serde_json::json;
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use ytmfast::lyrics::{Fetched, HttpWeb, LyricsWeb};

/// The test client: any http link on the local server, a short timeout.
fn local() -> HttpWeb {
    fn loopback(u: &Url) -> bool {
        u.host_str() == Some("127.0.0.1")
    }
    HttpWeb::with(loopback, Duration::from_millis(500))
}

async fn answer(server: &MockServer, at: &str, reply: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path(at))
        .respond_with(reply)
        .mount(server)
        .await;
}

#[tokio::test]
async fn answers_are_sorted_as_the_widget_did() {
    let server = MockServer::start().await;
    let ok = json!({"syncedLyrics": "[00:01.00] a"});
    answer(
        &server,
        "/json",
        ResponseTemplate::new(200).set_body_json(&ok),
    )
    .await;
    answer(
        &server,
        "/list",
        ResponseTemplate::new(200).set_body_json(json!([])),
    )
    .await;
    answer(
        &server,
        "/null",
        ResponseTemplate::new(200).set_body_string("null"),
    )
    .await;
    answer(
        &server,
        "/html",
        ResponseTemplate::new(200).set_body_string("<html>"),
    )
    .await;
    answer(&server, "/empty", ResponseTemplate::new(204)).await;
    for (at, status) in [("/404", 404), ("/400", 400)] {
        answer(&server, at, ResponseTemplate::new(status)).await;
    }
    for (at, status) in [("/408", 408), ("/429", 429), ("/500", 500), ("/503", 503)] {
        answer(&server, at, ResponseTemplate::new(status)).await;
    }
    let web = local();
    let get = |p: &str| format!("{}{p}", server.uri());
    assert_eq!(web.get_json(&get("/json")).await, Fetched::Json(ok));
    assert_eq!(web.get_json(&get("/list")).await, Fetched::Json(json!([])));
    for p in ["/null", "/html", "/empty", "/408", "/429", "/500", "/503"] {
        assert_eq!(web.get_json(&get(p)).await, Fetched::Failed, "{p}");
    }
    for p in ["/404", "/400"] {
        assert_eq!(web.get_json(&get(p)).await, Fetched::NotFound, "{p}");
    }
}

#[tokio::test]
async fn an_answer_over_2_mib_is_dropped() {
    let server = MockServer::start().await;
    let at_cap = format!("\"{}\"", "a".repeat(2 * 1024 * 1024 - 2));
    let over = format!("\"{}\"", "a".repeat(2 * 1024 * 1024 - 1));
    answer(
        &server,
        "/cap",
        ResponseTemplate::new(200).set_body_string(at_cap),
    )
    .await;
    answer(
        &server,
        "/over",
        ResponseTemplate::new(200).set_body_string(over),
    )
    .await;
    let web = local();
    assert!(matches!(
        web.get_json(&format!("{}/cap", server.uri())).await,
        Fetched::Json(_)
    ));
    assert_eq!(
        web.get_json(&format!("{}/over", server.uri())).await,
        Fetched::Failed
    );
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let server = MockServer::start().await;
    let target = format!("{}/elsewhere", server.uri());
    answer(
        &server,
        "/moved",
        ResponseTemplate::new(302).insert_header("Location", target.as_str()),
    )
    .await;
    Mock::given(path("/elsewhere"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"a": 1})))
        .expect(0)
        .mount(&server)
        .await;
    let web = local();
    assert_eq!(
        web.get_json(&format!("{}/moved", server.uri())).await,
        Fetched::Failed
    );
    // `expect(0)` is checked when the server drops.
}

#[tokio::test]
async fn a_slow_answer_times_out() {
    let server = MockServer::start().await;
    answer(
        &server,
        "/slow",
        ResponseTemplate::new(200)
            .set_body_json(json!({"a": 1}))
            .set_delay(Duration::from_secs(3)),
    )
    .await;
    let started = std::time::Instant::now();
    assert_eq!(
        local().get_json(&format!("{}/slow", server.uri())).await,
        Fetched::Failed
    );
    assert!(started.elapsed() < Duration::from_secs(2));
}

/// The real client sends nothing to a host that is not one of the three lyrics hosts.
#[tokio::test]
async fn the_real_client_refuses_other_hosts() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"a": 1})))
        .expect(0)
        .mount(&server)
        .await;
    let web = HttpWeb::new();
    assert_eq!(
        web.get_json(&format!("{}/api/get", server.uri())).await,
        Fetched::Failed
    );
    assert_eq!(web.get_json("not a link").await, Fetched::Failed);
}
