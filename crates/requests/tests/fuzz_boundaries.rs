use requests_native::cookies::{CookieSnapshot, JarSnapshot};
use requests_native::retry::{
    BackoffPolicy, RetryCount, RetryPolicy, RetryReason, RetryState, StatusSet,
};
use requests_native::{
    HeaderInput, HeaderPart, ResponseDispositionState, ResponseEvent, prepare_headers, prepare_url,
};

#[test]
fn fuzz_url() {
    bolero::check!().for_each(|input: &[u8]| {
        let text = String::from_utf8_lossy(input);
        if let Ok(prepared) = prepare_url(&text, "") {
            assert_eq!(prepare_url(&prepared, ""), Ok(prepared));
        }
    });
}

#[test]
fn fuzz_headers() {
    bolero::check!().for_each(|input: &[u8]| {
        let split = input.len() / 2;
        let headers = [HeaderInput {
            name: HeaderPart::Bytes(input[..split].to_vec()),
            value: HeaderPart::Bytes(input[split..].to_vec()),
        }];
        if let Ok(prepared) = prepare_headers(&headers) {
            assert_eq!(prepared.len(), 1);
            assert_eq!(prepared[0].source_index, 0);
        }
    });
}

#[test]
fn fuzz_cookies() {
    bolero::check!().for_each(|input: &[u8]| {
        let cookies = input
            .chunks(3)
            .map(|chunk| CookieSnapshot {
                name: format!("n{}", chunk.first().copied().unwrap_or_default() % 4),
                value: Some(chunk.get(1).copied().unwrap_or_default().to_string()),
                domain: Some("example.test".to_owned()),
                path: Some("/".to_owned()),
                secure: false,
                expires: None,
                discard: true,
                rest: Vec::new(),
            })
            .collect();
        let snapshot = JarSnapshot::new(cookies);
        let mapping = snapshot.get_dict(Some("example.test"), Some("/"));
        let unique = mapping
            .iter()
            .map(|(name, _)| name)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(mapping.len(), unique.len());
    });
}

fn retry_policy(limit: u32) -> RetryPolicy {
    let count = RetryCount::Limited(limit);
    RetryPolicy {
        total: count,
        connect: count,
        read: count,
        status: count,
        redirect: count,
        other: count,
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
fn fuzz_redirect() {
    bolero::check!().for_each(|input: &[u8]| {
        let limit = (input.len().min(32) + 1) as u32;
        let mut state = RetryState::new(retry_policy(limit));
        for (index, byte) in input.iter().take(limit as usize).enumerate() {
            let status = [301, 302, 303, 307, 308][*byte as usize % 5];
            let location = format!("/{index}");
            state = state
                .increment(
                    RetryReason::Status { status },
                    "GET",
                    "http://example.test/",
                    Some(&location),
                )
                .expect("chain is bounded by the generated budget");
        }
        assert_eq!(state.history().len(), input.len().min(limit as usize));
    });
}

#[test]
fn fuzz_body_state() {
    bolero::check!().for_each(|input: &[u8]| {
        let mut state = ResponseDispositionState::default();
        for byte in input {
            let event = match byte % 10 {
                0 => ResponseEvent::Partial,
                1 => ResponseEvent::CleanEof,
                2 => ResponseEvent::Close,
                3 => ResponseEvent::Drop,
                4 => ResponseEvent::ReadError,
                5 => ResponseEvent::DecodeError,
                6 => ResponseEvent::ProtocolError,
                7 => ResponseEvent::Cancel,
                8 => ResponseEvent::ActionDisconnect,
                _ => ResponseEvent::ReplyDisconnect,
            };
            state.apply(event);
            assert!(state.decision_count() <= 1);
        }
    });
}
