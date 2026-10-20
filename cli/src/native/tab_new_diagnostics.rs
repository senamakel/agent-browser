use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

static NEXT_INVOCATION: AtomicU64 = AtomicU64::new(1);
static ACTIVE_INVOCATION: AtomicU64 = AtomicU64::new(0);

pub(crate) struct InvocationGuard {
    id: u64,
    started: Instant,
}

pub(crate) fn begin() -> Option<InvocationGuard> {
    std::env::var_os("AGENT_BROWSER_TAB_NEW_DIAGNOSTICS")?;
    let id = NEXT_INVOCATION.fetch_add(1, Ordering::SeqCst);
    let started = Instant::now();
    ACTIVE_INVOCATION.store(id, Ordering::SeqCst);
    eprintln!("TAB_NEW_DIAG invocation={id} stage=begin elapsed_ms=0 outcome=started");
    Some(InvocationGuard { id, started })
}

pub(crate) fn mark(stage: &str, started: Instant, succeeded: bool) {
    let invocation = ACTIVE_INVOCATION.load(Ordering::SeqCst);
    if invocation == 0 {
        return;
    }
    let outcome = if succeeded { "ok" } else { "error" };
    eprintln!(
        "TAB_NEW_DIAG invocation={invocation} stage={stage} elapsed_ms={} outcome={outcome}",
        started.elapsed().as_millis()
    );
}

impl Drop for InvocationGuard {
    fn drop(&mut self) {
        let _ = ACTIVE_INVOCATION.compare_exchange(self.id, 0, Ordering::SeqCst, Ordering::SeqCst);
        eprintln!(
            "TAB_NEW_DIAG invocation={} stage=scope-exit elapsed_ms={} outcome=exited",
            self.id,
            self.started.elapsed().as_millis()
        );
    }
}
