use super::*;
use axum::{Json, Router, http::StatusCode, routing::post};
use std::sync::atomic::{AtomicUsize, Ordering};
use transport::HttpReply;
fn header(number: String) -> Value {
    let mut value =
        serde_json::to_value(alloy::rpc::types::Block::<alloy::rpc::types::Transaction>::default())
            .unwrap();
    value["number"] = json!(number);
    value["hash"] = json!(format!("0x{}", "11".repeat(32)));
    value["parentHash"] = json!(format!("0x{}", "22".repeat(32)));
    value
}
fn reply(status: u16, code: i64, message: &str) -> HttpReply {
    HttpReply {
        status,
        retry_after: None,
        body: json!({"jsonrpc":"2.0","id":1,"error":{"code":code,"message":message}}),
    }
}
#[test]
fn classification_precedence_and_every_error_class() {
    let cases = [
        (302, -32602, "bad", Failure::Redirect),
        (401, -32005, "rate", Failure::Identity),
        (403, -32600, "bad", Failure::Identity),
        (400, -32602, "bad params", Failure::Request),
        (500, -32600, "bad request", Failure::Request),
        (500, 3, "execution reverted", Failure::Revert),
        (200, -32601, "unsupported", Failure::Capability),
        (413, 0, "", Failure::Body),
        (408, 0, "", Failure::Transport),
        (429, 0, "", Failure::Throttled),
        (503, 0, "", Failure::Server),
        (501, 0, "", Failure::Server),
        (599, 0, "", Failure::Server),
        (200, -32603, "internal", Failure::Server),
        (200, -32005, "quota exceeded", Failure::Throttled),
        (200, -32005, "block range too wide", Failure::Range),
        (200, -32005, "unknown limit", Failure::Unclassified),
        (200, -32000, "unknown", Failure::Unclassified),
    ];
    for (status, code, message, expected) in cases {
        assert_eq!(
            classify("eth_getLogs", &reply(status, code, message)),
            Some(expected),
            "{status} {code} {message}"
        );
    }
    assert_eq!(
        classify(
            "eth_sendRawTransaction",
            &reply(200, 3, "execution reverted")
        ),
        Some(Failure::Revert)
    );
    assert_eq!(
        classify("eth_call", &reply(200, -32005, "block range too wide")),
        Some(Failure::Unclassified)
    );
    assert_eq!(
        classify(
            "eth_call",
            &HttpReply {
                status: 200,
                retry_after: None,
                body: json!({})
            }
        ),
        Some(Failure::Malformed)
    );
}
async fn server(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (url, task)
}
#[tokio::test]
async fn http_adapter_preserves_status_retry_after_and_refuses_redirects() {
    let target_hits = Arc::new(AtomicUsize::new(0));
    let hits = target_hits.clone();
    let (target, target_task) = server(Router::new().route(
        "/",
        post(move || {
            let hits = hits.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                "{}"
            }
        }),
    ))
    .await;
    let target_url = target.clone();
    let router = Router::new()
        .route(
            "/quota",
            post(|| async {
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    [("retry-after", "7")],
                    Json(json!({"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"bad"}})),
                )
            }),
        )
        .route(
            "/redirect",
            post(move || {
                let target = target_url.clone();
                async move {
                    (
                        StatusCode::TEMPORARY_REDIRECT,
                        [("location", target)],
                        "secret must not follow",
                    )
                }
            }),
        )
        .route(
            "/auth",
            post(|| async {
                (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error":{"code":-32602}})),
                )
            }),
        )
        .route(
            "/large",
            post(|| async { "x".repeat(transport::MAX_BODY_BYTES.saturating_add(1)) }),
        );
    let (url, task) = server(router).await;
    let http = transport::client().unwrap();
    let request = json!({"jsonrpc":"2.0","id":1,"method":"eth_call","params":[]});
    let quota = transport::send(&http, &format!("{url}/quota").parse().unwrap(), &request)
        .await
        .unwrap();
    assert_eq!(quota.status, 429);
    assert_eq!(quota.retry_after, Some(Duration::from_secs(7)));
    assert_eq!(classify("eth_call", &quota), Some(Failure::Request));
    let redirect = transport::send(&http, &format!("{url}/redirect").parse().unwrap(), &request)
        .await
        .unwrap();
    assert_eq!(classify("eth_call", &redirect), Some(Failure::Redirect));
    assert_eq!(target_hits.load(Ordering::SeqCst), 0);
    let auth = transport::send(&http, &format!("{url}/auth").parse().unwrap(), &request)
        .await
        .unwrap();
    assert_eq!(classify("eth_call", &auth), Some(Failure::Identity));
    assert!(matches!(
        transport::send(&http, &format!("{url}/large").parse().unwrap(), &request).await,
        Err(Failure::Malformed)
    ));
    task.abort();
    target_task.abort();
}
fn budgets() -> Arc<Budgets> {
    Arc::new(
        Budgets::new(&BTreeMap::from([
            (
                "account".into(),
                budget::BudgetSpec {
                    requests_per_second: 10,
                    burst: 1,
                },
            ),
            (
                "key".into(),
                budget::BudgetSpec {
                    requests_per_second: 100,
                    burst: 1,
                },
            ),
        ]))
        .unwrap(),
    )
}
#[tokio::test]
async fn joint_admission_cannot_bank_account_permits_while_key_waits() {
    let budgets = budgets();
    budgets.pause("key", Duration::from_millis(150));
    let mut tasks = Vec::new();
    let start = Instant::now();
    for _ in 0..3 {
        let b = budgets.clone();
        tasks.push(tokio::spawn(async move {
            b.admit("account", "key", Instant::now() + Duration::from_secs(3))
                .await
                .unwrap();
            Instant::now()
        }));
    }
    let mut admitted = Vec::new();
    for task in tasks {
        admitted.push(task.await.unwrap());
    }
    admitted.sort();
    assert!(admitted[0].duration_since(start) >= Duration::from_millis(140));
    for pair in admitted.windows(2) {
        assert!(pair[1].duration_since(pair[0]) >= Duration::from_millis(85));
    }
}
fn group(url: &str) -> Arc<RpcGroup> {
    RpcGroup::new(
        "a".into(),
        1,
        GroupPolicy::default(),
        vec![Member {
            id: "one".into(),
            company: "company".into(),
            endpoint: Redacted::parse(url).unwrap(),
            account: "account".into(),
            key: "key".into(),
            priority: 0,
            weight: 1,
        }],
        budgets(),
    )
    .unwrap()
}
#[tokio::test]
async fn cooldown_requires_repeated_success_and_redirect_quarantine_is_permanent() {
    let group = group("http://127.0.0.1:1");
    assert_eq!(group.eligible(), 0);
    group.verified(0, true);
    assert_eq!(group.eligible(), 1);
    group.failed(0, Failure::Stale);
    assert_eq!(group.eligible(), 0);
    group.probe_result(0, true);
    assert_eq!(group.eligible(), 0);
    group.probe_result(0, false);
    group.probe_result(0, true);
    assert_eq!(group.eligible(), 0);
    group.probe_result(0, true);
    assert_eq!(group.eligible(), 1);
    group.failed(0, Failure::Redirect);
    group.probe_result(0, true);
    group.probe_result(0, true);
    assert_eq!(group.eligible(), 0);
    assert!(!group.probe_due(0));
}
#[tokio::test]
async fn serving_metrics_follow_account_pause_without_changing_selection() {
    let group = group("http://127.0.0.1:1");
    group.verified(0, true);
    assert_eq!(group.serving_members(), 1);
    group.budgets.pause("account", Duration::from_secs(1));
    assert_eq!(group.serving_members(), 0);
    assert_eq!(
        group.eligible(),
        1,
        "quota pause does not remove identity verification"
    );
    assert!(group.select(&Default::default(), None).is_err());
}

#[tokio::test]
async fn stale_head_is_not_published() {
    let number = Arc::new(AtomicUsize::new(100));
    let source = number.clone();
    let (url,task)=server(Router::new().route("/",post(move |Json(request):Json<Value>| {let source=source.clone();async move {Json(json!({"jsonrpc":"2.0","id":request["id"],"result":header(format!("0x{:x}",source.load(Ordering::SeqCst)))}))}}))).await;
    let group = group(&url);
    group.verified(0, true);
    assert_eq!(
        group
            .head(0, "latest", Instant::now() + Duration::from_secs(2))
            .await
            .unwrap()
            .number,
        100
    );
    number.store(99, Ordering::SeqCst);
    assert_eq!(
        group
            .head(0, "latest", Instant::now() + Duration::from_secs(2))
            .await,
        Err(Failure::Stale)
    );
    task.abort();
}

#[tokio::test]
async fn small_head_regression_is_tolerated_for_pinned_read_probes() {
    let number = Arc::new(AtomicUsize::new(100));
    let source = number.clone();
    let server_source = source.clone();
    let (url, task) = server(Router::new().route(
        "/",
        post(move |Json(request): Json<Value>| {
            let source = server_source.clone();
            async move {
                Json(json!({
                    "jsonrpc":"2.0",
                    "id":request["id"],
                    "result":header(format!("0x{:x}", source.load(Ordering::SeqCst)))
                }))
            }
        }),
    ))
    .await;
    let mut policy = GroupPolicy::default();
    policy.head_regression_tolerance = 2;
    let group = RpcGroup::new(
        "tolerant-head".into(),
        1,
        policy,
        vec![Member {
            id: "tolerant".into(),
            company: "company".into(),
            endpoint: Redacted::parse(&url).unwrap(),
            account: "account".into(),
            key: "key".into(),
            priority: 0,
            weight: 1,
        }],
        budgets(),
    )
    .unwrap();
    group.verified(0, true);
    let deadline = || Instant::now() + Duration::from_secs(2);
    assert_eq!(
        group.head(0, "latest", deadline()).await.unwrap().number,
        100
    );
    source.store(98, Ordering::SeqCst);
    assert_eq!(
        group.head(0, "latest", deadline()).await.unwrap().number,
        100
    );
    assert_eq!(group.eligible(), 1);
    source.store(97, Ordering::SeqCst);
    assert_eq!(
        group.head(0, "latest", deadline()).await,
        Err(Failure::Stale)
    );
    task.abort();
}

#[tokio::test]
async fn http_413_splits_topics_without_changing_the_numeric_window() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let requests = seen.clone();
    let (url,task)=server(Router::new().route("/",post(move |Json(request):Json<Value>| {let requests=requests.clone();async move {
        requests.lock().unwrap().push(request.clone());
        let topics=request.pointer("/params/0/topics/1").unwrap().as_array().unwrap();
        if topics.len()>1 {(StatusCode::PAYLOAD_TOO_LARGE,Json(json!({"error":{"code":0,"message":"body"}})))}
        else {(StatusCode::OK,Json(json!({"jsonrpc":"2.0","id":request["id"],"result":[{"topic":topics[0]}]})))}
    }}))).await;
    let group = group(&url);
    let request = json!({"jsonrpc":"2.0","id":8,"method":"eth_getLogs","params":[{"fromBlock":"0x10","toBlock":"0x20","topics":[null,["a","b"]]}]});
    let result = group
        .send_logs(0, &request, Instant::now() + Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(result["result"].as_array().unwrap().len(), 2);
    for sent in seen.lock().unwrap().iter() {
        assert_eq!(sent["params"][0]["fromBlock"], "0x10");
        assert_eq!(sent["params"][0]["toBlock"], "0x20");
    }
    task.abort();
}

#[tokio::test]
async fn shared_account_budget_applies_across_methods_and_credentials_at_real_send() {
    let sends = Arc::new(Mutex::new(Vec::new()));
    let recorded = sends.clone();
    let (url, task) = server(Router::new().route(
        "/",
        post(move |Json(request): Json<Value>| {
            let recorded = recorded.clone();
            async move {
                recorded
                    .lock()
                    .unwrap()
                    .push((Instant::now(), request["method"].clone()));
                Json(json!({"jsonrpc":"2.0","id":request["id"],"result":null}))
            }
        }),
    ))
    .await;
    let budgets = Arc::new(
        Budgets::new(&BTreeMap::from([
            (
                "account".into(),
                budget::BudgetSpec {
                    requests_per_second: 10,
                    burst: 1,
                },
            ),
            (
                "first".into(),
                budget::BudgetSpec {
                    requests_per_second: 100,
                    burst: 1,
                },
            ),
            (
                "second".into(),
                budget::BudgetSpec {
                    requests_per_second: 100,
                    burst: 1,
                },
            ),
        ]))
        .unwrap(),
    );
    let members = ["first", "second"]
        .into_iter()
        .map(|id| Member {
            id: id.into(),
            company: "company".into(),
            endpoint: Redacted::parse(&url).unwrap(),
            account: "account".into(),
            key: id.into(),
            priority: 0,
            weight: 1,
        })
        .collect();
    let group = RpcGroup::new(
        "shared-budget-test".into(),
        1,
        GroupPolicy::default(),
        members,
        budgets.clone(),
    )
    .unwrap();
    budgets.pause("first", Duration::from_millis(200));
    budgets.pause("second", Duration::from_millis(200));
    let mut tasks = Vec::new();
    for (index, method) in [
        (0, "eth_call"),
        (1, "eth_getTransactionReceipt"),
        (0, "eth_getLogs"),
        (1, "eth_call"),
    ] {
        let group = group.clone();
        tasks.push(tokio::spawn(async move {
            group
                .send(
                    index,
                    &json!({"jsonrpc":"2.0","id":1,"method":method,"params":[]}),
                    Instant::now() + Duration::from_secs(3),
                )
                .await
                .unwrap();
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let sends = sends.lock().unwrap();
    assert_eq!(sends.len(), 4);
    for pair in sends.windows(2) {
        assert!(pair[1].0.duration_since(pair[0].0) >= Duration::from_millis(80));
    }
    task.abort();
}

#[tokio::test]
async fn lagging_fast_empty_logs_are_never_requested_or_accepted() {
    use crate::chain::evm::{EvmClient, FinalizedReader, window::WindowRequest};
    let hits = Arc::new(AtomicUsize::new(0));
    let logs = hits.clone();
    let (url, task) = server(Router::new().route(
        "/",
        post(move |Json(request): Json<Value>| {
            let logs = logs.clone();
            async move {
                let result = if request["method"] == "eth_getLogs" {
                    logs.fetch_add(1, Ordering::SeqCst);
                    json!([])
                } else {
                    header("0x63".into())
                };
                Json(json!({"jsonrpc":"2.0","id":request["id"],"result":result}))
            }
        }),
    ))
    .await;
    let group = group(&url);
    group.verified(0, true);
    let reader = FinalizedReader::new(Arc::new(
        EvmClient::from_group(group.clone(), None).unwrap(),
    ));
    let request = WindowRequest {
        from: 100,
        to: 100,
        recipients: vec![alloy::primitives::Address::repeat_byte(1)],
        tokens: vec![],
        factory: None,
        finalized: true,
        exclude_member: None,
    };
    assert_eq!(
        reader.group_window(&request).await.unwrap_err(),
        crate::chain::evm::ChainError::Group(Failure::Stale)
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "fast [] from lagging member must never become evidence"
    );
    assert_eq!(group.eligible(), 0);
    task.abort();
}

#[tokio::test]
async fn complete_window_fails_over_outage_lag_and_rpc_error_to_correct_member() {
    use crate::chain::evm::{EvmClient, FinalizedReader, window::WindowRequest};
    let block: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/base-sepolia/block-47297199.json"
    ))
    .unwrap();
    let logs: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/base-sepolia/bridge-mint-logs.json"
    ))
    .unwrap();
    let receipt: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/base-sepolia/bridge-mint-receipt.json"
    ))
    .unwrap();
    let number = u64::from_str_radix(
        block["number"]
            .as_str()
            .unwrap()
            .strip_prefix("0x")
            .unwrap(),
        16,
    )
    .unwrap();
    let mut tasks = Vec::new();
    let mut members = Vec::new();
    let lagged_logs = Arc::new(AtomicUsize::new(0));
    for mode in 0..4 {
        let block = block.clone();
        let logs = logs.clone();
        let receipt = receipt.clone();
        let lagged = lagged_logs.clone();
        let (url, task) = server(Router::new().route("/",post(move |Json(request):Json<Value>| {
            let mut block=block.clone();let logs=logs.clone();let receipt=receipt.clone();let lagged=lagged.clone();
            async move {
                if mode == 0 { return (StatusCode::SERVICE_UNAVAILABLE,Json(json!({}))); }
                let method=request["method"].as_str().unwrap();
                if method == "eth_getLogs" && mode == 2 {
                    return (StatusCode::OK,Json(json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32603,"message":"internal"}})));
                }
                let result=match method {
                    "eth_getBlockByNumber"|"eth_getBlockByHash" => {
                        if mode == 1 || request["params"][0] == json!(format!("0x{:x}",number-1)) {
                            block["number"]=json!(format!("0x{:x}",number-1));
                            block["hash"]=block["parentHash"].clone();
                            block["parentHash"]=json!(format!("0x{}","22".repeat(32)));
                        }
                        block
                    },
                    "eth_getLogs" => {if mode == 1 {lagged.fetch_add(1,Ordering::SeqCst);json!([])} else {logs}},
                    "eth_getTransactionReceipt" => receipt,
                    _ => panic!("unexpected method {method}"),
                };
                (StatusCode::OK,Json(json!({"jsonrpc":"2.0","id":request["id"],"result":result})))
            }
        }))).await;
        tasks.push(task);
        members.push(Member {
            id: format!("member-{mode}"),
            company: "company".into(),
            endpoint: Redacted::parse(&url).unwrap(),
            account: "account".into(),
            key: "key".into(),
            priority: mode,
            weight: 1,
        });
    }
    let policy = GroupPolicy {
        max_attempts: 4,
        ..Default::default()
    };
    let group = RpcGroup::new(
        "window-failover-test".into(),
        84532,
        policy,
        members,
        budgets(),
    )
    .unwrap();
    for index in 0..4 {
        group.verified(index, true);
    }
    let reader = FinalizedReader::new(Arc::new(EvmClient::from_group(group, None).unwrap()));
    let request = WindowRequest {
        from: number,
        to: number,
        recipients: vec![
            "0x3c0fe91b38c2f708d360f5724208fa7ecaa6ed34"
                .parse()
                .unwrap(),
        ],
        tokens: vec![],
        factory: None,
        finalized: true,
        exclude_member: None,
    };
    let window = reader.group_window(&request).await.unwrap();
    assert_eq!(window.transfers.len(), 1);
    assert_eq!(
        window.transfers[0].amount.value(),
        alloy::primitives::U256::from(500)
    );
    assert_eq!(window.proof.unwrap().member, "member-3");
    assert_eq!(lagged_logs.load(Ordering::SeqCst), 0);
    for task in tasks {
        task.abort();
    }
}

