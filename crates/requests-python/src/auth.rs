use pyo3::exceptions::{PyDeprecationWarning, PyKeyError, PyRuntimeError};
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyAny, PyBytes, PyDict, PyList, PyModule, PyString, PyTuple, PyType};
use pyo3::wrap_pyfunction;
use requests::auth::{
    BasicCredentials, Digest401Machine, Digest401Step, DigestChallenge, DigestOutput,
    DigestPreparation, DigestRequest, DigestState, prepare_digest,
};

use crate::bridge::{ActionSender, BridgeClosed, WorkerPayload};
use crate::runtime::run_with_owned_actions;

struct AuthState {
    module: Py<PyModule>,
    basic_function: Py<PyAny>,
    basic_code: Py<PyAny>,
    basestring: Py<PyAny>,
    compat_str: Py<PyAny>,
    b64encode: Py<PyAny>,
    to_native_string: Py<PyAny>,
    to_native_string_code: Py<PyAny>,
    warnings: Py<PyAny>,
    warn: Py<PyAny>,
    basic_type: Py<PyType>,
    basic_call: Py<PyAny>,
    proxy_type: Py<PyType>,
    proxy_call: Py<PyAny>,
    digest_type: Py<PyType>,
    digest_build_code: Py<PyAny>,
    digest_401_code: Py<PyAny>,
    hashlib: Py<PyAny>,
    hashlib_functions: Vec<(&'static str, Py<PyAny>)>,
    os: Py<PyAny>,
    time: Py<PyAny>,
    urlparse: Py<PyAny>,
    extract_cookies_to_jar: Py<PyAny>,
    parse_dict_header: Py<PyAny>,
    re: Py<PyAny>,
    response_type: Py<PyType>,
    prepared_request_type: Py<PyType>,
    cookie_jar_type: Py<PyType>,
    header_type: Py<PyType>,
    response_content: Py<PyAny>,
    response_close: Py<PyAny>,
    prepared_copy: Py<PyAny>,
    prepared_cookies: Py<PyAny>,
}

static AUTH_STATE: PyOnceLock<AuthState> = PyOnceLock::new();

fn initialize_auth_state(py: Python<'_>) -> PyResult<AuthState> {
    let module = PyModule::import(py, "requests.auth")?;
    let basic_function = module.getattr("_basic_auth_str")?;
    let to_native_string = module.getattr("to_native_string")?;
    let warnings = module.getattr("warnings")?;
    let basic_type = module.getattr("HTTPBasicAuth")?.cast_into::<PyType>()?;
    let proxy_type = module.getattr("HTTPProxyAuth")?.cast_into::<PyType>()?;
    let digest_type = module.getattr("HTTPDigestAuth")?.cast_into::<PyType>()?;
    let digest_build = digest_type.getattr("build_digest_header")?;
    let digest_401 = digest_type.getattr("handle_401")?;
    let hashlib = module.getattr("hashlib")?;
    let hashlib_functions = ["md5", "sha1", "sha256", "sha512"]
        .into_iter()
        .map(|name| Ok((name, hashlib.getattr(name)?.unbind())))
        .collect::<PyResult<Vec<_>>>()?;
    let models = PyModule::import(py, "requests.models")?;
    let cookies = PyModule::import(py, "requests.cookies")?;
    let structures = PyModule::import(py, "requests.structures")?;
    let response_type = models.getattr("Response")?.cast_into::<PyType>()?;
    let prepared_request_type = models.getattr("PreparedRequest")?.cast_into::<PyType>()?;
    let cookie_jar_type = cookies
        .getattr("RequestsCookieJar")?
        .cast_into::<PyType>()?;
    let header_type = structures
        .getattr("CaseInsensitiveDict")?
        .cast_into::<PyType>()?;
    Ok(AuthState {
        module: module.clone().unbind(),
        basic_code: basic_function.getattr("__code__")?.unbind(),
        basic_function: basic_function.unbind(),
        basestring: module.getattr("basestring")?.unbind(),
        compat_str: module.getattr("str")?.unbind(),
        b64encode: module.getattr("b64encode")?.unbind(),
        to_native_string_code: to_native_string.getattr("__code__")?.unbind(),
        to_native_string: to_native_string.unbind(),
        warn: warnings.getattr("warn")?.unbind(),
        warnings: warnings.unbind(),
        basic_call: basic_type.getattr("__call__")?.unbind(),
        basic_type: basic_type.unbind(),
        proxy_call: proxy_type.getattr("__call__")?.unbind(),
        proxy_type: proxy_type.unbind(),
        digest_type: digest_type.unbind(),
        digest_build_code: digest_build.getattr("__code__")?.unbind(),
        digest_401_code: digest_401.getattr("__code__")?.unbind(),
        hashlib: hashlib.unbind(),
        hashlib_functions,
        os: module.getattr("os")?.unbind(),
        time: module.getattr("time")?.unbind(),
        urlparse: module.getattr("urlparse")?.unbind(),
        extract_cookies_to_jar: module.getattr("extract_cookies_to_jar")?.unbind(),
        parse_dict_header: module.getattr("parse_dict_header")?.unbind(),
        re: module.getattr("re")?.unbind(),
        response_content: response_type.getattr("content")?.unbind(),
        response_close: response_type.getattr("close")?.unbind(),
        prepared_copy: prepared_request_type.getattr("copy")?.unbind(),
        prepared_cookies: prepared_request_type.getattr("prepare_cookies")?.unbind(),
        response_type: response_type.unbind(),
        prepared_request_type: prepared_request_type.unbind(),
        cookie_jar_type: cookie_jar_type.unbind(),
        header_type: header_type.unbind(),
    })
}

fn auth_state(py: Python<'_>) -> PyResult<&AuthState> {
    AUTH_STATE.get_or_try_init(py, || initialize_auth_state(py))
}

fn module_entry_is(
    module: &Bound<'_, PyModule>,
    name: &str,
    expected: &Py<PyAny>,
) -> PyResult<bool> {
    Ok(module.getattr(name)?.is(expected.bind(module.py())))
}

fn basic_is_pristine(py: Python<'_>, state: &AuthState) -> PyResult<bool> {
    let module = state.module.bind(py);
    let function = module.getattr("_basic_auth_str")?;
    if !function.is(state.basic_function.bind(py))
        || !function.getattr("__code__")?.is(state.basic_code.bind(py))
        || !module_entry_is(module, "basestring", &state.basestring)?
        || !module_entry_is(module, "str", &state.compat_str)?
        || !module_entry_is(module, "b64encode", &state.b64encode)?
        || !module_entry_is(module, "to_native_string", &state.to_native_string)?
        || !module_entry_is(module, "warnings", &state.warnings)?
    {
        return Ok(false);
    }
    let to_native_string = state.to_native_string.bind(py);
    Ok(to_native_string
        .getattr("__code__")?
        .is(state.to_native_string_code.bind(py))
        && state
            .warnings
            .bind(py)
            .getattr("warn")?
            .is(state.warn.bind(py)))
}

fn is_exact_string_or_bytes(value: &Bound<'_, PyAny>) -> bool {
    value.is_exact_instance_of::<PyString>() || value.is_exact_instance_of::<PyBytes>()
}

fn is_non_exact_string_or_bytes_subclass(value: &Bound<'_, PyAny>) -> PyResult<bool> {
    if is_exact_string_or_bytes(value) {
        return Ok(false);
    }
    let py = value.py();
    let value_type = value.get_type();
    Ok(value_type.is_subclass(&py.get_type::<PyString>())?
        || value_type.is_subclass(&py.get_type::<PyBytes>())?)
}

fn warn_deprecated(py: Python<'_>, state: &AuthState, message: String) -> PyResult<()> {
    let kwargs = PyDict::new(py);
    kwargs.set_item("category", py.get_type::<PyDeprecationWarning>())?;
    state
        .warnings
        .bind(py)
        .getattr("warn")?
        .call((message,), Some(&kwargs))?;
    Ok(())
}

fn coerce_credential_after_check(
    py: Python<'_>,
    state: &AuthState,
    value: &Bound<'_, PyAny>,
    username: bool,
    is_basestring: bool,
) -> PyResult<Vec<u8>> {
    let owned = if is_basestring {
        value.clone()
    } else {
        let message = if username {
            format!(
                "Non-string usernames will no longer be supported in Requests 3.0.0. \
                 Please convert the object you've passed in ({}) to a string or bytes object \
                 in the near future to avoid problems.",
                value.repr()?.to_str()?
            )
        } else {
            format!(
                "Non-string passwords will no longer be supported in Requests 3.0.0. \
                 Please convert the object you've passed in ({}) to a string or bytes object \
                 in the near future to avoid problems.",
                value.get_type().repr()?.to_str()?
            )
        };
        warn_deprecated(py, state, message)?;
        state.compat_str.bind(py).call1((value,))?
    };
    let encoded = if owned.is_instance_of::<PyString>() {
        owned.call_method1("encode", ("latin1",))?
    } else {
        owned
    };
    Ok(encoded.cast::<PyBytes>()?.as_bytes().to_vec())
}

#[pyfunction]
fn _basic_auth_trial(
    py: Python<'_>,
    compat: &Bound<'_, PyAny>,
    username: &Bound<'_, PyAny>,
    password: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let state = auth_state(py)?;
    if !matches!(basic_is_pristine(py, state), Ok(true)) {
        return Ok(compat.call1((username, password))?.unbind());
    }
    if is_non_exact_string_or_bytes_subclass(username)?
        || is_non_exact_string_or_bytes_subclass(password)?
    {
        return Ok(compat.call1((username, password))?.unbind());
    }
    let username_is_basestring = username.is_instance(state.basestring.bind(py))?;
    let username =
        coerce_credential_after_check(py, state, username, true, username_is_basestring)?;
    let password_is_basestring = password.is_instance(state.basestring.bind(py))?;
    let password =
        coerce_credential_after_check(py, state, password, false, password_is_basestring)?;
    Ok(BasicCredentials::new(username, password)
        .authorization()
        .into_pyobject(py)?
        .into_any()
        .unbind())
}

fn fallback_basic_apply<'py>(
    state: &AuthState,
    py: Python<'py>,
    subject: &Bound<'py, PyAny>,
    username: &Bound<'py, PyAny>,
    password: &Bound<'py, PyAny>,
    proxy: bool,
) -> PyResult<Py<PyAny>> {
    let class_name = if proxy {
        "HTTPProxyAuth"
    } else {
        "HTTPBasicAuth"
    };
    Ok(state
        .module
        .bind(py)
        .getattr(class_name)?
        .call1((username, password))?
        .call1((subject,))?
        .unbind())
}

