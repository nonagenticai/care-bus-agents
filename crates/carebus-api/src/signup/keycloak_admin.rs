//! Keycloak admin API of the care-bus user realm (agent `keycloak_admin.rs`, contract C5): the approve creates the
//! login itself WITHOUT an email (names by fr's split rule, born with its `carebus_signup_id` stamp), and the one email
//! goes out last via execute-actions-email with `client_id = care-bus` and the public URL. It never writes roles or a
//! tenant attribute: tenancy lives in care-bus's `users` table. Every call is fatal on failure, the lookup included:
//! "could not ask" never reads as "no such user".
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::signup::config::KeycloakAdminConfig;
use crate::signup::upstream::{ServiceToken, UpstreamError, encode};

/// The actions the set-password email asks for.
pub const INVITE_ACTIONS: [&str; 2] = ["UPDATE_PASSWORD", "VERIFY_EMAIL"];

#[derive(Deserialize)]
struct RealmUser {
    id: String,
    username: Option<String>,
    email: Option<String>,
}

pub struct ActionsEmail<'a> {
    pub actions: &'a [&'a str],
    pub client_id: &'a str,
    pub redirect_uri: &'a str,
    pub lifespan_seconds: u64,
}

pub struct KeycloakAdmin {
    api: String,
    token: ServiceToken,
    http: reqwest::Client,
}

impl KeycloakAdmin {
    pub fn new(cfg: &KeycloakAdminConfig, http: reqwest::Client) -> Self {
        Self {
            api: cfg.api_url.trim_end_matches('/').into(),
            token: ServiceToken::new(&cfg.creds, http.clone()),
            http,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.api)
    }

    async fn send(
        &self,
        req: reqwest::RequestBuilder,
        what: &str,
    ) -> Result<reqwest::Response, UpstreamError> {
        let token = self.token.get().await?;
        req.bearer_auth(token)
            .send()
            .await
            .map_err(|e| UpstreamError::transport(what, &e))
    }