#[tokio::test]
async fn oversized_and_non_json_http_bodies_keep_classification_metadata() {
    for (status, expected) in [
        (401, Failure::Identity),
        (403, Failure::Identity),
        (307, Failure::Redirect),
        (408, Failure::Transport),
        (413, Failure::Body),
        (429, Failure::Throttled),
        (503, Failure::Server),
    ] {
        let (url, task) = server(Router::new().route(
            "/",
            post(move || async move {
                (
                    StatusCode::from_u16(status).unwrap(),
                    [("retry-after", "9")],
                    "x".repeat(transport::MAX_BODY_BYTES + 1),
                )
            }),
        ))
        .await;
        let reply = transport::send(
            &transport::client().unwrap(),
            &url.parse().unwrap(),
            &json!({"method":"eth_call"}),
        )
        .await
        .unwrap();
        assert_eq!(reply.status, status);
        assert_eq!(reply.retry_after, Some(Duration::from_secs(9)));
        assert_eq!(classify("eth_call", &reply), Some(expected));
        task.abort();
    }
}

#[test]
fn reviewed_rules_are_scoped_and_cannot_override_terminal_causes() {
    use rules::{BudgetScope, ErrorClass, ErrorRule};
    let rule = ErrorRule {
        company: "company".into(),
        methods: vec!["eth_getLogs".into()],
        http_statuses: vec![200],
        rpc_code: -32005,
        message_prefix: "quota".into(),
        class: ErrorClass::Throttled,
        budget_scope: BudgetScope::Key,
    };
    assert!(rules::validate(std::slice::from_ref(&rule)).is_ok());
    assert!(rule.matches(
        "company",
        "eth_getLogs",
        &reply(200, -32005, "Quota exceeded")
    ));
    assert!(!rule.matches(
        "other",
        "eth_getLogs",
        &reply(200, -32005, "quota exceeded")
    ));
    assert!(!rule.matches("company", "eth_call", &reply(200, -32005, "quota exceeded")));
    assert!(rules::validate(&[rule.clone(), rule.clone()]).is_err());
    let forbidden = ErrorRule {
        rpc_code: -32602,
        ..rule
    };
    assert!(rules::validate(&[forbidden]).is_err());
    assert_eq!(
        classify(
            "eth_sendRawTransaction",
            &reply(200, -32000, "already known")
        ),
        Some(Failure::Unclassified)
    );
    assert_eq!(
        classify(
            "eth_sendRawTransaction",
            &reply(503, -32003, "execution reverted")
        ),
        Some(Failure::Revert)
    );
}

