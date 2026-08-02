use std::sync::Arc;

use runic::substrate::{ArtifactStore, Blobs, SessionStore, Sessions};
use runic::transcriber::SpeechToText;
use sqlx::PgPool;

use crate::hosts::AgentRegistry;
use crate::store::{Runs, Schedules};

#[derive(Clone)]
pub struct AppState {
    pub sessions: Sessions,
    pub blobs: Blobs,
    pub pool: PgPool,
    pub runs: Runs,
    pub transcriber: Option<Arc<dyn SpeechToText>>,
    pub agents: Arc<AgentRegistry>,
    pub completions: crate::completion::Completions,
    pub events: Arc<dyn crate::stream::RunEvents>,
    pub hooks: Arc<crate::hook::HookRegistry>,
    pub schedules: Schedules,
    pub routines: Arc<crate::routines::RoutineRegistry>,
}

impl AppState {
    pub fn store(&self) -> Arc<dyn SessionStore> {
        self.sessions.store()
    }

    pub fn runs(&self) -> &Runs {
        &self.runs
    }

    pub fn schedules(&self) -> &Schedules {
        &self.schedules
    }

    pub fn artifacts(&self) -> Arc<dyn ArtifactStore> {
        self.blobs.store()
    }

    pub fn session(&self, tenant: &str, session_id: &str) -> runic::Session {
        self.scratch(tenant, session_id)
            .store(self.sessions.clone())
    }

    pub fn scratch(&self, tenant: &str, session_id: &str) -> runic::Session {
        runic::session((tenant, session_id)).artifacts(self.blobs.clone())
    }
}
