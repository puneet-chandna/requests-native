use std::collections::BTreeSet;

use requests::retry::{
    BackoffPolicy, MethodSet, RetryCount, RetryFailure, RetryPolicy, RetryReason, RetryState,
    StatusSet,
};

fn methods(values: &[&str]) -> MethodSet {
    MethodSet::new(values.iter().map(|value| (*value).to_owned()))
}

fn statuses(values: &[u16]) -> StatusSet {
    StatusSet::new(values.iter().copied())
}

fn policy() -> RetryPolicy {
    RetryPolicy {
        total: RetryCount::Limited(4),
        connect: RetryCount::Limited(2),
        read: RetryCount::Limited(2),
        status: RetryCount::Limited(2),
        redirect: RetryCount::Limited(2),
        other: RetryCount::Limited(1),
        allowed_methods: Some(methods(&["GET", "PUT"])),
        status_forcelist: statuses(&[429, 503]),
        backoff: BackoffPolicy {
            factor: 0.5,
            maximum: Some(10.0),
            jitter: 0.0,
        },
        respect_retry_after: true,
        raise_on_status: true,
        raise_on_redirect: true,
    }
}

#[test]
fn connect_read_status_and_other_use_their_own_counter_plus_total() {
    let initial = RetryState::new(policy());

    let connect = initial
        .increment(RetryReason::Connect, "GET", "http://example.test", None)
        .expect("first connect retry");
    assert_eq!(connect.remaining().total, RetryCount::Limited(3));
    assert_eq!(connect.remaining().connect, RetryCount::Limited(1));
    assert_eq!(connect.remaining().read, RetryCount::Limited(2));

    let read = connect
        .increment(RetryReason::Read, "GET", "http://example.test", None)
        .expect("first read retry");
    assert_eq!(read.remaining().total, RetryCount::Limited(2));
    assert_eq!(read.remaining().read, RetryCount::Limited(1));

    let status = read
        .increment(
            RetryReason::Status { status: 503 },
            "GET",
            "http://example.test",
            None,
        )
        .expect("first status retry");
    assert_eq!(status.remaining().total, RetryCount::Limited(1));
    assert_eq!(status.remaining().status, RetryCount::Limited(1));

    let other = status
        .increment(RetryReason::Other, "GET", "http://example.test", None)
        .expect("first other retry");
    assert_eq!(other.remaining().total, RetryCount::Limited(0));
    assert_eq!(other.remaining().other, RetryCount::Limited(0));
}

#[test]
fn exhausted_specific_counter_stops_even_when_total_remains() {
    let mut configured = policy();
    configured.total = RetryCount::Limited(9);
    configured.connect = RetryCount::Limited(0);
    let state = RetryState::new(configured);

    assert_eq!(
        state.increment(RetryReason::Connect, "GET", "http://example.test", None,),
        Err(RetryFailure::Exhausted {
            reason: RetryReason::Connect
        })
    );
}

#[test]
fn method_filter_and_retry_after_status_admission_are_explicit() {
    let state = RetryState::new(policy());

    assert!(state.is_retry("GET", 503, false));
    assert!(!state.is_retry("POST", 503, false));
    assert!(state.is_retry("PUT", 429, true));
    assert!(!state.is_retry("GET", 404, true));
}

#[test]
fn explicit_status_with_location_consumes_redirect_not_status_budget() {
    let state = RetryState::new(policy());
    let incremented = state
        .increment(
            RetryReason::Status { status: 503 },
            "GET",
            "http://example.test/original",
            Some("/elsewhere"),
        )
        .expect("status-forcelist response with Location retries original URL");

    assert_eq!(incremented.remaining().redirect, RetryCount::Limited(1));
    assert_eq!(incremented.remaining().status, RetryCount::Limited(2));
    assert_eq!(incremented.history()[0].url, "http://example.test/original");
    assert_eq!(
        incremented.history()[0].redirect_location.as_deref(),
        Some("/elsewhere")
    );
}

#[test]
fn first_retry_has_zero_backoff_and_jitter_is_added_before_the_cap() {
    let mut configured = policy();
    configured.backoff = BackoffPolicy {
        factor: 1.0,
        maximum: Some(2.5),
        jitter: 1.0,
    };
    let first = RetryState::new(configured)
        .increment(RetryReason::Connect, "GET", "http://one.test", None)
        .expect("first retry");
    assert_eq!(first.backoff(0.75), 0.0);

    let second = first
        .increment(RetryReason::Connect, "GET", "http://two.test", None)
        .expect("second retry");
    assert_eq!(second.backoff(0.75), 2.5);
}

#[test]
fn redirects_break_the_consecutive_error_backoff_sequence() {
    let first = RetryState::new(policy())
        .increment(RetryReason::Connect, "GET", "http://one.test", None)
        .expect("first retry");
    let redirected = first
        .increment(
            RetryReason::Redirect { status: 302 },
            "GET",
            "http://one.test",
            Some("/next"),
        )
        .expect("redirect retry");
    let after_redirect = redirected
        .increment(RetryReason::Connect, "GET", "http://one.test", None)
        .expect("first error after redirect");

    assert_eq!(after_redirect.backoff(0.0), 0.0);
}

#[test]
fn none_and_empty_allowed_methods_both_mean_all_methods() {
    let mut configured = policy();
    configured.allowed_methods = None;
    assert!(RetryState::new(configured.clone()).is_retry("PATCH", 503, false));

    configured.allowed_methods = Some(MethodSet::new(BTreeSet::new()));
    assert!(RetryState::new(configured).is_retry("PATCH", 503, false));
}

#[test]
fn configured_allowed_method_case_is_preserved_while_request_method_is_uppercased() {
    let mut configured = policy();
    configured.allowed_methods = Some(methods(&["get"]));
    let state = RetryState::new(configured);

    assert!(!state.allows_method("get"));
    assert!(!state.is_retry("get", 503, false));
}

#[test]
fn connect_and_other_retries_do_not_use_the_read_method_gate() {
    let mut configured = policy();
    configured.allowed_methods = Some(methods(&["GET"]));
    let state = RetryState::new(configured);

    assert!(
        state
            .increment(RetryReason::Connect, "POST", "http://x", None)
            .is_ok()
    );
    assert!(
        state
            .increment(RetryReason::Other, "POST", "http://x", None)
            .is_ok()
    );
    assert!(!state.allows_method("POST"));
}