#[tokio::test]
async fn weighted_selection_never_repopulates_an_empty_pool() {
    let members = [(1, 0), (3, 1)]
        .into_iter()
        .map(|(weight, priority)| Member {
            id: format!("weighted-{priority}"),
            company: "company".into(),
            endpoint: Redacted::parse("http://127.0.0.1:1").unwrap(),
            account: "account".into(),
            key: "key".into(),
            priority,
            weight,
        })
        .collect();
    let group = RpcGroup::new(
        "weighted-test".into(),
        1,
        GroupPolicy {
            selection: Selection::WeightedRoundRobin,
            ..Default::default()
        },
        members,
        budgets(),
    )
    .unwrap();
    assert_eq!(
        group.select(&BTreeSet::new(), None),
        Err(Failure::Unavailable)
    );
    group.verified(0, true);
    group.verified(1, true);
    let mut counts = [0, 0];
    for _ in 0..8 {
        counts[group.select(&BTreeSet::new(), None).unwrap()] += 1;
    }
    assert_eq!(counts, [2, 6]);
    group.failed(0, Failure::Redirect);
    group.failed(1, Failure::Stale);
    assert_eq!(
        group.select(&BTreeSet::new(), None),
        Err(Failure::Unavailable)
    );
}

struct SafeFloor;
#[async_trait]
impl WatermarkStore for SafeFloor {
    async fn blocked(&self, _: u64) -> Result<(), Failure> {
        Ok(())
    }
    async fn freeze(&self, _: u64) -> Result<(), Failure> {
        Ok(())
    }
    async fn load(&self, _: u64, _: &str, tag: &str) -> Result<Option<HeadAnchor>, Failure> {
        Ok((tag == "safe").then(|| HeadAnchor {
            number: 100,
            hash: format!("0x{}", "11".repeat(32)),
            parent_hash: format!("0x{}", "22".repeat(32)),
        }))
    }
    async fn accept(
        &self,
        _: u64,
        _: &str,
        _: &str,
        _: &str,
        _: &HeadAnchor,
    ) -> Result<(), Failure> {
        Ok(())
    }
}
#[tokio::test]
async fn recovery_probe_cannot_readmit_a_member_below_persisted_safe_head() {
    let (url,task)=server(Router::new().route("/",post(|Json(request):Json<Value>| async move {
        let number=if request["params"][0]=="safe" {99} else {100};
        Json(json!({"jsonrpc":"2.0","id":request["id"],"result":header(format!("0x{number:x}"))}))
    }))).await;
    let group = group(&url);
    group.set_store(Arc::new(SafeFloor));
    let probe = group.probe_copy().unwrap();
    for tag in ["latest", "safe", "finalized"] {
        probe
            .head(0, tag, Instant::now() + Duration::from_secs(2))
            .await
            .unwrap();
    }
    assert_eq!(group.validate_probe(0, &probe).await, Err(Failure::Stale));
    assert_eq!(group.eligible(), 0);
    task.abort();
}

