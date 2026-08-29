//! The user's inbox over HTTP.
//!
//! Reads come straight from the store — it is a read model and nothing here
//! needs to wake a session to answer a list. Writes are the interesting half,
//! because two of them are not really about the inbox at all: replying to a
//! question resumes a parked agent, and deleting one declines it. Both go
//! through the *same* supervisor commands the session page uses, so there is
//! one way to answer a question and one way to talk to an agent, however many
//! pages offer it.
//!
//! Plain handlers rather than `control::operations()`. Registering there is how
//! a resource gets a `horsie_*` tool as well as a route, and an agent that can
//! read and clear the person's inbox is not a convenience — it is an agent that
//! can answer its own questions.

use crate::http::error::Api;
use crate::http::handlers::ask;
use crate::http::{Scope, Scoped};
use crate::sessions::supervisor::SessionSupervisorCommand;
use crate::user_inbox::{
    AGENT_DECLINED_ASK, InboxFilter, InboxPage, InboxRow, InboxStateFilter, now_ms_i64,
};
use axum::Json;
use axum::extract::Query;
use axum::response::IntoResponse;
use horsie_models::inbox::{
    AskBody, InboxListResponse, InboxMessageBody, InboxMessageIds, InboxMessageView,
    InboxReplyRequest, InboxState, NoticeBody,
};
use horsie_models::session_api::Ack;
use serde::Deserialize;

/// How many messages one page may hold, whatever was asked for.
const MAX_LIMIT: usize = 500;