#[pyfunction]
fn _basic_auth_apply_trial(
    py: Python<'_>,
    subject: &Bound<'_, PyAny>,
    username: &Bound<'_, PyAny>,
    password: &Bound<'_, PyAny>,
    proxy: bool,
) -> PyResult<Py<PyAny>> {
    let state = auth_state(py)?;
    let (auth_type, expected_call) = if proxy {
        (&state.proxy_type, &state.proxy_call)
    } else {
        (&state.basic_type, &state.basic_call)
    };
    let class_name = if proxy {
        "HTTPProxyAuth"
    } else {
        "HTTPBasicAuth"
    };
    let apply_is_pristine = || -> PyResult<bool> {
        let live_type = state
            .module
            .bind(py)
            .getattr(class_name)?
            .cast_into::<PyType>()?;
        Ok(live_type.is(auth_type.bind(py))
            && live_type.getattr("__call__")?.is(expected_call.bind(py)))
    };
    if !matches!(basic_is_pristine(py, state), Ok(true)) || !matches!(apply_is_pristine(), Ok(true))
    {
        return fallback_basic_apply(state, py, subject, username, password, proxy);
    }
    if is_non_exact_string_or_bytes_subclass(username)?
        || is_non_exact_string_or_bytes_subclass(password)?
    {
        return fallback_basic_apply(state, py, subject, username, password, proxy);
    }
    let username_is_basestring = username.is_instance(state.basestring.bind(py))?;
    let username =
        coerce_credential_after_check(py, state, username, true, username_is_basestring)?;
    let password_is_basestring = password.is_instance(state.basestring.bind(py))?;
    let password =
        coerce_credential_after_check(py, state, password, false, password_is_basestring)?;
    let authorization = BasicCredentials::new(username, password).authorization();
    let header = if proxy {
        "Proxy-Authorization"
    } else {
        "Authorization"
    };
    subject
        .getattr("headers")?
        .set_item(header, authorization)?;
    Ok(subject.clone().unbind())
}

