//! ChatGPT device-code sign-in endpoints for `kind = "chatgpt"` providers.
//!
//! Three verbs against one provider: start a login, poll it, and sign out. The
//! browser half happens on OpenAI's site, so nothing here ever sees a ChatGPT
//! password and no callback route exists to be reached from outside.

use crate::config::chatgpt_login::{LoginError, PollOutcome};
use crate::http::Scope;
use crate::http::error::Api;
use axum::Json;
use axum::extract::Path;
use axum::http::StatusCode;
use serde::Serialize;

impl From<LoginError> for Api {
    fn from(e: LoginError) -> Self {
        match e {
            LoginError::UnknownProvider => Api::not_found("no such provider"),
            LoginError::NotChatGpt => {
                Api::unprocessable("this provider is not a ChatGPT plan provider")
            }
            LoginError::NotStarted => {
                Api::unprocessable("no ChatGPT sign-in is in progress for this provider")
            }
            LoginError::Upstream(m) => Api::internal(m),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartedLoginBody {
    /// What the operator types at `verificationUrl`.
    pub user_code: String,
    pub verification_url: String,
    /// How often to poll, per OpenAI. Polling faster earns a rate limit.
    pub interval_secs: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PollBody {
    /// `pending` | `complete`.
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusBody {
    pub signed_in: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

/// `POST /api/admin/providers/:name/chatgpt/login`
pub async fn start(
    Scope(state): Scope,
    Path(name): Path<String>,
) -> Result<Json<StartedLoginBody>, Api> {
    let started = state.chatgpt.start(&name).await?;
    Ok(Json(StartedLoginBody {
        user_code: started.user_code,
        verification_url: started.verification_url,
        interval_secs: started.interval_secs,
    }))
}

/// `POST /api/admin/providers/:name/chatgpt/poll`
pub async fn poll(Scope(state): Scope, Path(name): Path<String>) -> Result<Json<PollBody>, Api> {
    Ok(Json(match state.chatgpt.poll(&name).await? {
        PollOutcome::Pending => PollBody {
            status: "pending",
            account_id: None,
        },
        PollOutcome::Complete { account_id } => PollBody {
            status: "complete",
            account_id: Some(account_id),
        },
    }))
}

/// `GET /api/admin/providers/:name/chatgpt`
pub async fn status(
    Scope(state): Scope,
    Path(name): Path<String>,
) -> Result<Json<StatusBody>, Api> {
    let account_id = state.chatgpt.account_id(&name).await?;
    Ok(Json(StatusBody {
        signed_in: account_id.is_some(),
        account_id,
    }))
}

/// `DELETE /api/admin/providers/:name/chatgpt/login`
pub async fn sign_out(Scope(state): Scope, Path(name): Path<String>) -> Result<StatusCode, Api> {
    state.chatgpt.sign_out(&name).await?;
    Ok(StatusCode::NO_CONTENT)
}
