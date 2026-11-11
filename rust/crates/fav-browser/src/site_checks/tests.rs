use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use fav_core::pyjson;
use fav_runtime::BoxFuture;
use serde_json::{Value, json};

use super::{BILIBILI_REFRESH_TOKEN_STORAGE_KEY, bilibili_extras, verify_rednote};
use crate::login::{LoginBrowser, LoginError};

/// The login window as the site checks see it: scripted probes, reloads and storage.
#[derive(Default)]
struct Window {
    probes: Mutex<Vec<Value>>,
    reloads: Mutex<u32>,
    storage: HashMap<(String, String), String>,
    storage_reads: Mutex<Vec<(String, String)>>,
}

impl Window {
    fn probing(probes: Vec<Value>) -> Self {
        Self {
            probes: Mutex::new(probes),
            ..Self::default()
        }
    }

    fn reloads(&self) -> u32 {
        *self.reloads.lock().unwrap()
    }
}

impl LoginBrowser for Window {
    fn closed(&self) -> BoxFuture<'_, bool> {
        Box::pin(async { false })
    }

    fn open<'a>(&'a self, _url: &'a str) -> BoxFuture<'a, Result<(), LoginError>> {
        Box::pin(async { Ok(()) })
    }

    fn cookies<'a>(&'a self, _urls: &'a [String]) -> BoxFuture<'a, Result<Vec<pyjson::Value>, LoginError>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn evaluate<'a>(&'a self, _script: &'a str) -> BoxFuture<'a, Result<Value, LoginError>> {
        Box::pin(async {
            let mut probes = self.probes.lock().unwrap();
            Ok(if probes.len() > 1 { probes.remove(0) } else { probes[0].clone() })
        })
    }

    fn reload(&self) -> BoxFuture<'_, Result<(), LoginError>> {
        Box::pin(async {
            *self.reloads.lock().unwrap() += 1;
            Ok(())
        })
    }

    fn local_storage<'a>(&'a self, url: &'a str, key: &'a str) -> BoxFuture<'a, Result<String, LoginError>> {
        Box::pin(async move {
            self.storage_reads.lock().unwrap().push((url.to_owned(), key.to_owned()));
            Ok(self.storage.get(&(url.to_owned(), key.to_owned())).cloned().unwrap_or_default())
        })
    }

    fn close(&self) -> BoxFuture<'_, Result<(), LoginError>> {
        Box::pin(async { Ok(()) })
    }
}

// py: tests/test_browser_login.py::test_the_bilibili_refresh_token_is_read_from_either_origin
#[tokio::test]
async fn the_bilibili_refresh_token_is_read_from_either_origin() {
    let mut window = Window::probing(vec![json!({})]);
    window.storage.insert(
        (
            "https://www.bilibili.com/robots.txt".to_owned(),
            BILIBILI_REFRESH_TOKEN_STORAGE_KEY.to_owned(),
        ),
        "rt-www".to_owned(),
    );

    let extras = bilibili_extras(&window, Duration::from_secs(10)).await.expect("read");

    assert_eq!(extras, [("refresh_token".to_owned(), "rt-www".to_owned())]);
    let reads = window.storage_reads.lock().unwrap().clone();
    assert_eq!(
        reads[0],
        ("https://passport.bilibili.com/robots.txt".to_owned(), "ac_time_value".to_owned())
    );
}

// py: tests/test_browser_login.py::test_a_bilibili_login_without_a_refresh_token_saves_no_extras
#[tokio::test]
async fn a_bilibili_login_without_a_refresh_token_saves_no_extras() {
    let window = Window::probing(vec![json!({})]);

    assert!(bilibili_extras(&window, Duration::ZERO).await.expect("read").is_empty());
    // Both origins were asked once before giving up.
    assert_eq!(window.storage_reads.lock().unwrap().len(), 2);
}

async fn verify(window: &Window) -> crate::login::LoginCheck {
    verify_rednote(window, &[], Duration::from_millis(50), Duration::ZERO)
        .await
        .expect("checked")
}

// py: tests/test_browser_login.py::test_rednote_is_confirmed_by_its_own_page
#[tokio::test]
async fn rednote_is_confirmed_by_its_own_page() {
    let window = Window::probing(vec![json!({}), json!({"logged_in": true, "user_id": "5ff"})]);

    let check = verify(&window).await;

    assert_eq!((check.ok, check.account.as_str(), window.reloads()), (true, "5ff", 0));
}

// py: tests/test_browser_login.py::test_rednote_mid_login_is_left_alone
#[tokio::test]
async fn rednote_mid_login_is_left_alone() {
    // The QR, the phone form or the security scan is on screen: the user is busy in it,
    // and a reload would throw their progress away.
    let window = Window::probing(vec![json!({"has_login_modal": true})]);
    let check = verify(&window).await;
    assert_eq!((check.ok, window.reloads()), (false, 0));

    let window = Window::probing(vec![json!({"has_verify_modal": true, "qr_src": "data:..."})]);
    let check = verify(&window).await;
    assert_eq!((check.ok, window.reloads()), (false, 0));

    // Each sign on its own holds the reload back.
    for probe in [json!({"has_verify_modal": true}), json!({"qr_src": "data:..."})] {
        let window = Window::probing(vec![probe]);
        let check = verify(&window).await;
        assert_eq!((check.ok, window.reloads()), (false, 0));
    }
}

// py: tests/test_browser_login.py::test_rednote_after_the_scan_is_reloaded_to_read_the_answer
#[tokio::test]
async fn rednote_after_the_scan_is_reloaded_to_read_the_answer() {
    // Both modals gone and the store still on its signed-out snapshot.
    let window = Window::probing(vec![json!({"logged_in": false, "guest": true})]);

    let check = verify(&window).await;

    assert_eq!((check.ok, window.reloads()), (false, 1));
    assert!(check.message.contains("Reloaded"), "{check:?}");
}
