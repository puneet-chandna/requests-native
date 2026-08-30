use requests::cookies::{CookieScalar, CookieSnapshot, JarSnapshot};
use requests::retry::{BackoffPolicy, RetryCount, RetryPolicy, RetryReason, RetryState, StatusSet};
use requests::{
    HeaderInput, HeaderPart, ResponseDecision, ResponseDisposition, ResponseDispositionState,
    ResponseEvent, prepare_headers, prepare_url,
};

fn bytes(seed: u64, length: usize) -> Vec<u8> {
    let mut state = seed;
    (0..length)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            (state >> 32) as u8
        })
        .collect()
}

#[test]
fn generated_urls_are_stable_after_preparation() {
    for seed in 0..512 {
        let input = bytes(seed, 12);
        let host = format!("h{:02x}{:02x}.example", input[0], input[1]);
        let path = input[2..8]
            .iter()
            .map(|byte| char::from(b'a' + byte % 26))
            .collect::<String>();
        let raw = format!("http://{host}/{path}?seed={seed}#fragment");
        let params = format!("extra={}", input[8]);
        let prepared = prepare_url(&raw, &params).expect("generated URL is valid");
        assert_eq!(prepare_url(&prepared, ""), Ok(prepared));
    }
}

#[test]
fn generated_headers_preserve_first_position_and_last_value() {
    for seed in 0..256 {
        let input = bytes(seed, 8);
        let first_name = format!("X-{:02X}", input[0]);
        let last_name = first_name.to_ascii_lowercase();
        let headers = [
            HeaderInput {
                name: HeaderPart::Text(first_name),
                value: HeaderPart::Text(format!("first-{}", input[1])),
            },
            HeaderInput {
                name: HeaderPart::Text(format!("Y-{:02X}", input[2])),
                value: HeaderPart::Text(format!("middle-{}", input[3])),
            },
            HeaderInput {
                name: HeaderPart::Text(last_name.clone()),
                value: HeaderPart::Text(format!("last-{}", input[4])),
            },
        ];
        let prepared = prepare_headers(&headers).expect("generated headers are valid");
        assert_eq!(prepared.len(), 2);
        assert_eq!(prepared[0].name, last_name);
        assert_eq!(prepared[0].source_index, 2);
    }
}

#[test]
fn generated_cookie_snapshots_are_ordered_last_write_wins_mappings() {
    for seed in 0..256 {
        let input = bytes(seed, 8);
        let shared = format!("cookie-{}", input[0] % 4);
        let cookies = vec![
            cookie(&shared, input[1], "a.example", "/"),
            cookie(&format!("other-{}", input[2]), input[3], "a.example", "/"),
            cookie(&shared, input[4], "a.example", "/"),
        ];
        let mapping = JarSnapshot::new(cookies).get_dict(Some("a.example"), Some("/"));
        assert_eq!(mapping.len(), 2);
        assert_eq!(mapping[0].0, shared);
        assert_eq!(mapping[0].1, Some(input[4].to_string()));
    }
}

fn cookie(name: &str, value: u8, domain: &str, path: &str) -> CookieSnapshot {
    CookieSnapshot {
        name: name.to_owned(),
        value: Some(value.to_string()),
        domain: Some(domain.to_owned()),
        path: Some(path.to_owned()),
        secure: false,
        expires: None,
        discard: true,
        rest: vec![("HttpOnly".to_owned(), CookieScalar::None)],
    }
}

fn retry_policy() -> RetryPolicy {
    RetryPolicy {
        total: RetryCount::Limited(8),
        connect: RetryCount::Limited(8),
        read: RetryCount::Limited(8),
        status: RetryCount::Limited(8),
        redirect: RetryCount::Limited(8),
        other: RetryCount::Limited(8),
        allowed_methods: None,
        status_forcelist: StatusSet::new([]),
        backoff: BackoffPolicy {
            factor: 0.0,
            maximum: None,
            jitter: 0.0,
        },
        respect_retry_after: false,
        raise_on_status: true,
        raise_on_redirect: true,
    }
}

#[test]
fn generated_redirect_chains_consume_only_the_redirect_budget() {
    for seed in 0..128 {
        let length = (seed % 7 + 1) as usize;
        let mut state = RetryState::new(retry_policy());
        for index in 0..length {
            let location = format!("/redirect/{seed}/{index}");
            state = state
                .increment(
                    RetryReason::Status { status: 302 },
                    "GET",
                    "http://example.test/start",
                    Some(&location),
                )
                .expect("generated chain fits the configured budget");
        }
        assert_eq!(state.history().len(), length);
        assert_eq!(
            state.remaining().redirect,
            RetryCount::Limited(8 - length as u32)
        );
        assert_eq!(state.remaining().status, RetryCount::Limited(8));
    }
}

#[test]
fn generated_body_boundaries_make_one_terminal_decision() {
    let terminal_events = [
        ResponseEvent::CleanEof,
        ResponseEvent::Close,
        ResponseEvent::Drop,
        ResponseEvent::ReadError,
        ResponseEvent::DecodeError,
        ResponseEvent::ProtocolError,
        ResponseEvent::Cancel,
        ResponseEvent::ActionDisconnect,
        ResponseEvent::ReplyDisconnect,
    ];
    for seed in 0..512 {
        let mut state = ResponseDispositionState::default();
        for _ in 0..seed % 5 {
            assert_eq!(
                state.apply(ResponseEvent::Partial),
                ResponseDisposition::Partial
            );
        }
        let event = terminal_events[seed as usize % terminal_events.len()];
        let terminal = state.apply(event);
        let decision = state.decision().expect("terminal event decides the lease");
        assert!(matches!(
            (terminal, decision),
            (ResponseDisposition::Reusable, ResponseDecision::Reusable)
                | (
                    ResponseDisposition::CloseDirty,
                    ResponseDecision::CloseDirty
                )
        ));
        assert_eq!(state.decision_count(), 1);
        state.apply(ResponseEvent::Drop);
        assert_eq!(state.decision_count(), 1);
    }
}