#[derive(Debug)]
enum DigestAction {
    Ctime,
    Random { size: usize },
}

#[derive(Debug)]
enum DigestReply {
    Bytes(Vec<u8>),
    Failed,
}

impl WorkerPayload for DigestAction {}
impl WorkerPayload for DigestReply {}

#[derive(Debug)]
enum DigestOutcome {
    Complete(DigestOutput),
    HandlerFailed,
    BridgeClosed(BridgeClosed),
}

struct OriginDigestOwner {
    module: Py<PyModule>,
    handler_error: Option<PyErr>,
}

fn store_digest_error(slot: &mut Option<PyErr>, error: PyErr) -> DigestReply {
    if slot.is_none() {
        *slot = Some(error);
    }
    DigestReply::Failed
}

fn store_digest_401_error(slot: &mut Option<PyErr>, error: PyErr) -> Digest401Reply {
    if slot.is_none() {
        *slot = Some(error);
    }
    Digest401Reply::Failed
}

fn digest_action(
    py: Python<'_>,
    action: DigestAction,
    owner: &OriginDigestOwner,
) -> PyResult<DigestReply> {
    let module = owner.module.bind(py);
    match action {
        DigestAction::Ctime => {
            let value = module
                .getattr("time")?
                .getattr("ctime")?
                .call0()?
                .call_method1("encode", ("utf-8",))?;
            Ok(DigestReply::Bytes(
                value.cast::<PyBytes>()?.as_bytes().to_vec(),
            ))
        }
        DigestAction::Random { size } => {
            let value = module.getattr("os")?.getattr("urandom")?.call1((size,))?;
            Ok(DigestReply::Bytes(
                value.cast::<PyBytes>()?.as_bytes().to_vec(),
            ))
        }
    }
}

