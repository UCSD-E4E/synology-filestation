//! Logging in, carrying the session id, and getting it back when DSM drops it.

use super::*;

/// Synology API error code returned when the SID has expired or is otherwise
/// not recognized by the server. DSM keeps sessions alive for ~30 minutes of
/// inactivity by default; after that any operation using the cached SID fails
/// with this code. When auto-relogin is enabled the client transparently
/// re-authenticates and retries the call.
#[allow(dead_code)] // unused by the FUSE binary today; consumed by python bindings.
pub(super) const SID_NOT_FOUND: u32 = 119;

/// Stashed credentials used by the auto-relogin path. OTP codes are
/// intentionally not stored: TOTP values are single-use, so re-login after
/// session expiry would always fail for 2FA-enabled accounts. Auto-relogin is
/// therefore only meaningful for accounts without 2FA.
#[derive(Clone)]
#[allow(dead_code)]
pub(super) struct StoredCreds {
    pub(super) user: String,
    pub(super) password: String,
}

/// Hand-written so the stashed password cannot reach a log through a stray
/// `{:?}`. A derived Debug would print it in full, and this struct exists
/// precisely to hold a password for the lifetime of the session.
impl std::fmt::Debug for StoredCreds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredCreds")
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// How the session id is carried on an authenticated request.
///
/// DSM accepts it either as a `_sid` query parameter or as an `id` cookie. They
/// are equivalent to the server and very different everywhere else: a query
/// parameter is written verbatim into the NAS's own nginx access log, into any
/// proxy's log in between, and into the `Display` of every `reqwest` transport
/// error — which this client then logs, returns across the FFI, and raises as a
/// Python exception. A cookie appears in none of those.
///
/// So the cookie is preferred, and the query parameter is the fallback for a
/// DSM that will not take it.
pub(super) const SESSION_AUTH_COOKIE: u8 = 0;
pub(super) const SESSION_AUTH_QUERY: u8 = 1;

impl SynologyClient {
    /// Build a client that transparently re-authenticates and retries once
    /// when an operation fails with `ApiError(119)` (SID expired). Use this
    /// for long-running scripts where the DSM session may outlast the ~30 min
    /// idle timeout.
    ///
    /// 2FA caveat: OTP codes are not stored, so re-login of a 2FA-enabled
    /// account after expiry will fail. Use plain [`SynologyClient::new`] for
    /// 2FA accounts and prompt for a fresh OTP at each login.
    #[allow(dead_code)] // unused by the FUSE binary today; consumed by python bindings.
    pub fn with_auto_relogin(host: &str, port: u16, https: bool) -> Self {
        let mut c = Self::new(host, port, https);
        c.auto_relogin = true;
        c
    }

    /// Attach the session id to a request in whichever way this DSM accepts.
    ///
    /// Every authenticated call goes through here rather than pushing `_sid`
    /// into its own parameter list, so there is one place that decides how the
    /// token travels — and one place to audit.
    pub(super) fn attach_session(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let sid = self.sid();
        if sid.is_empty() {
            return req;
        }
        if self.session_auth.load(std::sync::atomic::Ordering::Relaxed) == SESSION_AUTH_COOKIE {
            req.header(reqwest::header::COOKIE, format!("id={sid}"))
        } else {
            req.query(&[("_sid", sid.as_str())])
        }
    }

