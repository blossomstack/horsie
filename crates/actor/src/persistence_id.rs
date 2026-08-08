/// Identity of a persistent actor: its **kind** (actor type, e.g. `"workflow"` /
/// `"agent"`) plus a per-instance **id** (e.g. a run id or session id).
///
/// A [`Journal`](crate::Journal) namespaces a log by `(kind, id)` — the kind groups
/// instances of the same actor type, the id distinguishes instances — so neither
/// has to encode the other. (Compare Akka's `persistenceId`, where the type is
/// folded into one string by convention; here it is an explicit, structured key.)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PersistenceId {
    pub kind: String,
    pub id: String,
}

impl PersistenceId {
    pub fn new(kind: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            id: id.into(),
        }
    }
}

impl std::fmt::Display for PersistenceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.kind, self.id)
    }
}
