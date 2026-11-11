use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_tungstenite::tungstenite::Message;
use futures_util::StreamExt as _;
use serde_json::{Value, json};

use super::{origin, read_devtools_cookies, read_devtools_local_storage};
use crate::testutil::{Reply, serve};

type Script = Arc<dyn Fn(&Value) -> Vec<Value> + Send + Sync>;

/// A browser's DevTools on loopback: `/json/version` names a WebSocket whose every
/// command is recorded and answered by `script` (events first, then the reply).
async fn fake_devtools(script: Script) -> (u16, Arc<Mutex<Vec<Value>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let socket_port = listener.local_addr().unwrap().port();
    let received = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&received);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let script = Arc::clone(&script);
            let recorded = Arc::clone(&recorded);
            tokio::spawn(async move {
                let mut socket = async_tungstenite::tokio::accept_async(stream).await.expect("a WebSocket");
                while let Some(Ok(Message::Text(text))) = socket.next().await {
                    let message: Value = serde_json::from_str(&text).expect("JSON");
                    recorded.lock().unwrap().push(message.clone());
                    for answer in script(&message) {
                        socket.send(Message::text(answer.to_string())).await.unwrap();
                    }
                }
            });
        }
    });
    let (http, _) = serve(move |request| {
        if request.starts_with("get /json/version") {
            Reply {
                status: "200 OK",
                content_type: "application/json",
                headers: String::new(),
                body: json!({"webSocketDebuggerUrl": format!("ws://127.0.0.1:{socket_port}/devtools/browser/abc")}).to_string(),
                delay: std::time::Duration::ZERO,
            }
        } else {
            Reply {
                status: "404 Not Found",
                ..Reply::html("")
            }
        }
    })
    .await;
    (http.port(), received)
}

// py: tests/test_browser_login.py::test_devtools_cookies_are_read_at_the_browser_level
#[tokio::test]
async fn devtools_cookies_are_read_at_the_browser_level() {
    let jar = json!([{"name": "auth_token", "value": "v", "domain": ".x.com", "path": "/", "expires": 1_900_000_000}]);
    let answer = jar.clone();
    let (port, received) = fake_devtools(Arc::new(move |message| {
        vec![
            // An unrelated event first: the reply is matched by its id.
            json!({"method": "Target.targetCreated", "params": {}}),
            json!({"id": message["id"], "result": {"cookies": answer}}),
        ]
    }))
    .await;

    let cookies = read_devtools_cookies(port).await.expect("read");

    assert_eq!(Value::Array(cookies), jar);
    // Byte for byte what Python sends: no params, no session.
    assert_eq!(*received.lock().unwrap(), [json!({"id": 1, "method": "Storage.getCookies"})]);
}

#[tokio::test]
async fn a_refused_devtools_call_names_the_method() {
    let (port, _) = fake_devtools(Arc::new(|message| {
        vec![json!({"id": message["id"], "error": {"code": -32000, "message": "no"}})]
    }))
    .await;

    let error = read_devtools_cookies(port).await.expect_err("refused");

    assert_eq!(error.class, "LoginError");
    assert_eq!(
        error.message,
        "DevTools refused Storage.getCookies: {'code': -32000, 'message': 'no'}"
    );
}

#[tokio::test]
async fn a_browser_that_is_not_up_yet_is_an_error_to_retry() {
    // Nothing listens on the port: the poll gets an error, never a hang.
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let error = tokio::time::timeout(Duration::from_secs(10), read_devtools_cookies(port))
        .await
        .expect("no hang")
        .expect_err("nothing there");
    assert_ne!(error.class, "LoginError", "{error:?}");
}

#[tokio::test]
async fn local_storage_is_read_in_a_background_tab_that_is_closed_again() {
    let evaluations = Arc::new(Mutex::new(0));
    let counter = Arc::clone(&evaluations);
    let (port, received) = fake_devtools(Arc::new(move |message| {
        let result = match message["method"].as_str().unwrap() {
            "Target.createTarget" => json!({"targetId": "T1"}),
            "Target.attachToTarget" => json!({"sessionId": "S1"}),
            "Runtime.evaluate" => {
                let mut count = counter.lock().unwrap();
                *count += 1;
                // Not loaded yet the first time.
                if *count == 1 {
                    json!({"result": {"type": "object", "value": null}})
                } else {
                    json!({"result": {"type": "string", "value": "rt-1"}})
                }
            }
            _ => json!({}),
        };
        vec![json!({"id": message["id"], "result": result})]
    }))
    .await;

    let value = read_devtools_local_storage(port, "https://www.bilibili.com/robots.txt", "ac_time_value", Duration::from_secs(5))
        .await
        .expect("read");

    assert_eq!(value, "rt-1");
    let sent = received.lock().unwrap().clone();
    let methods: Vec<&str> = sent.iter().map(|message| message["method"].as_str().unwrap()).collect();
    assert_eq!(
        methods,
        [
            "Target.createTarget",
            "Target.attachToTarget",
            "Runtime.evaluate",
            "Runtime.evaluate",
            "Target.closeTarget"
        ]
    );
    assert_eq!(
        sent[0]["params"],
        json!({"url": "https://www.bilibili.com/robots.txt", "background": true})
    );
    assert_eq!(sent[1]["params"], json!({"targetId": "T1", "flatten": true}));
    assert_eq!(sent[2]["sessionId"], "S1");
    assert_eq!(
        sent[2]["params"]["expression"],
        "document.readyState === 'complete' && location.origin === \"https://www.bilibili.com\" ? (localStorage.getItem(\"ac_time_value\") || \"\") : null"
    );
    assert!(sent[4].get("sessionId").is_none());
    assert_eq!(
        sent.iter().map(|message| message["id"].as_i64().unwrap()).collect::<Vec<_>>(),
        [1, 2, 3, 4, 5]
    );
}

#[tokio::test]
async fn an_entry_that_never_appears_is_empty() {
    let (port, received) = fake_devtools(Arc::new(|message| {
        let result = match message["method"].as_str().unwrap() {
            "Target.createTarget" => json!({"targetId": "T1"}),
            "Target.attachToTarget" => json!({"sessionId": "S1"}),
            _ => json!({}),
        };
        vec![json!({"id": message["id"], "result": result})]
    }))
    .await;

    let value = read_devtools_local_storage(port, "https://passport.bilibili.com/robots.txt", "k", Duration::ZERO)
        .await
        .expect("read");

    assert_eq!(value, "");
    assert_eq!(received.lock().unwrap().last().unwrap()["method"], "Target.closeTarget");
}

#[test]
fn an_origin_is_scheme_and_netloc() {
    assert_eq!(origin("https://www.bilibili.com/robots.txt"), "https://www.bilibili.com");
    assert_eq!(origin("http://127.0.0.1:8080/a?b"), "http://127.0.0.1:8080");
}