async fn digest_worker(
    actions: ActionSender<DigestAction, DigestReply>,
    plan: requests::auth::DigestPlan,
) -> DigestOutcome {
    let ctime = match actions.request(DigestAction::Ctime).await {
        Ok(DigestReply::Bytes(value)) => value,
        Ok(DigestReply::Failed) => return DigestOutcome::HandlerFailed,
        Err(error) => return DigestOutcome::BridgeClosed(error),
    };
    let random = match actions.request(DigestAction::Random { size: 8 }).await {
        Ok(DigestReply::Bytes(value)) => value,
        Ok(DigestReply::Failed) => return DigestOutcome::HandlerFailed,
        Err(error) => return DigestOutcome::BridgeClosed(error),
    };
    DigestOutcome::Complete(plan.finish(&ctime, &random))
}

fn digest_is_pristine(
    py: Python<'_>,
    state: &AuthState,
    subject: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    if !subject.get_type().is(state.digest_type.bind(py)) {
        return Ok(false);
    }
    let module = state.module.bind(py);
    if !module_entry_is(module, "hashlib", &state.hashlib)?
        || !module_entry_is(module, "os", &state.os)?
        || !module_entry_is(module, "time", &state.time)?
        || !module_entry_is(module, "urlparse", &state.urlparse)?
        || !module_entry_is(module, "str", &state.compat_str)?
    {
        return Ok(false);
    }
    for (name, expected) in &state.hashlib_functions {
        if !state.hashlib.bind(py).getattr(*name)?.is(expected.bind(py)) {
            return Ok(false);
        }
    }
    Ok(state
        .digest_type
        .bind(py)
        .getattr("build_digest_header")?
        .getattr("__code__")?
        .is(state.digest_build_code.bind(py)))
}

fn exact_string(value: Bound<'_, PyAny>) -> PyResult<Option<String>> {
    if value.is_exact_instance_of::<PyString>() {
        Ok(Some(value.extract::<String>()?))
    } else {
        Ok(None)
    }
}

fn optional_exact_string(value: Option<Bound<'_, PyAny>>) -> PyResult<Option<Option<String>>> {
    match value {
        None => Ok(Some(None)),
        Some(value) if value.is_none() => Ok(Some(None)),
        Some(value) => Ok(exact_string(value)?.map(Some)),
    }
}

fn required_challenge_string(
    challenge: &Bound<'_, PyDict>,
    name: &'static str,
) -> PyResult<String> {
    let value = challenge
        .get_item(name)?
        .ok_or_else(|| PyKeyError::new_err(name))?;
    exact_string(value)?.ok_or_else(|| {
        PyRuntimeError::new_err("digest trial requires exact string challenge values")
    })
}

fn bridge_error(error: BridgeClosed) -> PyErr {
    PyRuntimeError::new_err(error.to_string())
}

#[pyfunction]
fn _digest_auth_trial(
    py: Python<'_>,
    compat: &Bound<'_, PyAny>,
    subject: &Bound<'_, PyAny>,
    operation: &str,
    arguments: &Bound<'_, PyTuple>,
) -> PyResult<Py<PyAny>> {
    let state = auth_state(py)?;
    if operation != "build_digest_header"
        || arguments.len() != 2
        || !matches!(digest_is_pristine(py, state, subject), Ok(true))
    {
        return Ok(compat.call0()?.unbind());
    }
    let Some(method) = exact_string(arguments.get_item(0)?)? else {
        return Ok(compat.call0()?.unbind());
    };
    let Some(url) = exact_string(arguments.get_item(1)?)? else {
        return Ok(compat.call0()?.unbind());
    };
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Ok(compat.call0()?.unbind());
    }
    let Some(username) = exact_string(subject.getattr("username")?)? else {
        return Ok(compat.call0()?.unbind());
    };
    let Some(password) = exact_string(subject.getattr("password")?)? else {
        return Ok(compat.call0()?.unbind());
    };
    let thread_local = subject.getattr("_thread_local")?;
    let challenge_value = thread_local.getattr("chal")?;
    if !challenge_value.is_exact_instance_of::<PyDict>() {
        return Ok(compat.call0()?.unbind());
    }
    let challenge = challenge_value.cast::<PyDict>()?;
    let realm = required_challenge_string(challenge, "realm")?;
    let nonce = required_challenge_string(challenge, "nonce")?;
    let Some(qop) = optional_exact_string(challenge.get_item("qop")?)? else {
        return Ok(compat.call0()?.unbind());
    };
    let Some(algorithm) = optional_exact_string(challenge.get_item("algorithm")?)? else {
        return Ok(compat.call0()?.unbind());
    };
    let Some(opaque) = optional_exact_string(challenge.get_item("opaque")?)? else {
        return Ok(compat.call0()?.unbind());
    };
    let Some(last_nonce) = exact_string(thread_local.getattr("last_nonce")?)? else {
        return Ok(compat.call0()?.unbind());
    };
    let nonce_count_value = thread_local.getattr("nonce_count")?;
    if !nonce_count_value.is_exact_instance_of::<pyo3::types::PyInt>() {
        return Ok(compat.call0()?.unbind());
    }
    let Ok(nonce_count) = nonce_count_value.extract::<u32>() else {
        return Ok(compat.call0()?.unbind());
    };

    let request = DigestRequest {
        username,
        password,
        method,
        url,
        challenge: DigestChallenge {
            realm,
            nonce,
            qop,
            algorithm,
            opaque,
        },
    };
    let preparation = prepare_digest(
        request,
        DigestState {
            last_nonce,
            nonce_count,
        },
    );
    let DigestPreparation::Ready(plan) = preparation else {
        return Ok(py.None());
    };

    thread_local.setattr("nonce_count", plan.state().nonce_count)?;
    let owner = OriginDigestOwner {
        module: state.module.clone_ref(py),
        handler_error: None,
    };
    let (outcome, mut owner) = run_with_owned_actions(
        py,
        owner,
        move |actions| digest_worker(actions, *plan),
        |py, action, owner| match digest_action(py, action, owner) {
            Ok(reply) => reply,
            Err(error) => store_digest_error(&mut owner.handler_error, error),
        },
    )?;
    if let Some(error) = owner.handler_error.take() {
        return Err(error);
    }
    match outcome {
        DigestOutcome::Complete(output) => {
            if output.header.is_some() {
                thread_local.setattr("last_nonce", output.state.last_nonce)?;
            }
            match output.header {
                Some(header) => Ok(header.into_pyobject(py)?.into_any().unbind()),
                None => Ok(py.None()),
            }
        }
        DigestOutcome::HandlerFailed => Err(PyRuntimeError::new_err(
            "digest callback failed without preserving its Python exception",
        )),
        DigestOutcome::BridgeClosed(error) => Err(bridge_error(error)),
    }
}