#[tokio::test]
async fn unknown_rpc_limit_under_http_429_pauses_account_and_switches_account() {
    let (url, task) = server(Router::new().route("/", post(|Json(request):Json<Value>| async move {
        if request["method"]=="eth_getBlockByNumber" {
            (StatusCode::OK,[("retry-after","30")],Json(json!({"jsonrpc":"2.0","id":request["id"],"result":header("0x1".into())})))
        } else {
            (StatusCode::TOO_MANY_REQUESTS, [("retry-after","30")], Json(json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32005,"message":"unfamiliar limit"}})))
        }
    }))).await;
    let group = group(&url);
    group.verified(0, true);
    assert_eq!(
        group
            .send(
                0,
                &json!({"method":"eth_call","params":[]}),
                Instant::now() + Duration::from_secs(2)
            )
            .await,
        Err(Failure::Throttled)
    );
    assert!(group.budgets.paused("account", "key"));
    let specs = BTreeMap::from([
        (
            "account".into(),
            budget::BudgetSpec {
                requests_per_second: 100,
                burst: 100,
            },
        ),
        (
            "key".into(),
            budget::BudgetSpec {
                requests_per_second: 100,
                burst: 100,
            },
        ),
        (
            "other-account".into(),
            budget::BudgetSpec {
                requests_per_second: 100,
                burst: 100,
            },
        ),
        (
            "other-key".into(),
            budget::BudgetSpec {
                requests_per_second: 100,
                burst: 100,
            },
        ),
    ]);
    let shared = Arc::new(Budgets::new(&specs).unwrap());
    let mut members = group.members.clone();
    let mut backup = members[0].clone();
    backup.id = "independent".into();
    backup.account = "other-account".into();
    backup.key = "other-key".into();
    backup.priority = 1;
    let backup_hits = Arc::new(AtomicUsize::new(0));
    let hits = backup_hits.clone();
    let (healthy_url, healthy_task) = server(Router::new().route(
        "/",
        post(move |Json(request): Json<Value>| {
            let hits = hits.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                let result = if request["method"] == "eth_getBlockByNumber" {
                    header("0x1".into())
                } else {
                    json!("0x1234")
                };
                Json(json!({"jsonrpc":"2.0","id":request["id"],"result":result}))
            }
        }),
    ))
    .await;
    backup.endpoint = Redacted::parse(&healthy_url).unwrap();
    members.push(backup);
    let switched = RpcGroup::new(
        "b".into(),
        1,
        GroupPolicy::default(),
        members,
        shared.clone(),
    )
    .unwrap();
    switched.verified(0, true);
    switched.verified(1, true);
    // Run the whole retry chain: the unknown RPC message must still honor HTTP 429,
    // retain Retry-After and retry using the independently budgeted healthy account.
    let request = json!({"jsonrpc":"2.0","id":1,"method":"eth_call","params":[{},"latest"]});
    assert_eq!(
        switched.request(request.clone()).await.unwrap()["result"],
        json!("0x1234")
    );
    assert!(shared.paused("account", "key"));
    assert!(backup_hits.load(Ordering::SeqCst) > 0);
    // A subsequent full request skips the paused first account rather than waiting 30s.
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), switched.request(request))
            .await
            .unwrap()
            .unwrap()["result"],
        json!("0x1234")
    );
    healthy_task.abort();
    assert_eq!(
        classify("eth_call", &reply(429, -32602, "invalid params")),
        Some(Failure::Request)
    );
    task.abort();
}