    async fn read(&self, id: &str) -> Result<Option<Value>, UpstreamError> {
        let what = "keycloak read user";
        let resp = self
            .send(
                self.http.get(self.url(&format!("users/{}", encode(id)))),
                what,
            )
            .await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(UpstreamError::status(what, resp.status()));
        }
        resp.json()
            .await
            .map(Some)
            .map_err(|e| UpstreamError::transport(what, &e))
    }

    async fn write(&self, id: &str, rep: &Value, what: &str) -> Result<(), UpstreamError> {
        let resp = self
            .send(
                self.http
                    .put(self.url(&format!("users/{}", encode(id))))
                    .json(rep),
                what,
            )
            .await?;
        if !resp.status().is_success() {
            return Err(UpstreamError::status(what, resp.status()));
        }
        Ok(())
    }

    /// The id of the realm user whose email, or else username, is exactly `email` (re-checked case-insensitively:
    /// Keycloak's `exact=true` is too).
    pub async fn find_by_email(&self, email: &str) -> Result<Option<String>, UpstreamError> {
        for field in ["email", "username"] {
            let url = self.url(&format!("users?{field}={}&exact=true&max=2", encode(email)));
            let what = format!("keycloak user lookup by {field}");
            let resp = self.send(self.http.get(url), &what).await?;
            if !resp.status().is_success() {
                return Err(UpstreamError::status(&what, resp.status()));
            }
            let hits: Vec<RealmUser> = resp
                .json()
                .await
                .map_err(|e| UpstreamError::transport(&what, &e))?;
            let hit = hits.into_iter().find(|u| {
                let v = if field == "email" {
                    &u.email
                } else {
                    &u.username
                };
                v.as_deref().is_some_and(|v| v.eq_ignore_ascii_case(email))
            });
            if let Some(u) = hit {
                return Ok(Some(u.id));
            }
        }
        Ok(None)
    }

    /// Create an enabled user for `email` (no email is sent); its id from `Location`; `None` on 409.
    pub async fn create_user(
        &self,
        email: &str,
        first_name: Option<&str>,
        last_name: Option<&str>,
        attributes: &[(&str, &str)],
    ) -> Result<Option<String>, UpstreamError> {
        let mut rep =
            json!({"username": email, "email": email, "enabled": true, "emailVerified": false});
        if !attributes.is_empty() {
            let map: Map<String, Value> = attributes
                .iter()
                .map(|(k, v)| ((*k).to_string(), json!([v])))
                .collect();
            rep["attributes"] = Value::Object(map);
        }
        if let Some(n) = first_name.filter(|n| !n.is_empty()) {
            rep["firstName"] = json!(n);
        }
        if let Some(n) = last_name.filter(|n| !n.is_empty()) {
            rep["lastName"] = json!(n);
        }
        let what = "keycloak create user";
        let resp = self
            .send(self.http.post(self.url("users")).json(&rep), what)
            .await?;
        if resp.status() == StatusCode::CONFLICT {
            return Ok(None);
        }
        if resp.status() != StatusCode::CREATED {
            return Err(UpstreamError::status(what, resp.status()));
        }
        resp.headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|l| l.trim_end_matches('/').rsplit('/').next())
            .filter(|id| !id.is_empty())
            .map(|id| Some(id.to_string()))
            .ok_or_else(|| UpstreamError::new(None, "keycloak create user: no id in Location"))
    }

    /// The first value of `id`'s attribute `attr` (`None` also when the user is gone).
    pub async fn attribute(&self, id: &str, attr: &str) -> Result<Option<String>, UpstreamError> {
        Ok(self
            .read(id)
            .await?
            .and_then(|rep| rep["attributes"][attr][0].as_str().map(str::to_string)))
    }

    /// Does the user `id` exist?
    pub async fn exists(&self, id: &str) -> Result<bool, UpstreamError> {
        Ok(self.read(id).await?.is_some())
    }

    /// Merge `attrs` into the user's attributes (`None` removes one), read-modify-write of the representation so no
    /// other attribute is lost.
    pub async fn update_attributes(
        &self,
        id: &str,
        attrs: &[(&str, Option<&str>)],
    ) -> Result<(), UpstreamError> {
        let Some(mut rep) = self.read(id).await? else {
            return Err(UpstreamError::new(
                Some(404),
                "keycloak read user failed: HTTP 404",
            ));
        };
        let mut map = rep
            .get("attributes")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        for (k, v) in attrs {
            match v {
                Some(v) => {
                    map.insert((*k).to_string(), json!([v]));
                }
                None => {
                    map.remove(*k);
                }
            }
        }
        rep["attributes"] = Value::Object(map);
        self.write(id, &rep, "keycloak update user attributes")
            .await
    }

    /// Any credential or identity-provider link? `Some(false)` only when Keycloak says so positively for both;
    /// `None` when either list is refused (unknown is never "none").
    pub async fn has_credential_or_link(&self, id: &str) -> Result<Option<bool>, UpstreamError> {
        let base = self.url(&format!("users/{}", encode(id)));
        let mut unknown = false;
        for (path, what) in [
            ("credentials", "keycloak read credentials"),
            ("federated-identity", "keycloak read identity links"),
        ] {
            let r = self
                .send(self.http.get(format!("{base}/{path}")), what)
                .await?;
            if r.status() == StatusCode::FORBIDDEN {
                unknown = true;
                continue;
            }
            if !r.status().is_success() {
                return Err(UpstreamError::status(what, r.status()));
            }
            let items: Vec<Value> = r
                .json()
                .await
                .map_err(|e| UpstreamError::transport(what, &e))?;
            if !items.is_empty() {
                return Ok(Some(true));
            }
        }
        Ok(if unknown { None } else { Some(false) })
    }

    /// Delete `id`; `false` when it was already gone.
    pub async fn delete_user(&self, id: &str) -> Result<bool, UpstreamError> {
        let what = "keycloak delete user";
        let resp = self
            .send(
                self.http.delete(self.url(&format!("users/{}", encode(id)))),
                what,
            )
            .await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(false);
        }
        if !resp.status().is_success() {
            return Err(UpstreamError::status(what, resp.status()));
        }
        Ok(true)
    }

    /// Disable `id` and set `attr` = `value` in ONE read-modify-write, then read back: `true` when both persisted,
    /// `false` when only the disable did. A disable that did not persist is an error.
    pub async fn disable_and_mark(
        &self,
        id: &str,
        attr: &str,
        value: &str,
    ) -> Result<bool, UpstreamError> {
        let Some(mut rep) = self.read(id).await? else {
            return Err(UpstreamError::new(
                Some(404),
                "keycloak read user failed: HTTP 404",
            ));
        };
        let mut map = rep
            .get("attributes")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        map.insert(attr.to_string(), json!([value]));
        rep["attributes"] = Value::Object(map);
        rep["enabled"] = json!(false);
        self.write(id, &rep, "keycloak disable user").await?;
        let back = self.read(id).await?.unwrap_or(Value::Null);
        if back.get("enabled").and_then(Value::as_bool) != Some(false) {
            return Err(UpstreamError::new(
                None,
                "keycloak disable user: the disable did not persist",
            ));
        }
        Ok(back["attributes"][attr][0].as_str() == Some(value))
    }

    /// Keycloak's action email: the link lands the person in the app after the actions.
    pub async fn execute_actions_email(
        &self,
        id: &str,
        to: &ActionsEmail<'_>,
    ) -> Result<(), UpstreamError> {
        let what = "keycloak execute-actions-email";
        let url = self.url(&format!(
            "users/{}/execute-actions-email?client_id={}&redirect_uri={}&lifespan={}",
            encode(id),
            encode(to.client_id),
            encode(to.redirect_uri),
            to.lifespan_seconds,
        ));
        let resp = self
            .send(self.http.put(url).json(&to.actions), what)
            .await?;
        if !resp.status().is_success() {
            return Err(UpstreamError::status(what, resp.status()));
        }
        Ok(())
    }
}
