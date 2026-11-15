//! Retention: ended jobs and their audio are deleted when their TTL runs out, and job
//! directories with no job row (left by a crash mid-delete) are removed.

use vc_store::files::remove_tree;

use crate::EngineContext;

/// Delete expired jobs and orphaned job directories. Returns how many jobs were removed.
pub fn sweep_expired(ctx: &EngineContext) -> usize {
    let now = ctx.clock.now_ms();
    let mut removed = 0;
    match ctx.store.expired_jobs(now) {
        Ok(ids) => {
            for id in ids {
                if let Err(e) = remove_tree(&ctx.data.job_dir(id)) {
                    tracing::error!(job_id = %id, "cannot delete expired job files: {e}");
                    continue;
                }
                match ctx.store.delete_job(id) {
                    Ok(()) => removed += 1,
                    Err(e) => tracing::error!(job_id = %id, "cannot delete expired job: {e}"),
                }
            }
        }
        Err(e) => tracing::error!("cannot list expired jobs: {e}"),
    }
    if let Ok(dirs) = ctx.data.job_dirs() {
        for id in dirs {
            let orphan = matches!(ctx.store.job_exists(id), Ok(false));
            if orphan {
                let _ = remove_tree(&ctx.data.job_dir(id));
            }
        }
    }
    if removed > 0 {
        tracing::info!(removed, "expired jobs deleted");
    }
    removed
}