#[derive(Debug)]
enum Digest401Action {
    Seek,
    Challenge,
    Reset401,
    Increment401 { value: u32 },
    Parse { header: String },
    Consume,
    Close,
    Copy,
    ExtractCookies,
    PrepareCookies,
    BuildDigestHeader,
    Send,
    AppendHistory,
    ReplaceRequest,
}

#[derive(Debug)]
enum Digest401Reply {
    Ack,
    Header(String),
    Failed,
}

impl WorkerPayload for Digest401Action {}
impl WorkerPayload for Digest401Reply {}

#[derive(Debug)]
enum Digest401Outcome {
    Original,
    Sent,
    HandlerFailed,
    BridgeClosed(BridgeClosed),
}

struct Digest401Input {
    num_401_calls: u32,
}

struct OriginDigest401Owner {
    module: Py<PyModule>,
    subject: Py<PyAny>,
    response: Py<PyAny>,
    kwargs: Py<PyDict>,
    audit: Py<PyList>,
    prepared: Option<Py<PyAny>>,
    sent: Option<Py<PyAny>>,
    handler_error: Option<PyErr>,
}

fn digest_401_audit(owner: &OriginDigest401Owner, py: Python<'_>, label: &str) -> PyResult<()> {
    owner.audit.bind(py).append(label)
}

fn digest_401_parse_challenge(
    py: Python<'_>,
    owner: &OriginDigest401Owner,
    header: &str,
) -> PyResult<Py<PyDict>> {
    let module = owner.module.bind(py);
    let compile_kwargs = PyDict::new(py);
    compile_kwargs.set_item("flags", module.getattr("re")?.getattr("IGNORECASE")?)?;
    let pattern = module
        .getattr("re")?
        .getattr("compile")?
        .call(("digest ",), Some(&compile_kwargs))?;
    let sub_kwargs = PyDict::new(py);
    sub_kwargs.set_item("count", 1)?;
    let stripped = pattern.call_method("sub", ("", header), Some(&sub_kwargs))?;
    Ok(module
        .getattr("parse_dict_header")?
        .call1((stripped,))?
        .cast_into::<PyDict>()?
        .unbind())
}

