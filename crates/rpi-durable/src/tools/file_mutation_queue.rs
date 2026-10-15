//! Per-file mutation serialization (port of
//! `packages/durable/src/tools/file-mutation-queue.ts` @ `abe508e1b`).

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};

use crate::chord::context::Context;
use crate::env::types::ExecutionEnv;
use crate::errors::FileErrorCode;
use crate::harness::types::ToolExecuteError;

/// Tail of the mutation chain of each file, keyed by file system id and
/// canonical path.
static QUEUES: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

async fn mutation_key(
    env: &dyn ExecutionEnv,
    path: &str,
    context: &Context,
) -> Result<String, ToolExecuteError> {
    let absolute = env.absolute_path(path, context).await?;
    Ok(format!(
        "{}\0{}",
        env.id(),
        canonical(env, &absolute, context).await?
    ))
}

/// The canonical path; for a file that does not exist yet, its canonical
/// parent joined with its name, so a `write` that creates a file and a later
/// mutation of it share one key even under a symlinked directory.
fn canonical<'a>(
    env: &'a dyn ExecutionEnv,
    absolute: &'a str,
    context: &'a Context,
) -> futures::future::BoxFuture<'a, Result<String, ToolExecuteError>> {
    Box::pin(async move {
        match env.canonical_path(absolute, context).await {
            Ok(path) => Ok(path),
            Err(error) if error.code == FileErrorCode::NotSupported => Ok(absolute.to_string()),
            Err(error) if error.code == FileErrorCode::NotFound => {
                let Some(parent) = Path::new(absolute).parent() else {
                    return Ok(absolute.to_string());
                };
                let parent = parent.to_string_lossy().into_owned();
                if parent.is_empty() || parent == absolute {
                    return Ok(absolute.to_string());
                }
                let name = Path::new(absolute)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let canonical_parent = canonical(env, &parent, context).await?;
                Ok(std::path::Path::new(&canonical_parent)
                    .join(name)
                    .to_string_lossy()
                    .into_owned())
            }
            Err(error) => Err(error.into()),
        }
    })
}

/// Serialize `edit` and `write` mutations of one file within this process:
/// same file system id and canonical path, whichever environment object the
/// call got. Other files, and other file systems, never wait. Concurrent calls
/// on one file run in the order their keys resolve. Not a lock against `bash`
/// or other processes.
pub async fn with_file_mutation_queue<T, F>(
    env: Arc<dyn ExecutionEnv>,
    path: &str,
    context: &Context,
    run: F,
) -> Result<T, ToolExecuteError>
where
    F: Future<Output = Result<T, ToolExecuteError>>,
{
    let key = mutation_key(env.as_ref(), path, context).await?;
    // Take the slot without awaiting, so no other call can take it in between.
    let queue = {
        let mut queues = QUEUES.lock().expect("mutation queues");
        Arc::clone(
            queues
                .entry(key.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    };
    let guard = queue.lock().await;
    let result = run.await;
    drop(guard);
    {
        let mut queues = QUEUES.lock().expect("mutation queues");
        if let Some(current) = queues.get(&key)
            && Arc::ptr_eq(current, &queue)
            && Arc::strong_count(current) <= 2
        {
            queues.remove(&key);
        }
    }
    result
}
