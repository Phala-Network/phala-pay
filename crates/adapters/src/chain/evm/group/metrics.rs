//! Bounded group health metrics; URLs, keys and raw errors never become labels.
use super::{Failure, RpcGroup};
use prometheus::{
    CounterVec, IntCounterVec, IntGaugeVec, Opts, core::Collector, proto::MetricFamily,
};
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock, PoisonError, Weak};
static GROUPS: OnceLock<Mutex<BTreeMap<String, Weak<RpcGroup>>>> = OnceLock::new();
pub(super) fn register(group: &std::sync::Arc<RpcGroup>) {
    GROUPS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(group.id.clone(), std::sync::Arc::downgrade(group));
}
/// Standard Prometheus health gauges for the current configured groups.
pub fn collect() -> Result<Vec<MetricFamily>, prometheus::Error> {
    let groups_metric = IntGaugeVec::new(
        Opts::new(
            "topup_rpc_group_eligible_members",
            "Serving validated members.",
        ),
        &["group", "chain_id"],
    )?;
    let eligible = IntGaugeVec::new(
        Opts::new("topup_rpc_member_eligible", "Member currently serving."),
        &["group", "chain_id", "member"],
    )?;
    let quarantined = IntGaugeVec::new(
        Opts::new(
            "topup_rpc_member_quarantined",
            "Redirect or credential quarantine.",
        ),
        &["group", "chain_id", "member"],
    )?;
    if let Some(groups) = GROUPS.get() {
        for group in groups
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .filter_map(Weak::upgrade)
        {
            let chain = group.chain.to_string();
            groups_metric
                .with_label_values(&[&group.id, &chain])
                .set(i64::try_from(group.serving_members()).unwrap_or(i64::MAX));
            let health = group.health.lock().unwrap_or_else(PoisonError::into_inner);
            for (m, h) in group.members.iter().zip(health.iter()) {
                let labels = [&*group.id, &*chain, &*m.id];
                eligible.with_label_values(&labels).set(i64::from(
                    h.eligible
                        && !h.quarantined
                        && h.until.is_none()
                        && !group.budgets.paused(&m.account, &m.key),
                ));
                quarantined
                    .with_label_values(&labels)
                    .set(i64::from(h.quarantined));
            }
        }
    }
    Ok(groups_metric
        .collect()
        .into_iter()
        .chain(eligible.collect())
        .chain(quarantined.collect())
        .collect())
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum EventKind {
    Failure(Failure),
    BudgetWait,
    BudgetWaitInteractive,
    HeadPerformed,
    HeadSkipped,
}

type EventKey = (String, u64, String, EventKind);
static EVENTS: Mutex<BTreeMap<EventKey, u64>> = Mutex::new(BTreeMap::new());
pub(super) fn event(group: &RpcGroup, index: usize, kind: EventKind, value: u64) {
    if let Some(member) = group.members.get(index) {
        let mut events = EVENTS.lock().unwrap_or_else(PoisonError::into_inner);
        let total = events
            .entry((group.id.clone(), group.chain, member.id.clone(), kind))
            .or_default();
        *total = total.saturating_add(value);
    }
}
/// Head validation, failure and quota-wait counters with configured labels and bounded classes.
pub fn events() -> Result<Vec<MetricFamily>, prometheus::Error> {
    let failures = IntCounterVec::new(
        Opts::new(
            "topup_rpc_member_failures_total",
            "Rejected group member attempts by bounded class.",
        ),
        &["group", "chain_id", "member", "class"],
    )?;
    let wait = CounterVec::new(
        Opts::new(
            "topup_rpc_budget_wait_seconds_total",
            "Time awaiting joint account and key admission.",
        ),
        &["group", "chain_id", "member"],
    )?;
    let heads = IntCounterVec::new(
        Opts::new(
            "topup_rpc_head_validations_total",
            "Head validations performed for RPC reads.",
        ),
        &["group", "chain_id", "member", "result"],
    )?;
    let interactive_wait = CounterVec::new(
        Opts::new(
            "topup_rpc_interactive_budget_wait_seconds_total",
            "Time awaiting joint account and key admission for interactive requests.",
        ),
        &["group", "chain_id", "member"],
    )?;
    for ((group, chain, member, kind), value) in
        EVENTS.lock().unwrap_or_else(PoisonError::into_inner).iter()
    {
        let chain = chain.to_string();
        match *kind {
            EventKind::BudgetWait => wait
                .with_label_values(&[group, &chain, member])
                .inc_by(std::time::Duration::from_nanos(*value).as_secs_f64()),
            EventKind::BudgetWaitInteractive => interactive_wait
                .with_label_values(&[group, &chain, member])
                .inc_by(std::time::Duration::from_nanos(*value).as_secs_f64()),
            EventKind::HeadPerformed => heads
                .with_label_values(&[group, &chain, member, "performed"])
                .inc_by(*value),
            EventKind::HeadSkipped => heads
                .with_label_values(&[group, &chain, member, "skipped_pinned"])
                .inc_by(*value),
            EventKind::Failure(error) => failures
                .with_label_values(&[group, &chain, member, error.code()])
                .inc_by(*value),
        }
    }
    Ok(failures
        .collect()
        .into_iter()
        .chain(wait.collect())
        .chain(interactive_wait.collect())
        .chain(heads.collect())
        .collect())
}
