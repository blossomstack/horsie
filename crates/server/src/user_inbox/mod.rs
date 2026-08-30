//! The user's inbox: what agents have addressed to the person steering them.
//!
//! Two kinds of message live here. A **notice** is something an agent said
//! while it carried on working; nothing waits on it. An **ask** is a question
//! the agent is parked on, and until that is resolved the agent is doing
//! nothing at all.
//!
//! `user_inbox`, not `inbox`: [`crate::agent_loop::inbox`] is the agent's own
//! queue of things addressed *to it*, and the two point opposite ways. One of
//! them had to say whose inbox it is.
//!
//! # Which half is derived
//!
//! The ask half is a read model. Every ask row can be rebuilt from a session
//! actor's own state, and `reconcile_session` at load is what rebuilds it. The
//! table exists because that state is spread across one journal per session and
//! no journal can be asked "which of you is waiting on me" — and because
//! deriving it would mean loading every session to read its status, which is
//! precisely what pins a session resident and defeats idle offload.
//!
//! The notice half is not derived and has no other home: a notice is a fact the
//! moment an agent speaks it, with no lifecycle to project.
//!
//! Store only, no service, for the same reason [`crate::agent_runs`] has none:
//! the session actor writes rows it derived itself and the HTTP layer reads
//! them straight back out. A service between them would be a pass-through with
//! a second error type.

mod store;

pub use store::{
    AGENT_DECLINED_ASK, AskRow, InboxFilter, InboxPage, InboxRow, InboxStateFilter, NoticeRow,
    UserInboxStore, now_ms_i64,
};
