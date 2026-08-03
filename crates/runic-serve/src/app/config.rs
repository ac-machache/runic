use std::collections::HashMap;
use std::sync::Arc;

use runic::store::Store;
use runic::transcriber::SpeechToText;
use sqlx::PgPool;

use super::state::AppState;
use crate::hosts::{AgentRegistry, HostedAgents};
use crate::store::{Runs, Schedules};

pub struct ServeConfig {
    pub store: Store,
    pub pool: PgPool,
    pub transcriber: Option<Arc<dyn SpeechToText>>,
    pub agents: HashMap<String, HostedAgents>,
    pub identity: Option<Arc<dyn crate::auth::IdentityResolver>>,
    pub events: Option<Arc<dyn crate::stream::RunEvents>>,
    pub hooks: crate::hook::HookRegistry,
    pub routines: crate::routines::RoutineRegistry,
    pub declared: Vec<Declared>,
}

#[derive(Debug, Clone)]
pub struct Declared {
    pub name: String,
    pub cron: String,
    pub tz: String,
}

impl ServeConfig {
    pub fn new(store: Store, pool: PgPool) -> Self {
        Self {
            store,
            pool,
            transcriber: None,
            agents: HashMap::new(),
            identity: None,
            events: None,
            hooks: crate::hook::HookRegistry::default(),
            routines: crate::routines::RoutineRegistry::default(),
            declared: Vec::new(),
        }
    }

    pub fn events(mut self, events: Arc<dyn crate::stream::RunEvents>) -> Self {
        self.events = Some(events);
        self
    }

    pub fn hook(mut self, name: impl Into<String>, hook: impl crate::hook::RunHook) -> Self {
        self.hooks.insert(name, Arc::new(hook));
        self
    }

    pub fn routine(
        mut self,
        name: impl Into<String>,
        cron: impl Into<String>,
        routine: impl crate::routines::Routine,
    ) -> Self {
        let name = name.into();
        self.declared.push(Declared {
            name: name.clone(),
            cron: cron.into(),
            tz: "UTC".to_string(),
        });
        self.routines.insert(name, Arc::new(routine));
        self
    }

    pub fn routine_in(
        mut self,
        name: impl Into<String>,
        cron: impl Into<String>,
        tz: impl Into<String>,
        routine: impl crate::routines::Routine,
    ) -> Self {
        let name = name.into();
        self.declared.push(Declared {
            name: name.clone(),
            cron: cron.into(),
            tz: tz.into(),
        });
        self.routines.insert(name, Arc::new(routine));
        self
    }

    pub fn agent(mut self, name: impl Into<String>, agent: impl Into<HostedAgents>) -> Self {
        self.agents.insert(name.into(), agent.into());
        self
    }

    pub async fn def(mut self, def: impl runic::AgentDef + 'static) -> anyhow::Result<Self> {
        let name = def.name().to_string();
        let description = def.description().map(str::to_string);
        let agent = def.build_agent().await?;
        self.agents
            .insert(name, HostedAgents { agent, description });
        Ok(self)
    }

    pub fn transcriber(mut self, transcriber: Option<Arc<dyn SpeechToText>>) -> Self {
        self.transcriber = transcriber;
        self
    }

    pub fn identity(mut self, identity: Arc<dyn crate::auth::IdentityResolver>) -> Self {
        self.identity = Some(identity);
        self
    }
}

pub(crate) async fn reconcile(
    schedules: &Schedules,
    declared: &[Declared],
) -> Result<(), sqlx::Error> {
    for entry in declared {
        schedules
            .declare(&entry.name, &entry.cron, &entry.tz)
            .await?;
        tracing::info!(routine = %entry.name, cron = %entry.cron, tz = %entry.tz, "routine declared");
    }
    let names: Vec<String> = declared.iter().map(|entry| entry.name.clone()).collect();
    for retired in schedules.retire_undeclared(&names).await? {
        tracing::warn!(routine = %retired, "routine is no longer declared in code; disabled");
    }
    Ok(())
}

pub(crate) fn app_state(
    config: ServeConfig,
) -> (AppState, Option<Arc<dyn crate::auth::IdentityResolver>>) {
    let state = AppState {
        store: config.store,
        runs: Runs::new(config.pool.clone()),
        schedules: Schedules::new(config.pool.clone()),
        pool: config.pool,
        transcriber: config.transcriber,
        agents: Arc::new(AgentRegistry::new(config.agents)),
        completions: crate::completion::Completions::new(),
        events: config
            .events
            .unwrap_or_else(|| crate::stream::LocalEvents::new()),
        hooks: Arc::new(config.hooks),
        routines: Arc::new(config.routines),
    };
    (state, config.identity)
}
