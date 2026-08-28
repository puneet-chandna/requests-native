#[cfg(test)]
mod task16_session_payload_contract {
    use super::*;
    use crate::bridge::WorkerPayload;

    fn assert_worker_payload<T: WorkerPayload + Send + 'static>() {}

    trait AmbiguousIfWorkerPayload<A> {
        fn marker() {}
    }

    impl<T: ?Sized> AmbiguousIfWorkerPayload<()> for T {}
    impl<T: ?Sized + WorkerPayload> AmbiguousIfWorkerPayload<u8> for T {}

    trait AmbiguousIfSend<A> {
        fn marker() {}
    }

    impl<T: ?Sized> AmbiguousIfSend<()> for T {}
    impl<T: ?Sized + Send> AmbiguousIfSend<u8> for T {}

    struct BorrowedValue<'a>(&'a str);

    #[test]
    fn all_session_payload_variants_are_worker_payloads() {
        assert_worker_payload::<SessionAction>();
        assert_worker_payload::<SessionReply>();
        assert_worker_payload::<NativeTransfer>();
        assert_worker_payload::<CompletionAction>();
        assert_worker_payload::<CompletionReply>();
        assert_worker_payload::<CompletionEnvelope>();
    }

    #[test]
    fn every_action_variant_constructs_and_exhaustively_destructures() {
        let generation = GenerationId::checked(7).unwrap();
        let sequence = Sequence::checked(11).unwrap();
        let correlation = CorrelationId::checked(13).unwrap();
        let request_id = RequestId::checked(17, generation).unwrap();
        let response_id = ResponseId::checked(19, generation).unwrap();
        let adapter_id = AdapterId::checked(23, generation).unwrap();
        let jar_id = JarId::checked(29, generation).unwrap();
        let hook_id = HookId::checked(31, generation).unwrap();
        let auth_id = AuthId::checked(37, generation).unwrap();
        let cursor_id = CursorId::checked(39, generation).unwrap();
        let opaque_value_id = OpaqueValueId::checked(41, generation).unwrap();
        let _ = (cursor_id, opaque_value_id);

        let actions = [
            SessionAction::ReadGlobal { authority: GlobalAuthority::Sessions, generation, sequence },
            SessionAction::ReadBody { request_id, generation, sequence },
            SessionAction::SendCustomAdapter { adapter_id, request_id, generation, correlation, sequence },
            SessionAction::DispatchHook { hook_id, response_id, generation, correlation, sequence },
            SessionAction::RunAuth { auth_id, request_id, generation, sequence },
            SessionAction::ExtractCookies { jar_id, request_id, response_id, generation, sequence },
            SessionAction::NestedSubmit { request_id, generation, parent_correlation: correlation, correlation, sequence },
        ];

        for action in actions {
            match action {
                SessionAction::ReadGlobal { authority, generation, sequence } => { let _ = (authority, generation, sequence); }
                SessionAction::ReadBody { request_id, generation, sequence } => { let _ = (request_id, generation, sequence); }
                SessionAction::SendCustomAdapter { adapter_id, request_id, generation, correlation, sequence } => { let _ = (adapter_id, request_id, generation, correlation, sequence); }
                SessionAction::DispatchHook { hook_id, response_id, generation, correlation, sequence } => { let _ = (hook_id, response_id, generation, correlation, sequence); }
                SessionAction::RunAuth { auth_id, request_id, generation, sequence } => { let _ = (auth_id, request_id, generation, sequence); }
                SessionAction::ExtractCookies { jar_id, request_id, response_id, generation, sequence } => { let _ = (jar_id, request_id, response_id, generation, sequence); }
                SessionAction::NestedSubmit { request_id, generation, parent_correlation, correlation, sequence } => { let _ = (request_id, generation, parent_correlation, correlation, sequence); }
            }
        }
    }

    #[test]
    fn every_reply_and_transfer_constructs_and_exhaustively_destructures() {
        let generation = GenerationId::checked(7).unwrap();
        let sequence = Sequence::checked(11).unwrap();
        let correlation = CorrelationId::checked(13).unwrap();
        let request_id = RequestId::checked(17, generation).unwrap();
        let response_id = ResponseId::checked(19, generation).unwrap();
        let adapter_id = AdapterId::checked(23, generation).unwrap();
        let value = OpaqueValueId::checked(41, generation).unwrap();
        let error_id = OpaqueValueId::checked(43, generation).unwrap();

        let replies = [
            SessionReply::Scalar { value, generation, correlation, sequence },
            SessionReply::Response { response_id, generation, correlation, sequence },
            SessionReply::Nested { request_id, generation, correlation, sequence },
            SessionReply::Raised { error_id, generation, correlation, sequence },
        ];
        for reply in replies {
            match reply {
                SessionReply::Scalar { value, generation, correlation, sequence } => { let _ = (value, generation, correlation, sequence); }
                SessionReply::Response { response_id, generation, correlation, sequence } => { let _ = (response_id, generation, correlation, sequence); }
                SessionReply::Nested { request_id, generation, correlation, sequence } => { let _ = (request_id, generation, correlation, sequence); }
                SessionReply::Raised { error_id, generation, correlation, sequence } => { let _ = (error_id, generation, correlation, sequence); }
            }
        }

        let transfer = NativeTransfer { method: MethodId::Get, url: UrlId::checked(47, generation).unwrap(), headers: HeadersId::checked(53, generation).unwrap(), body_id: value, adapter_id, generation, correlation };
        let NativeTransfer { method, url, headers, body_id, adapter_id, generation, correlation } = transfer;
        let _ = (method, url, headers, body_id, adapter_id, generation, correlation);
    }

    #[test]
    fn python_and_origin_values_are_not_worker_payloads() {
        let _ = <pyo3::Py<pyo3::PyAny> as AmbiguousIfWorkerPayload<_>>::marker;
        let _ = <pyo3::PyErr as AmbiguousIfWorkerPayload<_>>::marker;
        let _ = <BorrowedValue<'static> as AmbiguousIfWorkerPayload<_>>::marker;
        let _ = <OriginSessionOwner as AmbiguousIfWorkerPayload<_>>::marker;
        let _ = <OriginSessionDestructor as AmbiguousIfWorkerPayload<_>>::marker;
        let _ = <CompletionOwner as AmbiguousIfSend<_>>::marker;
        let _ = <dyn std::fmt::Debug as AmbiguousIfWorkerPayload<_>>::marker;
    }

    #[test]
    fn checked_ids_reject_wrong_categories_and_stale_generations() {
        let generation = GenerationId::checked(7).unwrap();
        let stale = GenerationId::checked(8).unwrap();
        let request_id = RequestId::checked(17, generation).unwrap();
        let response_id = ResponseId::checked(19, generation).unwrap();
        let adapter_id = AdapterId::checked(23, generation).unwrap();
        let jar_id = JarId::checked(29, generation).unwrap();
        let hook_id = HookId::checked(31, generation).unwrap();
        let auth_id = AuthId::checked(37, generation).unwrap();
        let cursor_id = CursorId::checked(39, generation).unwrap();
        let value_id = OpaqueValueId::checked(41, generation).unwrap();
        assert!(ResponseId::try_from_request(request_id).is_err());
        assert!(AdapterId::try_from_response(response_id).is_err());
        assert!(JarId::try_from_adapter(adapter_id).is_err());
        assert!(HookId::try_from_jar(jar_id).is_err());
        assert!(AuthId::try_from_hook(hook_id).is_err());
        assert!(CursorId::try_from_auth(auth_id).is_err());
        assert!(OpaqueValueId::try_from_cursor(cursor_id).is_err());
        assert!(request_id.validate_generation(stale).is_err());
        assert!(response_id.validate_generation(stale).is_err());
        assert!(adapter_id.validate_generation(stale).is_err());
        assert!(jar_id.validate_generation(stale).is_err());
        assert!(hook_id.validate_generation(stale).is_err());
        assert!(auth_id.validate_generation(stale).is_err());
        assert!(cursor_id.validate_generation(stale).is_err());
        assert!(value_id.validate_generation(stale).is_err());
    }

    #[test]
    fn checked_ids_reject_overflow_and_unchecked_allocation() {
        let generation = GenerationId::checked(7).unwrap();
        assert!(RequestId::checked(u64::MAX, generation).is_err());
        assert!(OpaqueValueId::checked(u64::MAX, generation).is_err());
        assert!(SessionIdAllocator::checked(u64::MAX).is_err());
    }
}
