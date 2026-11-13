//! Chromium automation for fav over the DevTools Protocol.
//!
//! Python origin: Playwright (`playwright.async_api`) as `src/web/rednote_browser.py`
//! and `src/api/browser_login.py` use it, and the login window of
//! `src/api/browser_login.py`.
//!
//! - [`chromium`]: the Chromium fav starts itself, with Playwright 1.61's switches.
//! - [`driver`]: a persistent browser context and its pages over chromiumoxide, the
//!   subset of Playwright fav calls.
//! - [`devtools`]: a hand-written browser-level DevTools exchange for the plain login
//!   window, which no driver may attach to.
//! - [`proxy`], [`profile`]: proxy URLs and Chromium profile locks.
//! - [`rednote`]: what RedNote's crawler and its login share (init scripts, the
//!   signed-in probe, the macOS user agent).
//! - [`zhihu`]: the Zhihu profile, its login check and the reader that opens one post
//!   in it.
//! - [`login`], [`window`], [`vnc`]: the login window: one sign-in at a time in a
//!   Chromium shown over VNC, its cookies polled until a site accepts them.

pub mod chromium;
pub mod devtools;
pub mod driver;
pub mod login;
pub mod profile;
pub mod proxy;
pub mod rednote;
pub mod site_checks;
pub mod vnc;
pub mod window;
pub mod zhihu;

#[cfg(test)]
mod testutil;