#[derive(Debug, Deserialize)]
pub struct ListParams {
    /// `all` (default), `open`, or `unread`.
    state: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

/// `GET /api/inbox` — everything agents have addressed to this person.
pub async fn list(
    Scope(state): Scope,
    Query(params): Query<ListParams>,
) -> Result<impl IntoResponse, Api> {
    // An unrecognised filter is a caller mistake and is refused. Falling back
    // to "everything" would answer a question nobody asked and read, to the
    // caller, as an inbox that had quietly ignored their filter.
    let want = match params.state.as_deref() {
        None | Some("all") => InboxStateFilter::All,
        Some("open") => InboxStateFilter::Open,
        Some("unread") => InboxStateFilter::Unread,
        Some(other) => {
            return Err(Api::unprocessable(format!(
                "unknown state filter '{other}': expected 'all', 'open' or 'unread'"
            )));
        }
    };
    let page = state
        .user_inbox
        .list(&InboxFilter {
            state: want,
            limit: params.limit.unwrap_or(100).min(MAX_LIMIT),
            offset: params.offset.unwrap_or(0),
        })
        .await
        .map_err(Api::internal)?;
    Ok(Json(to_wire(page)))
}

/// `POST /api/inbox/read` — note that these have been opened.
pub async fn mark_read(
    Scope(state): Scope,
    Json(req): Json<InboxMessageIds>,
) -> Result<impl IntoResponse, Api> {
    state
        .user_inbox
        .mark_read(&req.ids, now_ms_i64())
        .await
        .map_err(Api::internal)?;
    Ok(Json(Ack {}))
}

/// `POST /api/inbox/delete` — remove messages, declining any question still
/// holding an agent.
///
/// The decline is the whole reason this is not a plain delete. A question in
/// the inbox is an agent that has stopped, and dropping the row would leave it
/// stopped for ever with nothing left on screen to restart it. So the agent is
/// told first — through the ordinary answer path, so it resumes exactly as it
/// would from a real answer — and only then does the row go.
///
/// Declined before deleted, and deliberately not the other way round: if the
/// decline fails, the row is still there to try again from. The reverse order
/// would lose the only handle on a stopped agent.
pub async fn delete(
    Scope(state): Scope,
    Json(req): Json<InboxMessageIds>,
) -> Result<impl IntoResponse, Api> {
    let rows = state
        .user_inbox
        .get_many(&req.ids)
        .await
        .map_err(Api::internal)?;
    for row in rows.iter().filter(|r| r.is_ask() && r.is_open()) {
        decline(&state, row).await?;
    }
    state
        .user_inbox
        .delete(&req.ids)
        .await
        .map_err(Api::internal)?;
    Ok(Json(Ack {}))
}

/// `POST /api/inbox/:id/reply` — say something back.
///
/// One endpoint for both kinds, because from where the person is sitting there
/// is one action: they type into a box and press send. What that *means*
/// differs — an answer to a parked call, or an ordinary message to an agent
/// that is not waiting for anything — and the message's own kind decides,
/// rather than the caller restating something the server already knows.
pub async fn reply(
    Scope(state): Scope,
    Scoped(id): Scoped<String>,
    Json(req): Json<InboxReplyRequest>,
) -> Result<impl IntoResponse, Api> {
    let text = req.text.trim();
    if text.is_empty() {
        return Err(Api::unprocessable("a reply cannot be empty"));
    }
    let found = state
        .user_inbox
        .get_many(std::slice::from_ref(&id))
        .await
        .map_err(Api::internal)?;
    let Some(row) = found.into_iter().next() else {
        return Err(Api::not_found("no such message"));
    };

    match (row.is_ask(), row.is_open()) {
        (true, true) => {
            let Some(tool_call_id) = row.tool_call_id.clone() else {
                return Err(Api::unprocessable(
                    "this question predates answerable asks and can only be answered in its session",
                ));
            };
            answer(
                &state,
                &row,
                vec![crate::agent_loop::AskAnswer {
                    tool_call_id,
                    text: text.to_string(),
                }],
                InboxState::Answered,
            )
            .await?;
        }
        // A settled question is history. Sending the text anyway would deliver
        // it as an ordinary message, which is not what "reply" meant here and
        // would surprise whoever typed it — so say so instead.
        (true, false) => {
            return Err(Api::conflict(
                "already-settled",
                "this question has already been dealt with; open the session to say more",
            ));
        }
        // A notice: nothing is parked, so this is just a message to that agent.
        (false, _) => {
            ask(&state, |reply| SessionSupervisorCommand::UserMessage {
                id: row.session_id.clone(),
                agent_id: Some(row.agent_id.clone()),
                text: text.to_string(),
                reply,
            })
            .await?
            .map_err(|e| Api::conflict("not-delivered", e.to_string()))?;
            state
                .user_inbox
                .set_state(&row.id, InboxState::Answered, now_ms_i64())
                .await
                .map_err(Api::internal)?;
        }
    }
    // Replying is reading. Leaving it unread would keep it in the badge after
    // the one action that most obviously means "dealt with".
    state
        .user_inbox
        .mark_read(std::slice::from_ref(&row.id), now_ms_i64())
        .await
        .map_err(Api::internal)?;
    Ok(Json(Ack {}))
}

/// Tell the agent nobody is going to answer, so it can get on with it.
async fn decline(state: &crate::projects::ProjectServices, row: &InboxRow) -> Result<(), Api> {
    let Some(tool_call_id) = row.tool_call_id.clone() else {
        // Nothing to answer against, so nothing is actually parked on this row
        // in a way this layer can release. Deleting it is still right.
        return Ok(());
    };
    answer(
        state,
        row,
        vec![crate::agent_loop::AskAnswer {
            tool_call_id,
            text: AGENT_DECLINED_ASK.to_string(),
        }],
        InboxState::Declined,
    )
    .await
}

/// Send answers to a parked agent and record what they were.
///
/// The mark goes down whether or not the projection has already closed the row
/// — `settle_agent_asks` lets a named outcome land over a `Closed`, precisely
/// because these two writers race. Without that, a question answered while the
/// agent was quick to resume would read "closed" rather than "answered".
async fn answer(
    state: &crate::projects::ProjectServices,
    row: &InboxRow,
    answers: Vec<crate::agent_loop::AskAnswer>,
    outcome: InboxState,
) -> Result<(), Api> {
    let ids: Vec<String> = answers.iter().map(|a| a.tool_call_id.clone()).collect();
    ask(state, |reply| SessionSupervisorCommand::Answer {
        id: row.session_id.clone(),
        agent_id: Some(row.agent_id.clone()),
        answers,
        reply,
    })
    .await?
    .map_err(|e| Api::unprocessable(e.to_string()))?;
    state
        .user_inbox
        .settle_agent_asks(&row.session_id, &row.agent_id, &ids, outcome, now_ms_i64())
        .await
        .map_err(Api::internal)?;
    Ok(())
}

/// Record what an answer sent from anywhere else was, once the agent took it.
///
/// Called by `handlers::answer_asks`, which is the one door every answer comes
/// through — the session page, the inbox, and anything else that ever offers
/// to answer. Putting it there rather than in each page is what makes "answered
/// in the agent run page" show as *answered* in the inbox rather than as a row
/// that merely stopped being open.
///
/// A failure to record is not a failure to answer: the agent already has the
/// answer and is running. Warn and carry on, and the load-time reconcile puts
/// the row right.
pub async fn note_answered(
    state: &crate::projects::ProjectServices,
    session_id: &str,
    agent_id: &str,
    answered: &[String],
) {
    if let Err(e) = state
        .user_inbox
        .settle_agent_asks(
            session_id,
            agent_id,
            answered,
            InboxState::Answered,
            now_ms_i64(),
        )
        .await
    {
        tracing::warn!(error = %e, session = %session_id, "could not mark inbox asks answered");
    }
}

fn to_wire(page: InboxPage) -> InboxListResponse {
    InboxListResponse {
        messages: page.messages.into_iter().map(to_wire_message).collect(),
        unread: page.unread,
        open_asks: page.open_asks,
    }
}

/// One row on the wire.
///
/// The kind-specific half is unpacked into the union here rather than being
/// carried as a bag of nullable fields, which is the whole reason the wire type
/// is a union: a client matching on `kind` gets exactly the fields that kind
/// has, and cannot render an ask with no question.
fn to_wire_message(row: InboxRow) -> InboxMessageView {
    let body = match row.tool_call_id {
        Some(tool_call_id) if row.kind == "ask" => InboxMessageBody::Ask(AskBody {
            question: row.body,
            choices: row.choices,
            multiple: row.multiple,
            tool_call_id,
        }),
        // Anything that is not an answerable ask reads as a notice. That covers
        // a real notice and the one degenerate ask — a pre-#62 row with no call
        // id — which is genuinely something said rather than something
        // answerable, since there is nothing to send an answer to.
        Some(_) | None => InboxMessageBody::Notice(NoticeBody { body: row.body }),
    };
    InboxMessageView {
        id: row.id,
        body,
        state: row.state,
        session_id: row.session_id,
        agent_id: row.agent_id,
        title: row.title,
        created_at: u64::try_from(row.created_at).unwrap_or(0),
        read_at: row.read_at.map(|t| u64::try_from(t).unwrap_or(0)),
    }
}