fn digest_401_action(
    py: Python<'_>,
    action: Digest401Action,
    owner: &mut OriginDigest401Owner,
) -> PyResult<Digest401Reply> {
    let subject = owner.subject.bind(py);
    let response = owner.response.bind(py);
    let thread_local = subject.getattr("_thread_local")?;
    match action {
        Digest401Action::Seek => {
            let position = thread_local.getattr("pos")?;
            if !position.is_none() {
                let body = response.getattr("request")?.getattr("body")?;
                let seek = body.getattr("seek").ok();
                if let Some(seek) = seek {
                    digest_401_audit(owner, py, "seek")?;
                    seek.call1((position,))?;
                }
            }
            Ok(Digest401Reply::Ack)
        }
        Digest401Action::Challenge => {
            digest_401_audit(owner, py, "challenge")?;
            let header = response
                .getattr("headers")?
                .call_method1("get", ("www-authenticate", ""))?
                .extract::<String>()?;
            Ok(Digest401Reply::Header(header))
        }
        Digest401Action::Reset401 => {
            thread_local.setattr("num_401_calls", 1)?;
            Ok(Digest401Reply::Ack)
        }
        Digest401Action::Increment401 { value } => {
            thread_local.setattr("num_401_calls", value)?;
            Ok(Digest401Reply::Ack)
        }
        Digest401Action::Parse { header } => {
            let challenge = digest_401_parse_challenge(py, owner, &header)?;
            thread_local.setattr("chal", challenge.bind(py))?;
            Ok(Digest401Reply::Ack)
        }
        Digest401Action::Consume => {
            digest_401_audit(owner, py, "content")?;
            response.getattr("content")?;
            Ok(Digest401Reply::Ack)
        }
        Digest401Action::Close => {
            digest_401_audit(owner, py, "close")?;
            response.call_method0("close")?;
            Ok(Digest401Reply::Ack)
        }
        Digest401Action::Copy => {
            digest_401_audit(owner, py, "copy")?;
            owner.prepared = Some(response.getattr("request")?.call_method0("copy")?.unbind());
            Ok(Digest401Reply::Ack)
        }
        Digest401Action::ExtractCookies => {
            digest_401_audit(owner, py, "extract")?;
            let prepared = owner
                .prepared
                .as_ref()
                .ok_or_else(|| PyRuntimeError::new_err("digest resend has no prepared request"))?
                .bind(py);
            owner
                .module
                .bind(py)
                .getattr("extract_cookies_to_jar")?
                .call1((
                    prepared.getattr("_cookies")?,
                    response.getattr("request")?,
                    response.getattr("raw")?,
                ))?;
            Ok(Digest401Reply::Ack)
        }
        Digest401Action::PrepareCookies => {
            digest_401_audit(owner, py, "prepare-cookies")?;
            let prepared = owner
                .prepared
                .as_ref()
                .ok_or_else(|| PyRuntimeError::new_err("digest resend has no prepared request"))?
                .bind(py);
            let jar = prepared.getattr("_cookies")?;
            prepared.call_method1("prepare_cookies", (jar,))?;
            Ok(Digest401Reply::Ack)
        }
        Digest401Action::BuildDigestHeader => {
            digest_401_audit(owner, py, "build")?;
            let prepared = owner
                .prepared
                .as_ref()
                .ok_or_else(|| PyRuntimeError::new_err("digest resend has no prepared request"))?
                .bind(py);
            let header = subject
                .getattr("build_digest_header")?
                .call1((prepared.getattr("method")?, prepared.getattr("url")?))?;
            digest_401_audit(owner, py, "header")?;
            prepared
                .getattr("headers")?
                .set_item("Authorization", header)?;
            Ok(Digest401Reply::Ack)
        }
        Digest401Action::Send => {
            digest_401_audit(owner, py, "send")?;
            let prepared = owner
                .prepared
                .as_ref()
                .ok_or_else(|| PyRuntimeError::new_err("digest resend has no prepared request"))?
                .bind(py);
            owner.sent = Some(
                response
                    .getattr("connection")?
                    .getattr("send")?
                    .call((prepared,), Some(owner.kwargs.bind(py)))?
                    .unbind(),
            );
            Ok(Digest401Reply::Ack)
        }
        Digest401Action::AppendHistory => {
            digest_401_audit(owner, py, "history")?;
            owner
                .sent
                .as_ref()
                .ok_or_else(|| PyRuntimeError::new_err("digest resend has no response"))?
                .bind(py)
                .getattr("history")?
                .call_method1("append", (response,))?;
            Ok(Digest401Reply::Ack)
        }
        Digest401Action::ReplaceRequest => {
            digest_401_audit(owner, py, "request")?;
            let prepared = owner
                .prepared
                .as_ref()
                .ok_or_else(|| PyRuntimeError::new_err("digest resend has no prepared request"))?
                .bind(py);
            owner
                .sent
                .as_ref()
                .ok_or_else(|| PyRuntimeError::new_err("digest resend has no response"))?
                .bind(py)
                .setattr("request", prepared)?;
            Ok(Digest401Reply::Ack)
        }
    }
}

async fn request_digest_401(
    actions: &ActionSender<Digest401Action, Digest401Reply>,
    action: Digest401Action,
) -> Result<Digest401Reply, Digest401Outcome> {
    actions
        .request(action)
        .await
        .map_err(Digest401Outcome::BridgeClosed)
}

