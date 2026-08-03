#[cfg(any(feature = "sqlite", feature = "postgres"))]
use std::path::Path;
use std::sync::Arc;

use runic_hook::WriteHook;
use runic_tool::Tool;

use crate::artifacts::{self, ArtifactStore};
use crate::{MemorySessionStore, Result, SessionStore};

pub const DEFAULT_ARTIFACT_DIR: &str = ".runic/artifacts";

#[cfg(feature = "sqlite")]
const SQLITE_FILE: &str = "runic.db";
#[cfg(feature = "sqlite")]
const ARTIFACT_SUBDIR: &str = "artifacts";

type ToolFactory = Arc<dyn Fn(&Store) -> Arc<dyn Tool> + Send + Sync>;
type HookFactory = Arc<dyn Fn(&Store) -> Arc<dyn WriteHook> + Send + Sync>;

#[derive(Clone)]
pub struct Store {
    sessions: Arc<dyn SessionStore>,
    artifacts: Arc<dyn ArtifactStore>,
    tools: Vec<ToolFactory>,
    hooks: Vec<HookFactory>,
}

impl Store {
    pub fn memory() -> Result<Self> {
        tracing::info!("using in-memory store (ephemeral)");
        Ok(Self::new(
            Arc::new(MemorySessionStore::new()),
            Arc::new(artifacts::memory()?),
        ))
    }

    #[cfg(feature = "sqlite")]
    pub async fn local(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|error| crate::Error::Io(error.to_string()))?;
        let sessions = crate::SqliteSessionStore::open(dir.join(SQLITE_FILE)).await?;
        let bytes = artifacts::local(dir.join(ARTIFACT_SUBDIR).to_string_lossy())?;
        tracing::info!(dir = %dir.display(), "using local store (sqlite log, bytes on disk)");
        Ok(Self::new(Arc::new(sessions), Arc::new(bytes)))
    }

    #[cfg(feature = "postgres")]
    pub async fn postgres(database_url: &str) -> Result<Self> {
        let sessions = crate::PostgresSessionStore::connect(database_url).await?;
        let pool = sessions.pool().clone();

        let root = Path::new(DEFAULT_ARTIFACT_DIR);
        let bytes: Arc<dyn ArtifactStore> = Arc::new(artifacts::local(root.to_string_lossy())?);
        let indexed = crate::PostgresArtifactStore::from_pool(pool, bytes, "local").await?;

        tracing::info!(
            bytes_root = %root.display(),
            "using postgres store; artifact bytes on local disk — set artifacts_location to move them"
        );
        Ok(Self::new(Arc::new(sessions), Arc::new(indexed)))
    }

    fn new(sessions: Arc<dyn SessionStore>, artifacts: Arc<dyn ArtifactStore>) -> Self {
        Self {
            sessions,
            artifacts,
            tools: Vec::new(),
            hooks: Vec::new(),
        }
    }

    pub fn artifacts_location(mut self, bytes: impl ArtifactStore + 'static) -> Self {
        let bytes: Arc<dyn ArtifactStore> = Arc::new(bytes);
        self.artifacts = match self.artifacts.reindex(bytes.clone()) {
            Some(reindexed) => reindexed,
            None => bytes,
        };
        self
    }

    pub fn sessions(&self) -> Arc<dyn SessionStore> {
        self.sessions.clone()
    }

    pub fn artifacts(&self) -> Arc<dyn ArtifactStore> {
        self.artifacts.clone()
    }

    pub fn tool(mut self, tool: impl Tool + 'static) -> Self {
        let tool = Arc::new(tool) as Arc<dyn Tool>;
        self.tools.push(Arc::new(move |_| tool.clone()));
        self
    }

    pub fn tool_with<T, F>(mut self, make: F) -> Self
    where
        T: Tool + 'static,
        F: Fn(&Store) -> T + Send + Sync + 'static,
    {
        self.tools.push(Arc::new(move |store| {
            Arc::new(make(store)) as Arc<dyn Tool>
        }));
        self
    }

    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.iter().map(|make| make(self)).collect()
    }

    pub fn hook(mut self, hook: impl WriteHook + 'static) -> Self {
        let hook = Arc::new(hook) as Arc<dyn WriteHook>;
        self.hooks.push(Arc::new(move |_| hook.clone()));
        self
    }

    pub fn hook_with<H, F>(mut self, make: F) -> Self
    where
        H: WriteHook + 'static,
        F: Fn(&Store) -> H + Send + Sync + 'static,
    {
        self.hooks.push(Arc::new(move |store| {
            Arc::new(make(store)) as Arc<dyn WriteHook>
        }));
        self
    }

    pub fn hooks(&self) -> Vec<Arc<dyn WriteHook>> {
        self.hooks.iter().map(|make| make(self)).collect()
    }
}

impl From<(Arc<dyn SessionStore>, Arc<dyn ArtifactStore>)> for Store {
    fn from((sessions, artifacts): (Arc<dyn SessionStore>, Arc<dyn ArtifactStore>)) -> Self {
        Self::new(sessions, artifacts)
    }
}
