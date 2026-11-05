//! Drives a visible Chrome window through university sign-in.
//!
//! The user does the SSO and MFA. This module only watches the page URL
//! and the cookie jar to know when each host is signed in, then copies the
//! cookies out. It keeps a dedicated Chrome profile so "stay signed in"
//! choices survive between logins, which makes re-login quicker.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use canvas_core::{Config, Error, Result};
use chromiumoxide::Page;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::network::GetCookiesParams;
use futures::StreamExt;

use crate::{Cookie, Session};

/// Canvas sets this cookie once a user is signed in.
const CANVAS_COOKIE: &str = "canvas_session";
/// Panopto sets this cookie once a user is signed in.
const PANOPTO_COOKIE: &str = ".ASPXAUTH";
/// How often to check whether sign-in has finished.
const POLL: Duration = Duration::from_millis(750);

/// Settings for [`login`].
#[derive(Debug, Clone)]
pub struct LoginOptions {
    /// How long to wait for the user to finish each sign-in step.
    pub timeout: Duration,
    /// Chrome profile directory kept between logins.
    pub profile_dir: PathBuf,
    /// Whether to also sign in to Panopto ReCap.
    pub panopto: bool,
}

impl Default for LoginOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(300),
            profile_dir: dirs::data_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("canvas-cli/chrome-profile"),
            panopto: true,
        }
    }
}

/// Opens Chrome, waits for the user to sign in to Canvas (and Panopto),
/// and returns the captured session. It does not store it; call
/// [`crate::store`] for that.
///
/// `progress` is called with short status lines for the user.
///
/// # Errors
/// Returns [`Error::Browser`] if Chrome cannot be started or controlled,
/// or if the user does not finish signing in before the timeout.
pub async fn login(
    config: &Config,
    options: &LoginOptions,
    progress: impl Fn(&str),
) -> Result<Session> {
    let canvas = url::Url::parse(&config.canvas_url)
        .map_err(|e| Error::Browser(format!("bad canvas_url: {e}")))?;
    let canvas_host = canvas.host_str().unwrap_or_default().to_owned();

    std::fs::create_dir_all(&options.profile_dir)
        .map_err(|e| Error::Browser(format!("could not create Chrome profile dir: {e}")))?;
    let browser_config = BrowserConfig::builder()
        .with_head()
        .user_data_dir(&options.profile_dir)
        .window_size(1000, 800)
        .build()
        .map_err(Error::Browser)?;
    let (mut browser, mut handler) = Browser::launch(browser_config)
        .await
        .map_err(|e| Error::Browser(format!("could not start Chrome: {e}")))?;
    let events = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let result = async {
        let page = browser
            .new_page(
                canvas
                    .join("/login")
                    .map_err(|e| Error::Browser(e.to_string()))?
                    .as_str(),
            )
            .await
            .map_err(|e| Error::Browser(e.to_string()))?;

        progress("Sign in to Canvas in the Chrome window (SSO and MFA).");
        wait_for_cookie(
            &page,
            &config.canvas_url,
            CANVAS_COOKIE,
            options.timeout,
            |url| url.host_str() == Some(canvas_host.as_str()) && !url.path().starts_with("/login"),
        )
        .await?;
        progress("Canvas signed in.");

        let mut urls = vec![config.canvas_url.clone()];
        if options.panopto {
            let panopto = format!("https://{}/Panopto/Pages/Home.aspx", config.panopto_host);
            page.goto(panopto.as_str())
                .await
                .map_err(|e| Error::Browser(e.to_string()))?;
            progress("Sign in to ReCap if asked (use your university account).");
            wait_for_cookie(&page, &panopto, PANOPTO_COOKIE, options.timeout, |url| {
                url.host_str() == Some(config.panopto_host.as_str())
            })
            .await?;
            progress("ReCap signed in.");
            urls.push(format!("https://{}/", config.panopto_host));
        }

        let cookies = page
            .execute(GetCookiesParams::builder().urls(urls).build())
            .await
            .map_err(|e| Error::Browser(e.to_string()))?
            .result
            .cookies
            .into_iter()
            .map(|c| Cookie {
                name: c.name,
                value: c.value,
                domain: c.domain,
                path: c.path,
                expires: (!c.session && c.expires > 0.0).then_some(c.expires),
            })
            .collect();
        let captured_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        Ok(Session {
            captured_at,
            cookies,
        })
    }
    .await;

    let _ = browser.close().await;
    let _ = browser.wait().await;
    events.abort();
    result
}

/// Polls until the page is on a URL accepted by `done` and `cookie` is set
/// for `site`, or the timeout passes.
async fn wait_for_cookie(
    page: &Page,
    site: &str,
    cookie: &str,
    timeout: Duration,
    done: impl Fn(&url::Url) -> bool,
) -> Result<()> {
    let start = Instant::now();
    loop {
        let on_target = page
            .url()
            .await
            .ok()
            .flatten()
            .and_then(|u| url::Url::parse(&u).ok())
            .is_some_and(|u| done(&u));
        if on_target {
            let found = page
                .execute(
                    GetCookiesParams::builder()
                        .urls(vec![site.to_owned()])
                        .build(),
                )
                .await
                .map(|r| r.result.cookies.iter().any(|c| c.name == cookie))
                .unwrap_or(false);
            if found {
                return Ok(());
            }
        }
        if start.elapsed() > timeout {
            return Err(Error::Browser(format!(
                "timed out after {}s waiting for sign-in at {site}",
                timeout.as_secs()
            )));
        }
        tokio::time::sleep(POLL).await;
    }
}