async fn digest_401_worker(
    actions: ActionSender<Digest401Action, Digest401Reply>,
    input: Digest401Input,
) -> Digest401Outcome {
    let mut header = None;
    for step in Digest401Machine::new() {
        match step {
            Digest401Step::Seek => {
                match request_digest_401(&actions, Digest401Action::Seek).await {
                    Ok(Digest401Reply::Ack) => {}
                    Ok(Digest401Reply::Failed) => return Digest401Outcome::HandlerFailed,
                    Ok(_) => return Digest401Outcome::HandlerFailed,
                    Err(outcome) => return outcome,
                }
            }
            Digest401Step::Challenge => {
                match request_digest_401(&actions, Digest401Action::Challenge).await {
                    Ok(Digest401Reply::Header(value)) => header = Some(value),
                    Ok(Digest401Reply::Failed) => return Digest401Outcome::HandlerFailed,
                    Ok(_) => return Digest401Outcome::HandlerFailed,
                    Err(outcome) => return outcome,
                }
            }
            Digest401Step::Increment => {
                let header = header.as_ref().expect("challenge precedes increment");
                if !header.to_lowercase().contains("digest") || input.num_401_calls >= 2 {
                    return match request_digest_401(&actions, Digest401Action::Reset401).await {
                        Ok(Digest401Reply::Ack) => Digest401Outcome::Original,
                        Ok(Digest401Reply::Failed) => Digest401Outcome::HandlerFailed,
                        Ok(_) => Digest401Outcome::HandlerFailed,
                        Err(outcome) => outcome,
                    };
                }
                match request_digest_401(
                    &actions,
                    Digest401Action::Increment401 {
                        value: input.num_401_calls + 1,
                    },
                )
                .await
                {
                    Ok(Digest401Reply::Ack) => {}
                    Ok(Digest401Reply::Failed) => return Digest401Outcome::HandlerFailed,
                    Ok(_) => return Digest401Outcome::HandlerFailed,
                    Err(outcome) => return outcome,
                }
            }
            Digest401Step::Parse => {
                match request_digest_401(
                    &actions,
                    Digest401Action::Parse {
                        header: header.take().expect("challenge precedes parse"),
                    },
                )
                .await
                {
                    Ok(Digest401Reply::Ack) => {}
                    Ok(Digest401Reply::Failed) => return Digest401Outcome::HandlerFailed,
                    Ok(_) => return Digest401Outcome::HandlerFailed,
                    Err(outcome) => return outcome,
                }
            }
            Digest401Step::Consume
            | Digest401Step::Close
            | Digest401Step::Copy
            | Digest401Step::ExtractCookies
            | Digest401Step::PrepareCookies => {
                let action = match step {
                    Digest401Step::Consume => Digest401Action::Consume,
                    Digest401Step::Close => Digest401Action::Close,
                    Digest401Step::Copy => Digest401Action::Copy,
                    Digest401Step::ExtractCookies => Digest401Action::ExtractCookies,
                    Digest401Step::PrepareCookies => Digest401Action::PrepareCookies,
                    _ => unreachable!(),
                };
                match request_digest_401(&actions, action).await {
                    Ok(Digest401Reply::Ack) => {}
                    Ok(Digest401Reply::Failed) => return Digest401Outcome::HandlerFailed,
                    Ok(_) => return Digest401Outcome::HandlerFailed,
                    Err(outcome) => return outcome,
                }
            }
            Digest401Step::PreparedParts => {
                match request_digest_401(&actions, Digest401Action::BuildDigestHeader).await {
                    Ok(Digest401Reply::Ack) => {}
                    Ok(Digest401Reply::Failed) => return Digest401Outcome::HandlerFailed,
                    Ok(_) => return Digest401Outcome::HandlerFailed,
                    Err(outcome) => return outcome,
                }
            }
            Digest401Step::UpdateNonceCount
            | Digest401Step::Ctime
            | Digest401Step::Random
            | Digest401Step::UpdateLastNonce
            | Digest401Step::SetHeader => {}
            Digest401Step::Send | Digest401Step::AppendHistory | Digest401Step::ReplaceRequest => {
                let action = match step {
                    Digest401Step::Send => Digest401Action::Send,
                    Digest401Step::AppendHistory => Digest401Action::AppendHistory,
                    Digest401Step::ReplaceRequest => Digest401Action::ReplaceRequest,
                    _ => unreachable!(),
                };
                match request_digest_401(&actions, action).await {
                    Ok(Digest401Reply::Ack) => {}
                    Ok(Digest401Reply::Failed) => return Digest401Outcome::HandlerFailed,
                    Ok(_) => return Digest401Outcome::HandlerFailed,
                    Err(outcome) => return outcome,
                }
            }
        }
    }
    Digest401Outcome::Sent
}