    /// Settle how the session id travels, once, straight after login.
    ///
    /// The cookie is a claim about somebody else's server, so it is checked
    /// rather than assumed: one cheap authenticated request in cookie mode, and
    /// if DSM answers 119 the client spends the rest of the session on the query
    /// parameter it used to use.
    ///
    /// Running this at login is the whole point of the design. A 119 is
    /// ambiguous in general — "the cookie was refused" and "the session expired"
    /// are the same code — but a session issued moments ago has not expired, so
    /// here the answer is unambiguous. Deciding it from the first ordinary call
    /// instead would misread an expired session as a rejected cookie and quietly
    /// start putting the id back into URLs.
    ///
    /// Only a definitive 119 triggers the fallback. A transport failure says
    /// nothing about whether the cookie is accepted — the network is simply
    /// down, and the next real call reports that on its own.
    pub(super) async fn probe_session_transport(&self) {
        use std::sync::atomic::Ordering;

        self.session_auth
            .store(SESSION_AUTH_COOKIE, Ordering::Relaxed);

        let url = format!("{}/entry.cgi", self.base_url);
        let req = self.attach_session(self.http.get(&url).query(&[
            ("api", "SYNO.FileStation.List"),
            ("version", "2"),
            ("method", "list_share"),
            ("limit", "1"),
            ("offset", "0"),
        ]));

        let rejected = match req.send().await {
            Ok(resp) => match resp.text().await {
                Ok(body) => serde_json::from_str::<SynoResponse<serde_json::Value>>(&body)
                    .ok()
                    .filter(|envelope| !envelope.success)
                    .and_then(|envelope| envelope.error)
                    .is_some_and(|e| e.code == SID_NOT_FOUND),
                Err(_) => false,
            },
            Err(_) => false,
        };

        if rejected {
            warn!(
                "this DSM did not accept the session cookie; falling back to the _sid \
                 query parameter. The session id will appear in the NAS's access log."
            );
            self.session_auth
                .store(SESSION_AUTH_QUERY, Ordering::Relaxed);
        } else {
            debug!("session id will travel as a cookie");
        }
    }

    /// True when the session id is being kept out of request URLs.
    pub fn session_in_cookie(&self) -> bool {
        self.session_auth.load(std::sync::atomic::Ordering::Relaxed) == SESSION_AUTH_COOKIE
    }
    pub(super) fn sid(&self) -> String {
        self.sid.read().unwrap().clone().unwrap_or_default()
    }