#[tokio::test]
async fn typed_decode_is_inside_the_attempt_and_enters_cooldown() {
    let (url, task) = server(Router::new().route(
        "/",
        post(|Json(request): Json<Value>| async move {
            let result = if request["method"] == "eth_getBlockByNumber" {
                header("0x1".into())
            } else {
                json!({})
            };
            Json(json!({"jsonrpc":"2.0","id":request["id"],"result":result}))
        }),
    ))
    .await;
    let group = group(&url);
    group.verified(0, true);
    let error=group.request(json!({"jsonrpc":"2.0","id":1,"method":"eth_getTransactionReceipt","params":[format!("0x{}","11".repeat(32))]})).await;
    assert_eq!(error, Err(Failure::Malformed));
    assert_eq!(
        group.eligible(),
        0,
        "malformed typed replies must count against cooldown"
    );
    task.abort();
}

#[tokio::test]
async fn pinned_probe_retries_transients_but_never_deterministic_errors() {
    for (status, code, expected_attempts) in [
        (503, -32603, 3),
        (429, -32005, 3),
        (200, -32601, 1),
        (200, -32602, 1),
        (200, -32005, 1),
        (401, -32000, 1),
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let (url, task) = server(Router::new().route(
            "/",
            post(move |Json(request): Json<Value>| {
                let count = count.clone();
                async move {
                    let attempt = count.fetch_add(1, Ordering::SeqCst);
                    let transient = status == 503 || status == 429;
                    if transient && attempt == 2 {
                        return (StatusCode::OK, [("retry-after", "0")], Json(json!({"jsonrpc":"2.0","id":request["id"],"result":[]})));
                    }
                    (StatusCode::from_u16(status).unwrap(), [("retry-after", "0")], Json(json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":code,"message":"block range too wide; https://secret.example/key"}})))
                }
            }),
        )).await;
        let probe = group(&url).probe_copy().unwrap();
        let result = probe
            .send(
                0,
                &json!({"jsonrpc":"2.0","id":1,"method":"eth_getLogs","params":[]}),
                Instant::now() + Duration::from_secs(5),
            )
            .await;
        assert_eq!(result.is_ok(), status == 503 || status == 429);
        assert_eq!(calls.load(Ordering::SeqCst), expected_attempts);
        task.abort();
    }
}

#[tokio::test]
async fn probe_deadline_includes_retry_after_and_all_nested_sends() {
    for retry_after in ["2", "18446744073709551615"] {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let (url, task) = server(Router::new().route("/", post(move || {
        let count = count.clone();
        async move {
            count.fetch_add(1, Ordering::SeqCst);
            (StatusCode::TOO_MANY_REQUESTS, [("retry-after", retry_after)], Json(json!({"jsonrpc":"2.0","id":1,"error":{"code":-32005,"message":"rate limit"}})))
        }
    }))).await;
        let mut policy = GroupPolicy::default();
        policy.probe.deadline = 250;
        policy.attempt_timeout_ms = 100;
        let base = group(&url);
        let probe = RpcGroup::new(
            base.id.clone(),
            base.chain,
            policy,
            base.members.clone(),
            base.budgets.clone(),
        )
        .unwrap()
        .probe_copy()
        .unwrap();
        let started = Instant::now();
        let result = probe
            .send(
                0,
                &json!({"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}),
                Instant::now() + Duration::from_secs(10),
            )
            .await;
        assert_eq!(result, Err(Failure::Deadline));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(started.elapsed() < Duration::from_secs(1));
        // Later sends cannot bypass the complete-probe deadline or its quota pause.
        assert_eq!(
            probe
                .send(
                    0,
                    &json!({"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}),
                    Instant::now() + Duration::from_secs(10)
                )
                .await,
            Err(Failure::Deadline)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        task.abort();
    }
}

#[tokio::test]
async fn probe_timeouts_retry_and_connection_errors_stop_at_attempt_bound() {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let (url, task) = server(Router::new().route(
        "/",
        post(move |Json(request): Json<Value>| {
            let count = count.clone();
            async move {
                if count.fetch_add(1, Ordering::SeqCst) < 2 {
                    sleep(Duration::from_millis(300)).await;
                }
                Json(json!({"jsonrpc":"2.0","id":request["id"],"result":"0x1"}))
            }
        }),
    ))
    .await;
    let base = group(&url);
    let policy = GroupPolicy {
        attempt_timeout_ms: 100,
        retry_delay_ms: 10,
        probe: ProbePolicy {
            attempts: 3,
            deadline: 1000,
        },
        ..Default::default()
    };
    let probe = RpcGroup::new(
        base.id.clone(),
        base.chain,
        policy.clone(),
        base.members.clone(),
        base.budgets.clone(),
    )
    .unwrap()
    .probe_copy()
    .unwrap();
    let request = json!({"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]});
    assert!(
        probe
            .send(0, &request, Instant::now() + Duration::from_secs(2))
            .await
            .is_ok()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    task.abort();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let connections = Arc::new(AtomicUsize::new(0));
    let count = connections.clone();
    let task = tokio::spawn(async move {
        for _ in 0..3 {
            let (socket, _) = listener.accept().await.unwrap();
            count.fetch_add(1, Ordering::SeqCst);
            drop(socket);
        }
    });
    let base = group(&url);
    let probe = RpcGroup::new(
        base.id.clone(),
        base.chain,
        policy,
        base.members.clone(),
        base.budgets.clone(),
    )
    .unwrap()
    .probe_copy()
    .unwrap();
    assert_eq!(
        probe
            .send(0, &request, Instant::now() + Duration::from_secs(2))
            .await,
        Err(Failure::Transport)
    );
    assert_eq!(connections.load(Ordering::SeqCst), 3);
    task.abort();
}

#[derive(Default)]
struct FinalizedFloor {
    frozen: std::sync::atomic::AtomicBool,
}
#[async_trait]
impl WatermarkStore for FinalizedFloor {
    async fn blocked(&self, _: u64) -> Result<(), Failure> {
        if self.frozen.load(Ordering::SeqCst) {
            Err(Failure::Fork)
        } else {
            Ok(())
        }
    }
    async fn freeze(&self, _: u64) -> Result<(), Failure> {
        self.frozen.store(true, Ordering::SeqCst);
        Ok(())
    }
    async fn load(&self, _: u64, _: &str, tag: &str) -> Result<Option<HeadAnchor>, Failure> {
        Ok(matches!(tag, "finalized" | "cursor")
            .then(|| HeadAnchor::parse(&header("0x64".into())).unwrap()))
    }
    async fn accept(
        &self,
        _: u64,
        _: &str,
        _: &str,
        _: &str,
        _: &HeadAnchor,
    ) -> Result<(), Failure> {
        Ok(())
    }
}

#[tokio::test]
async fn probe_rejects_snapshot_conflicting_with_same_height_persisted_anchor() {
    let numeric = Arc::new(AtomicUsize::new(0));
    let reads = numeric.clone();
    let (url, task) = server(Router::new().route(
        "/",
        post(move |Json(request): Json<Value>| {
            let reads = reads.clone();
            async move {
                let mut value = header("0x64".into());
                if request["params"][0] == "finalized" {
                    value["hash"] = json!(format!("0x{}", "33".repeat(32)));
                } else if request["params"][0] == "0x64" {
                    reads.fetch_add(1, Ordering::SeqCst);
                } else {
                    value["number"] = json!("0xc8");
                }
                Json(json!({"jsonrpc":"2.0","id":request["id"],"result":value}))
            }
        }),
    ))
    .await;
    let group = group(&url);
    let store = Arc::new(FinalizedFloor::default());
    group.set_store(store.clone());
    let probe = group.probe_copy().unwrap();
    for tag in ["latest", "safe", "finalized"] {
        probe
            .head(0, tag, Instant::now() + Duration::from_secs(2))
            .await
            .unwrap();
    }
    assert_eq!(group.validate_probe(0, &probe).await, Err(Failure::Fork));
    assert!(store.frozen.load(Ordering::SeqCst));
    assert_eq!(
        numeric.load(Ordering::SeqCst),
        0,
        "a good numeric answer must not override the conflicting snapshot"
    );
    assert_eq!(group.eligible(), 0);
    task.abort();
}

#[tokio::test]
async fn probe_rejects_later_numeric_hash_conflicting_with_snapshot_without_retry() {
    let numeric = Arc::new(AtomicUsize::new(0));
    let reads = numeric.clone();
    let (url, task) = server(Router::new().route(
        "/",
        post(move |Json(request): Json<Value>| {
            let reads = reads.clone();
            async move {
                let mut value = header("0x64".into());
                if request["params"][0] == "0x64" {
                    reads.fetch_add(1, Ordering::SeqCst);
                    value["hash"] = json!(format!("0x{}", "33".repeat(32)));
                }
                Json(json!({"jsonrpc":"2.0","id":request["id"],"result":value}))
            }
        }),
    ))
    .await;
    let group = group(&url);
    let store = Arc::new(FinalizedFloor::default());
    group.set_store(store.clone());
    let probe = group.probe_copy().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    for tag in ["latest", "safe", "finalized"] {
        probe.head(0, tag, deadline).await.unwrap();
    }
    assert_eq!(group.validate_probe(0, &probe).await, Err(Failure::Fork));
    assert!(store.frozen.load(Ordering::SeqCst));
    // Serving requests must be prevented from advancing after the rejected probe.
    let other = self::group(&url);
    other.set_store(store.clone());
    other.verified(0, true);
    assert_eq!(
        other.head(0, "finalized", deadline).await,
        Err(Failure::Fork)
    );
    assert_eq!(numeric.load(Ordering::SeqCst), 1);
    task.abort();
}