fn digest_401_is_pristine(
    py: Python<'_>,
    state: &AuthState,
    subject: &Bound<'_, PyAny>,
    response: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    if !digest_is_pristine(py, state, subject)?
        || !subject.get_type().is(state.digest_type.bind(py))
        || !response.get_type().is(state.response_type.bind(py))
    {
        return Ok(false);
    }
    let module = state.module.bind(py);
    if !module
        .getattr("HTTPDigestAuth")?
        .getattr("handle_401")?
        .getattr("__code__")?
        .is(state.digest_401_code.bind(py))
        || !module_entry_is(
            module,
            "extract_cookies_to_jar",
            &state.extract_cookies_to_jar,
        )?
        || !module_entry_is(module, "parse_dict_header", &state.parse_dict_header)?
        || !module_entry_is(module, "re", &state.re)?
    {
        return Ok(false);
    }
    let response_type = state.response_type.bind(py);
    let prepared_type = state.prepared_request_type.bind(py);
    if !response_type
        .getattr("content")?
        .is(state.response_content.bind(py))
        || !response_type
            .getattr("close")?
            .is(state.response_close.bind(py))
        || !prepared_type
            .getattr("copy")?
            .is(state.prepared_copy.bind(py))
        || !prepared_type
            .getattr("prepare_cookies")?
            .is(state.prepared_cookies.bind(py))
    {
        return Ok(false);
    }
    let request = response.getattr("request")?;
    Ok(request.get_type().is(prepared_type)
        && response
            .getattr("headers")?
            .get_type()
            .is(state.header_type.bind(py))
        && request
            .getattr("headers")?
            .get_type()
            .is(state.header_type.bind(py))
        && request
            .getattr("_cookies")?
            .get_type()
            .is(state.cookie_jar_type.bind(py)))
}

#[pyfunction]
fn _digest_401_trial(
    py: Python<'_>,
    compat: &Bound<'_, PyAny>,
    subject: &Bound<'_, PyAny>,
    response: &Bound<'_, PyAny>,
    kwargs: Py<PyDict>,
    audit: Py<PyList>,
) -> PyResult<Py<PyAny>> {
    let state = auth_state(py)?;
    if !matches!(
        digest_401_is_pristine(py, state, subject, response),
        Ok(true)
    ) {
        return Ok(compat.call0()?.unbind());
    }
    let status = response.getattr("status_code")?;
    if !status.is_exact_instance_of::<pyo3::types::PyInt>() || status.extract::<i64>()? != 401 {
        return Ok(compat.call0()?.unbind());
    }
    let thread_local = subject.getattr("_thread_local")?;
    let Ok(num_401_calls) = thread_local.getattr("num_401_calls")?.extract::<u32>() else {
        return Ok(compat.call0()?.unbind());
    };
    let input = Digest401Input { num_401_calls };
    let owner = OriginDigest401Owner {
        module: state.module.clone_ref(py),
        subject: subject.clone().unbind(),
        response: response.clone().unbind(),
        kwargs,
        audit,
        prepared: None,
        sent: None,
        handler_error: None,
    };
    let (outcome, mut owner) = run_with_owned_actions(
        py,
        owner,
        move |actions| digest_401_worker(actions, input),
        |py, action, owner| match digest_401_action(py, action, owner) {
            Ok(reply) => reply,
            Err(error) => store_digest_401_error(&mut owner.handler_error, error),
        },
    )?;
    if let Some(error) = owner.handler_error.take() {
        return Err(error);
    }
    match outcome {
        Digest401Outcome::Original => Ok(owner.response.clone_ref(py)),
        Digest401Outcome::Sent => owner
            .sent
            .as_ref()
            .map(|response| response.clone_ref(py))
            .ok_or_else(|| PyRuntimeError::new_err("digest resend lost its response")),
        Digest401Outcome::HandlerFailed => Err(PyRuntimeError::new_err(
            "digest 401 action failed without preserving its Python exception",
        )),
        Digest401Outcome::BridgeClosed(error) => Err(bridge_error(error)),
    }
}

pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = module.py();
    AUTH_STATE.get_or_try_init(py, || initialize_auth_state(py))?;
    module.add_function(wrap_pyfunction!(_basic_auth_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_basic_auth_apply_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_digest_auth_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_digest_401_trial, module)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        Digest401Action, Digest401Reply, DigestAction, DigestReply, OriginDigest401Owner,
        OriginDigestOwner,
    };
    use crate::bridge::WorkerPayload;

    fn assert_worker_payload<T: WorkerPayload>() {}

    trait AmbiguousIfWorkerPayload<A> {
        fn marker() {}
    }

    impl<T: ?Sized> AmbiguousIfWorkerPayload<()> for T {}
    impl<T: ?Sized + WorkerPayload> AmbiguousIfWorkerPayload<u8> for T {}

    #[test]
    fn digest_action_payloads_are_worker_safe() {
        assert_worker_payload::<DigestAction>();
        assert_worker_payload::<DigestReply>();
        assert_worker_payload::<Digest401Action>();
        assert_worker_payload::<Digest401Reply>();
    }

    #[test]
    fn digest_origin_owner_is_not_a_worker_payload() {
        let _ = <OriginDigestOwner as AmbiguousIfWorkerPayload<_>>::marker;
        let _ = <OriginDigest401Owner as AmbiguousIfWorkerPayload<_>>::marker;
    }
}