    /// Issue a GET request and return the response body as a string, retrying up
    /// to 3 times on transient connection errors (connection reset, read
    /// timeout, etc.). Used by every read-only API call so a momentary network
    /// blip — e.g. a VPN coming up and silently killing existing TCP
    /// connections — recovers transparently instead of bubbling up as EIO.
    pub(super) async fn get_text_retried(
        &self,
        url: &str,
        params: &[(&str, &str)],
    ) -> Result<String, SynoFsError> {
        let mut last_err = SynoFsError::Io("no attempts".into());
        for attempt in 0..3u8 {
            if attempt > 0 {
                debug!(
                    "retry {} for GET {}",
                    attempt,
                    crate::redact::redact_secrets(url)
                );
                tokio::time::sleep(Duration::from_millis(200 * attempt as u64)).await;
            }
            let resp = match self
                .attach_session(self.http.get(url).query(params))
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    last_err = e.into();
                    continue;
                }
            };
            match resp.text().await {
                Ok(t) => return Ok(t),
                Err(e) => {
                    last_err = e.into();
                }
            }
        }
        Err(last_err)
    }

    /// [`get_text_retried`](Self::get_text_retried), for a request that must
    /// not happen twice: it is sent again only when no connection was ever
    /// made, so the server cannot have seen it.
    ///
    /// Any later failure — the request went out, and the answer was lost —
    /// is returned rather than retried. For a call that starts something on
    /// the appliance, resending then starts a second one.
    pub(super) async fn get_text_once(
        &self,
        url: &str,
        params: &[(&str, &str)],
    ) -> Result<String, SynoFsError> {
        let mut attempt = 0u8;
        loop {
            match self
                .attach_session(self.http.get(url).query(params))
                .send()
                .await
            {
                Ok(resp) => return resp.text().await.map_err(Into::into),
                Err(e) if e.is_connect() && attempt < 2 => {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(200 * attempt as u64)).await;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Login and store the session ID.
    ///
    /// `otp_code` is the 6-digit TOTP code required when the account has 2-factor
    /// authentication enabled. Pass `None` if 2FA is not configured.
    ///
    /// Sent as a form-encoded **POST**. It used to be a GET with
    /// `passwd=<plaintext>` in the query string, which put the account password
    /// into DSM's own nginx access log — and into any proxy's log between here
    /// and the NAS — on every single login. Request bodies are not logged that
    /// way. DSM's `auth.cgi` accepts either verb; this is the same exchange, it
    /// just stops writing the password to disk on the way past.
    pub async fn login(
        &self,
        user: &str,
        password: &str,
        otp_code: Option<&str>,
    ) -> Result<(), SynoFsError> {
        let url = format!("{}/auth.cgi", self.base_url);
        let mut params = vec![
            ("api", "SYNO.API.Auth"),
            ("version", "7"),
            ("method", "login"),
            ("account", user),
            ("passwd", password),
            ("session", "FileStation"),
            ("format", "sid"),
        ];
        if let Some(otp) = otp_code {
            params.push(("otp_code", otp));
        }
        let resp = self
            .http
            .post(&url)
            .form(&params)
            .send()
            .await?
            .json::<SynoResponse<AuthData>>()
            .await?;

        if resp.success {
            let sid = resp
                .data
                .ok_or_else(|| SynoFsError::Io("no auth data".into()))?
                .sid;
            // Deliberately not logged, not even a prefix: the session id is a
            // bearer token, and a log line is exactly where it must not be.
            debug!("Logged in ({} byte session id)", sid.len());
            *self.sid.write().unwrap() = Some(sid);
            // Settle how the token travels before any real call uses it, while
            // the session is new enough that a 119 can only mean one thing.
            self.probe_session_transport().await;
            if self.auto_relogin {
                *self.creds.write().unwrap() = Some(StoredCreds {
                    user: user.to_string(),
                    password: password.to_string(),
                });
            }
            Ok(())
        } else {
            let code = resp.error.map(|e| e.code).unwrap_or(0);
            Err(SynoFsError::ApiError(code))
        }
    }

    /// Re-authenticate using stashed credentials. Returns `NotSupported` if
    /// auto-relogin is off or no credentials are available (e.g. 2FA login).
    #[allow(dead_code)]
    pub(super) async fn relogin(&self) -> Result<(), SynoFsError> {
        if !self.auto_relogin {
            return Err(SynoFsError::NotSupported);
        }
        let creds = self
            .creds
            .read()
            .unwrap()
            .clone()
            .ok_or(SynoFsError::NotSupported)?;
        warn!("SID expired, re-authenticating");
        self.login(&creds.user, &creds.password, None).await
    }

    /// True if this client was constructed with auto-relogin enabled.
    #[allow(dead_code)]
    pub fn auto_relogin_enabled(&self) -> bool {
        self.auto_relogin
    }

    /// Run `op` once. If it fails with `ApiError(119)` and auto-relogin is on,
    /// re-authenticate and run `op` exactly one more time. Any other error is
    /// returned untouched.
    ///
    /// If the re-login itself fails, the underlying error is wrapped in
    /// `SynoFsError::LoginFailed(...)` so callers can distinguish "the
    /// operation failed" from "we couldn't even re-authenticate to retry it."
    /// A persistent 119 (re-login succeeds but the retry still returns 119)
    /// surfaces as the second 119 untransformed.
    #[allow(dead_code)]
    pub async fn with_relogin_retry<F, Fut, T>(&self, mut op: F) -> Result<T, SynoFsError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, SynoFsError>>,
    {
        match op().await {
            Err(SynoFsError::ApiError(SID_NOT_FOUND)) if self.auto_relogin => {
                self.relogin()
                    .await
                    .map_err(|e| SynoFsError::LoginFailed(Box::new(e)))?;
                op().await
            }
            other => other,
        }
    }

    /// Logout and clear the session ID.
    pub async fn logout(&self) -> Result<(), SynoFsError> {
        let url = format!("{}/auth.cgi", self.base_url);
        let _ = self
            .attach_session(self.http.get(&url).query(&[
                ("api", "SYNO.API.Auth"),
                ("version", "7"),
                ("method", "logout"),
                ("session", "FileStation"),
            ]))
            .send()
            .await;
        *self.sid.write().unwrap() = None;
        Ok(())
    }
}
