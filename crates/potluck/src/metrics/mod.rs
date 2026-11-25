//! Prometheus metrics: the event registry the server records into, and the
//! OpenMetrics text rendering that adds the values read at scrape time.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use prometheus_client::collector::Collector;
use prometheus_client::encoding::{DescriptorEncoder, EncodeLabelSet, EncodeMetric, text};
use prometheus_client::metrics::MetricType;
use prometheus_client::metrics::counter::{ConstCounter, Counter};
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::ConstGauge;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::metrics::info::Info;
use prometheus_client::registry::{Registry, Unit};

use crate::api::AppState;

pub(crate) mod http;

/// Distinct refusal rule names kept as their own series; later ones become "other".
pub(crate) const MAX_RULES: usize = 128;
/// The label of a real rule named `other`; `:` is outside the rule charset, so no rule clashes.
const OTHER_RULE_LABEL: &str = "rule:other";

const HTTP_BUCKETS: [f64; 17] = [
    0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0, 30.0, 60.0,
    300.0,
];
const SLOT_WAIT_BUCKETS: [f64; 8] = [0.001, 0.01, 0.1, 0.5, 1.0, 5.0, 10.0, 30.0];

/// The scheme word of a rejected `Authorization` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthScheme {
    Basic,
    Bearer,
    Other,
}

impl AuthScheme {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Basic => "basic",
            Self::Bearer => "bearer",
            Self::Other => "other",
        }
    }
}

/// How a user tried to sign in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SignInMethod {
    Password,
    Oidc,
}

impl SignInMethod {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Password => "password",
            Self::Oidc => "oidc",
        }
    }
}

/// How a sign-in attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SignInOutcome {
    Success,
    Failure,
    RateLimitedIp,
    RateLimitedUsername,
}

impl SignInOutcome {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::RateLimitedIp => "rate_limited_ip",
            Self::RateLimitedUsername => "rate_limited_username",
        }
    }
}

/// The git smart-HTTP service a request reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GitService {
    Advertise,
    UploadPack,
    ReceivePack,
}

impl GitService {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Advertise => "advertise",
            Self::UploadPack => "upload_pack",
            Self::ReceivePack => "receive_pack",
        }
    }
}

/// How a git request through the engine ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GitOutcome {
    Ok,
    Busy,
    Overran,
    Failed,
    /// The client went away before the engine recorded any other outcome for the request.
    Cancelled,
}

impl GitOutcome {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Busy => "busy",
            Self::Overran => "overran",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Whether a push moved a ref or was refused by the hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PushOutcome {
    Accepted,
    Refused,
}

impl PushOutcome {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Refused => "refused",
        }
    }
}

// Label sets: field names are the exposed label keys, in output order.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct HttpLabels {
    http_request_method: &'static str,
    http_route: String,
    http_response_status_code: u16,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct SchemeLabels {
    scheme: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct SignInLabels {
    method: &'static str,
    outcome: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct GitLabels {
    service: &'static str,
    outcome: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct OutcomeLabels {
    outcome: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct RefusalLabels {
    code: &'static str,
    rule: String,
    kind: &'static str,
}

fn http_histogram() -> Histogram {
    Histogram::new(HTTP_BUCKETS)
}

/// The event metrics: every family the server records into as things happen.
#[derive(Debug)]
pub(crate) struct Metrics {
    registry: Registry,
    http: Family<HttpLabels, Histogram, fn() -> Histogram>,
    auth_failures: Family<SchemeLabels, Counter>,
    sign_ins: Family<SignInLabels, Counter>,
    git_requests: Family<GitLabels, Counter>,
    slot_wait: Histogram,
    pushes: Family<OutcomeLabels, Counter>,
    refusals: Family<RefusalLabels, Counter>,
    seen_rules: Mutex<HashSet<String>>,
}

impl Metrics {
    /// Builds the registry with every family and the build info for `version`.
    pub(crate) fn new(version: &str) -> Arc<Metrics> {
        let mut registry = Registry::default();
        let http = Family::<HttpLabels, Histogram, fn() -> Histogram>::new_with_constructor(
            http_histogram as fn() -> Histogram,
        );
        let auth_failures = Family::default();
        let sign_ins = Family::default();
        let git_requests = Family::default();
        let slot_wait = Histogram::new(SLOT_WAIT_BUCKETS);
        let pushes = Family::default();
        let refusals = Family::default();

        registry.register(
            "potluck_build",
            "The running Potluck version",
            Info::new(vec![("version".to_owned(), version.to_owned())]),
        );
        registry.register_with_unit(
            "http_server_request_duration",
            "Duration of HTTP server requests",
            Unit::Seconds,
            http.clone(),
        );
        registry.register(
            "potluck_auth_failures",
            "Requests answered 401 despite carrying credentials",
            auth_failures.clone(),
        );
        registry.register("potluck_sign_ins", "Sign-in attempts", sign_ins.clone());
        registry.register(
            "potluck_git_requests",
            "Git requests that reached the engine",
            git_requests.clone(),
        );
        registry.register_with_unit(
            "potluck_git_slot_wait",
            "Time spent waiting for a git slot",
            Unit::Seconds,
            slot_wait.clone(),
        );
        registry.register(
            "potluck_pushes",
            "Receive-packs with a push context",
            pushes.clone(),
        );
        registry.register(
            "potluck_push_refusals",
            "Refusal findings reported by the pre-receive hook",
            refusals.clone(),
        );

        Arc::new(Metrics {
            registry,
            http,
            auth_failures,
            sign_ins,
            git_requests,
            slot_wait,
            pushes,
            refusals,
            seen_rules: Mutex::new(HashSet::new()),
        })
    }

    /// Records one finished HTTP response under its route template.
    pub(crate) fn observe_http(&self, method: &'static str, route: &str, status: u16, secs: f64) {
        let labels = HttpLabels {
            http_request_method: method,
            http_route: route.to_owned(),
            http_response_status_code: status,
        };
        self.http.get_or_create(&labels).observe(secs);
    }

    /// Counts a 401 answered to a request that carried credentials.
    pub(crate) fn auth_failure(&self, scheme: AuthScheme) {
        let labels = SchemeLabels {
            scheme: scheme.as_str(),
        };
        self.auth_failures.get_or_create(&labels).inc();
    }

    /// Counts one sign-in attempt by method and outcome.
    pub(crate) fn sign_in(&self, method: SignInMethod, outcome: SignInOutcome) {
        let labels = SignInLabels {
            method: method.as_str(),
            outcome: outcome.as_str(),
        };
        self.sign_ins.get_or_create(&labels).inc();
    }

    /// Counts one git request by service and outcome.
    pub(crate) fn git_request(&self, service: GitService, outcome: GitOutcome) {
        let labels = GitLabels {
            service: service.as_str(),
            outcome: outcome.as_str(),
        };
        self.git_requests.get_or_create(&labels).inc();
    }

    /// Records how long one slot acquisition waited, whether or not it got a slot.
    pub(crate) fn slot_wait(&self, secs: f64) {
        self.slot_wait.observe(secs);
    }

    /// Counts one push by outcome.
    pub(crate) fn push(&self, outcome: PushOutcome) {
        let labels = OutcomeLabels {
            outcome: outcome.as_str(),
        };
        self.pushes.get_or_create(&labels).inc();
    }

    /// Counts one refusal finding.
    /// `code`, `rule`, `kind` are already validated; the 129th distinct rule becomes "other", and
    /// a rule literally named `other` records as `rule:other` so it never joins that series. The
    /// malformed-report placeholder rule records under its own label and is exempt from the cap;
    /// the report parser maps a real rule named `unknown` to `rule:unknown`, so they never clash.
    pub(crate) fn refusal(&self, code: &'static str, rule: &str, kind: &'static str) {
        // The placeholder is one fixed label, so it skips the cap and never takes a rule's slot.
        // A real rule keeps its own series once seen, so the cap only bounds new names.
        let rule = if rule == crate::git::report::MALFORMED_RULE {
            rule.to_owned()
        } else {
            let mut seen = self.seen_rules.lock().unwrap_or_else(|e| e.into_inner());
            if seen.contains(rule) || seen.len() < MAX_RULES {
                seen.insert(rule.to_owned());
                if rule == "other" {
                    OTHER_RULE_LABEL.to_owned()
                } else {
                    rule.to_owned()
                }
            } else {
                "other".to_owned()
            }
        };
        self.refusals
            .get_or_create(&RefusalLabels { code, rule, kind })
            .inc();
    }

    /// Renders the event registry, then `snapshot`, as one OpenMetrics text exposition.
    pub(crate) fn render(&self, snapshot: &Snapshot) -> String {
        let mut scrape = Registry::default();
        scrape.register_collector(Box::new(snapshot.clone()));
        let mut out = String::new();
        // Writing into a String cannot fail; an encoder error would be a bug, so log it.
        let encoded = text::encode_registry(&mut out, &self.registry)
            .and_then(|()| text::encode_registry(&mut out, &scrape))
            .and_then(|()| text::encode_eof(&mut out));
        if encoded.is_err() {
            tracing::error!("metrics: encoding the exposition failed");
        }
        out
    }
}

/// The OpenMetrics text exposition's media type.
const OPENMETRICS: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";

/// The metrics listener's routes: `GET /metrics` renders the exposition; any other path or
/// method is an empty 404, so the port reveals nothing else.
pub(crate) fn router(state: AppState) -> Router {
    Router::new()
        .route("/metrics", get(scrape))
        .fallback(not_found)
        .method_not_allowed_fallback(not_found)
        .with_state(state)
}

async fn scrape(State(state): State<AppState>) -> impl IntoResponse {
    let (slots_in_use, slots_limit) = state.git().slots();
    let (update_available, update_checks) = state.updates().map_or_else(
        || (false, UpdateCounts::default()),
        |c| (c.status().latest().is_some(), c.check_counts()),
    );
    let snapshot = Snapshot {
        sse_subscribers: state.notices().open() as u64,
        slots_in_use,
        slots_limit,
        db_queue_depth: state.db().queue_depth(),
        update_available,
        update_checks,
    };
    let body = state.metrics().render(&snapshot);
    ([(header::CONTENT_TYPE, OPENMETRICS)], body)
}

async fn not_found() -> StatusCode {
    StatusCode::NOT_FOUND
}

/// Maps a request method to its label: the nine standard methods, else `_OTHER`.
pub(crate) fn method_label(method: &axum::http::Method) -> &'static str {
    use axum::http::Method;
    match *method {
        Method::GET => "GET",
        Method::HEAD => "HEAD",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::DELETE => "DELETE",
        Method::CONNECT => "CONNECT",
        Method::OPTIONS => "OPTIONS",
        Method::TRACE => "TRACE",
        Method::PATCH => "PATCH",
        _ => "_OTHER",
    }
}

/// Values read from live server state at scrape time.
#[derive(Debug, Default, Clone)]
pub(crate) struct Snapshot {
    pub(crate) sse_subscribers: u64,
    pub(crate) slots_in_use: u64,
    pub(crate) slots_limit: u64,
    pub(crate) db_queue_depth: u64,
    pub(crate) update_available: bool,
    pub(crate) update_checks: UpdateCounts,
}

/// Update checks so far, by outcome.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct UpdateCounts {
    pub(crate) found: u64,
    pub(crate) current: u64,
    pub(crate) not_found: u64,
    pub(crate) failed: u64,
}

impl Collector for Snapshot {
    fn encode(&self, mut encoder: DescriptorEncoder) -> Result<(), std::fmt::Error> {
        let gauges = [
            (
                "potluck_sse_subscribers",
                "Open change-notice streams",
                self.sse_subscribers,
            ),
            (
                "potluck_git_slots_in_use",
                "Git slots held now",
                self.slots_in_use,
            ),
            (
                "potluck_git_slots_limit",
                "Git slots configured",
                self.slots_limit,
            ),
            (
                "potluck_db_queue_depth",
                "Database jobs waiting to run",
                self.db_queue_depth,
            ),
            (
                "potluck_update_available",
                "1 when a newer release was found",
                u64::from(self.update_available),
            ),
        ];
        for (name, help, value) in gauges {
            let metric = encoder.encode_descriptor(name, help, None, MetricType::Gauge)?;
            ConstGauge::new(value).encode(metric)?;
        }

        let c = self.update_checks;
        let mut family = encoder.encode_descriptor(
            "potluck_update_checks",
            "Update checks by outcome",
            None,
            MetricType::Counter,
        )?;
        for (outcome, value) in [
            ("found", c.found),
            ("current", c.current),
            ("not_found", c.not_found),
            ("failed", c.failed),
        ] {
            let labels = [("outcome", outcome)];
            ConstCounter::new(value).encode(family.encode_family(&labels)?)?;
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) fn sample(text: &str, series: &str) -> Option<f64> {
    text.lines()
        .find_map(|l| {
            l.strip_prefix(series)
                .and_then(|rest| rest.strip_prefix(' '))
        })
        .and_then(|v| v.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Method;

    #[test]
    fn methods_outside_the_standard_nine_are_other() {
        for (m, want) in [
            (Method::GET, "GET"),
            (Method::HEAD, "HEAD"),
            (Method::POST, "POST"),
            (Method::PUT, "PUT"),
            (Method::DELETE, "DELETE"),
            (Method::CONNECT, "CONNECT"),
            (Method::OPTIONS, "OPTIONS"),
            (Method::TRACE, "TRACE"),
            (Method::PATCH, "PATCH"),
        ] {
            assert_eq!(method_label(&m), want, "{m}");
        }
        let odd = Method::from_bytes(b"PROPFIND").unwrap();
        assert_eq!(method_label(&odd), "_OTHER", "an extension method");
    }

    #[test]
    fn a_fresh_registry_renders_build_info_and_ends_with_eof() {
        let m = Metrics::new("1.2.3");
        let text = m.render(&Snapshot::default());
        assert!(
            text.contains("potluck_build_info{version=\"1.2.3\"} 1"),
            "{text}"
        );
        assert!(text.ends_with("# EOF\n"), "{text}");
        assert_eq!(text.matches("# EOF").count(), 1, "one EOF only: {text}");
    }

    #[test]
    fn counters_render_with_total_and_their_labels() {
        let m = Metrics::new("0.1.0");
        m.sign_in(SignInMethod::Password, SignInOutcome::RateLimitedUsername);
        m.auth_failure(AuthScheme::Basic);
        m.git_request(GitService::UploadPack, GitOutcome::Busy);
        m.git_request(GitService::ReceivePack, GitOutcome::Cancelled);
        m.push(PushOutcome::Refused);
        m.refusal("secret_env_file", "env_file", "apinata");
        let text = m.render(&Snapshot::default());
        for series in [
            "potluck_sign_ins_total{method=\"password\",outcome=\"rate_limited_username\"}",
            "potluck_auth_failures_total{scheme=\"basic\"}",
            "potluck_git_requests_total{service=\"upload_pack\",outcome=\"busy\"}",
            "potluck_git_requests_total{service=\"receive_pack\",outcome=\"cancelled\"}",
            "potluck_pushes_total{outcome=\"refused\"}",
            "potluck_push_refusals_total{code=\"secret_env_file\",rule=\"env_file\",kind=\"apinata\"}",
        ] {
            assert_eq!(sample(&text, series), Some(1.0), "{series} in {text}");
        }
    }

    #[test]
    fn http_and_slot_histograms_use_the_spec_names() {
        let m = Metrics::new("0.1.0");
        m.observe_http("GET", "/api/v1/health", 200, 0.02);
        m.slot_wait(0.5);
        let text = m.render(&Snapshot::default());
        let series = "http_server_request_duration_seconds_count{http_request_method=\"GET\",http_route=\"/api/v1/health\",http_response_status_code=\"200\"}";
        assert_eq!(sample(&text, series), Some(1.0), "{text}");
        assert!(
            text.contains("le=\"300.0\""),
            "the git-transfer bucket: {text}"
        );
        assert_eq!(
            sample(&text, "potluck_git_slot_wait_seconds_count"),
            Some(1.0),
            "{text}"
        );
    }

    #[test]
    fn rule_names_past_the_cap_become_other() {
        let m = Metrics::new("0.1.0");
        for i in 0..1000 {
            m.refusal("secret_pattern", &format!("rule_{i}"), "apinata");
        }
        let text = m.render(&Snapshot::default());
        let series = text
            .lines()
            .filter(|l| l.starts_with("potluck_push_refusals_total{"))
            .count();
        assert_eq!(series, MAX_RULES + 1, "128 named rules plus other");
        let other =
            "potluck_push_refusals_total{code=\"secret_pattern\",rule=\"other\",kind=\"apinata\"}";
        assert_eq!(
            sample(&text, other),
            Some((1000 - MAX_RULES) as f64),
            "{text}"
        );
        m.refusal("secret_pattern", "rule_5", "apinata");
        let again = m.render(&Snapshot::default());
        let five =
            "potluck_push_refusals_total{code=\"secret_pattern\",rule=\"rule_5\",kind=\"apinata\"}";
        assert_eq!(
            sample(&again, five),
            Some(2.0),
            "a seen rule keeps its own series"
        );
    }

    #[test]
    fn a_rule_named_other_is_kept_apart_from_the_overflow_series() {
        let m = Metrics::new("0.1.0");
        let series = |rule: &str| {
            format!(
                "potluck_push_refusals_total{{code=\"secret_pattern\",rule=\"{rule}\",kind=\"apinata\"}}"
            )
        };
        m.refusal("secret_pattern", "other", "apinata");
        let text = m.render(&Snapshot::default());
        assert_eq!(sample(&text, &series("rule:other")), Some(1.0), "{text}");
        assert_eq!(sample(&text, &series("other")), None, "{text}");
        for i in 1..MAX_RULES {
            m.refusal("secret_pattern", &format!("rule_{i}"), "apinata");
        }
        m.refusal("secret_pattern", "one_too_many", "apinata");
        let text = m.render(&Snapshot::default());
        assert_eq!(sample(&text, &series("other")), Some(1.0), "{text}");
        assert_eq!(sample(&text, &series("rule:other")), Some(1.0), "{text}");
        m.refusal("secret_pattern", "other", "apinata");
        let text = m.render(&Snapshot::default());
        assert_eq!(sample(&text, &series("rule:other")), Some(2.0), "{text}");
        assert_eq!(sample(&text, &series("other")), Some(1.0), "{text}");
    }

    #[test]
    fn a_rule_named_unknown_and_a_malformed_report_record_apart() {
        let m = Metrics::new("0.1.0");
        for bytes in [&b"new_code unknown\n"[..], &b"garbage\n"[..]] {
            for (code, rule) in crate::git::report::parse(bytes).refusals() {
                m.refusal(code, rule, "apinata");
            }
        }
        let text = m.render(&Snapshot::default());
        let series = |rule: &str| {
            format!(
                "potluck_push_refusals_total{{code=\"unknown\",rule=\"{rule}\",kind=\"apinata\"}}"
            )
        };
        assert_eq!(sample(&text, &series("unknown")), Some(1.0), "{text}");
        assert_eq!(sample(&text, &series("rule:unknown")), Some(1.0), "{text}");
    }

    #[test]
    fn a_malformed_report_after_the_cap_keeps_its_own_series() {
        let m = Metrics::new("0.1.0");
        for i in 0..MAX_RULES {
            m.refusal("secret_pattern", &format!("rule_{i}"), "apinata");
        }
        for (code, rule) in crate::git::report::parse(b"garbage\n").refusals() {
            m.refusal(code, rule, "apinata");
        }
        let text = m.render(&Snapshot::default());
        let series = |code: &str, rule: &str| {
            format!(
                "potluck_push_refusals_total{{code=\"{code}\",rule=\"{rule}\",kind=\"apinata\"}}"
            )
        };
        assert_eq!(
            sample(&text, &series("unknown", "unknown")),
            Some(1.0),
            "{text}"
        );
        assert_eq!(sample(&text, &series("unknown", "other")), None, "{text}");
    }

    #[test]
    fn a_malformed_report_does_not_use_up_a_rule_slot() {
        let m = Metrics::new("0.1.0");
        for (code, rule) in crate::git::report::parse(b"garbage\n").refusals() {
            m.refusal(code, rule, "apinata");
        }
        for i in 0..MAX_RULES {
            m.refusal("secret_pattern", &format!("rule_{i}"), "apinata");
        }
        let text = m.render(&Snapshot::default());
        let series = |rule: &str| {
            format!(
                "potluck_push_refusals_total{{code=\"secret_pattern\",rule=\"{rule}\",kind=\"apinata\"}}"
            )
        };
        let last = format!("rule_{}", MAX_RULES - 1);
        assert_eq!(sample(&text, &series(&last)), Some(1.0), "{text}");
        assert_eq!(sample(&text, &series("other")), None, "{text}");
    }

    #[test]
    fn the_snapshot_renders_as_gauges_and_counters() {
        let m = Metrics::new("0.1.0");
        let snap = Snapshot {
            sse_subscribers: 3,
            slots_in_use: 1,
            slots_limit: 8,
            db_queue_depth: 2,
            update_available: true,
            update_checks: UpdateCounts {
                found: 1,
                current: 4,
                not_found: 0,
                failed: 2,
            },
        };
        let text = m.render(&snap);
        for (series, want) in [
            ("potluck_sse_subscribers", 3.0),
            ("potluck_git_slots_in_use", 1.0),
            ("potluck_git_slots_limit", 8.0),
            ("potluck_db_queue_depth", 2.0),
            ("potluck_update_available", 1.0),
            ("potluck_update_checks_total{outcome=\"found\"}", 1.0),
            ("potluck_update_checks_total{outcome=\"current\"}", 4.0),
            ("potluck_update_checks_total{outcome=\"not_found\"}", 0.0),
            ("potluck_update_checks_total{outcome=\"failed\"}", 2.0),
        ] {
            assert_eq!(sample(&text, series), Some(want), "{series} in {text}");
        }
        assert!(text.ends_with("# EOF\n"), "{text}");
    }
}
