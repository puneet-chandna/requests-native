use pyo3::exceptions::{PyNameError, PyRuntimeError, PyStopIteration, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyAny, PyDict, PyList, PyModule, PyTuple};
use pyo3::wrap_pyfunction;
use requests::hooks::{HookCall, HookRegistry, HookValueId};

use crate::bridge::{BridgeClosed, WorkerPayload};
use crate::runtime::run_with_owned_actions;

struct HookState {
    module: Py<PyModule>,
}

static HOOK_STATE: PyOnceLock<HookState> = PyOnceLock::new();

fn initialize_hook_state(py: Python<'_>) -> PyResult<HookState> {
    let module = PyModule::import(py, "requests.hooks")?;
    Ok(HookState {
        module: module.unbind(),
    })
}

fn hook_state(py: Python<'_>) -> PyResult<&HookState> {
    HOOK_STATE.get_or_try_init(py, || initialize_hook_state(py))
}

#[derive(Debug)]
enum HookAction {
    Call(HookCall),
}

#[derive(Debug)]
enum HookReply {
    Unchanged,
    Replaced(HookValueId),
    End,
    Failed,
}

impl WorkerPayload for HookAction {}
impl WorkerPayload for HookReply {}

#[derive(Debug)]
enum HookOutcome {
    Complete(HookValueId),
    HandlerFailed,
    BridgeClosed(BridgeClosed),
}

struct OriginHookOwner {
    iterator: Py<PyAny>,
    values: Vec<Py<PyAny>>,
    kwargs: Py<PyDict>,
    handler_error: Option<PyErr>,
}

fn store_handler_error(slot: &mut Option<PyErr>, error: PyErr) -> HookReply {
    if slot.is_none() {
        *slot = Some(error);
    }
    HookReply::Failed
}

fn call_hook(py: Python<'_>, owner: &mut OriginHookOwner, call: HookCall) -> PyResult<HookReply> {
    let hook = match owner.iterator.bind(py).call_method0("__next__") {
        Ok(hook) => hook,
        Err(error) if error.is_instance_of::<PyStopIteration>(py) => return Ok(HookReply::End),
        Err(error) => return Err(error),
    };
    let Some(value) = owner.values.get(call.value.index()) else {
        return Err(PyRuntimeError::new_err(
            "hook dispatch referenced an unknown Python value",
        ));
    };
    let replacement = hook.call((value.bind(py),), Some(owner.kwargs.bind(py)))?;
    if replacement.is_none() {
        return Ok(HookReply::Unchanged);
    }
    let replacement_id = HookValueId::new(owner.values.len());
    owner.values.push(replacement.unbind());
    Ok(HookReply::Replaced(replacement_id))
}

fn execute_action(py: Python<'_>, action: HookAction, owner: &mut OriginHookOwner) -> HookReply {
    let result = match action {
        HookAction::Call(call) => call_hook(py, owner, call),
    };
    match result {
        Ok(reply) => reply,
        Err(error) => store_handler_error(&mut owner.handler_error, error),
    }
}

async fn dispatch_worker(
    actions: crate::bridge::ActionSender<HookAction, HookReply>,
) -> HookOutcome {
    let mut registry = HookRegistry::new(HookValueId::new(0));
    loop {
        let call = registry.next_call();
        match actions.request(HookAction::Call(call)).await {
            Ok(HookReply::Unchanged) => {}
            Ok(HookReply::Replaced(value)) => registry.replace(value),
            Ok(HookReply::End) => return HookOutcome::Complete(registry.current_value()),
            Ok(HookReply::Failed) => return HookOutcome::HandlerFailed,
            Err(error) => return HookOutcome::BridgeClosed(error),
        }
    }
}

fn bridge_error(error: BridgeClosed) -> PyErr {
    PyRuntimeError::new_err(error.to_string())
}

#[pyfunction(signature = (*args))]
fn _dispatch_hook_trial(py: Python<'_>, args: &Bound<'_, PyTuple>) -> PyResult<Py<PyAny>> {
    let state = hook_state(py)?;
    let key_index = match args.len() {
        4 => 0,
        5 => 1,
        count => {
            return Err(PyTypeError::new_err(format!(
                "_dispatch_hook_trial expected 4 or 5 arguments, got {count}"
            )));
        }
    };
    let key = args.get_item(key_index)?.extract::<String>()?;
    let hooks = args.get_item(key_index + 1)?;
    let hook_data = args.get_item(key_index + 2)?;
    let kwargs = args.get_item(key_index + 3)?.cast_into::<PyDict>()?;
    let empty_hooks = PyDict::new(py);
    let hook_map = if hooks.is_truthy()? {
        hooks.clone()
    } else {
        empty_hooks.into_any()
    };
    let selected = hook_map.call_method1("get", (key.as_str(),))?;
    if !selected.is_truthy()? {
        return Ok(hook_data.unbind());
    }
    let Some(callable) = state.module.bind(py).dict().get_item("Callable")? else {
        let kwargs = PyDict::new(py);
        kwargs.set_item("name", "Callable")?;
        return Err(PyErr::from_value(
            py.get_type::<PyNameError>()
                .call(("name 'Callable' is not defined",), Some(&kwargs))?,
        ));
    };
    let iterable = if selected.is_instance(&callable)? {
        PyList::new(py, [selected])?.into_any()
    } else {
        selected
    };
    let iterator = iterable.try_iter()?.into_any().unbind();
    let owner = OriginHookOwner {
        iterator,
        values: vec![hook_data.unbind()],
        kwargs: kwargs.unbind(),
        handler_error: None,
    };
    let (outcome, mut owner) =
        run_with_owned_actions(py, owner, dispatch_worker, |py, action, owner| {
            execute_action(py, action, owner)
        })?;
    if let Some(error) = owner.handler_error.take() {
        return Err(error);
    }
    match outcome {
        HookOutcome::Complete(value) => owner
            .values
            .get(value.index())
            .map(|value| value.clone_ref(py))
            .ok_or_else(|| PyRuntimeError::new_err("hook dispatch lost its final Python value")),
        HookOutcome::HandlerFailed => Err(PyRuntimeError::new_err(
            "hook dispatch callback failed without preserving its Python exception",
        )),
        HookOutcome::BridgeClosed(error) => Err(bridge_error(error)),
    }
}

#[pyfunction]
fn _deregister_hook_trial(
    subject: &Bound<'_, PyAny>,
    event: &str,
    hook: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    let hooks = subject.getattr("hooks")?;
    let registered = hooks.get_item(event)?;
    match registered.call_method1("remove", (hook,)) {
        Ok(_) => Ok(true),
        Err(error) if error.is_instance_of::<PyValueError>(subject.py()) => Ok(false),
        Err(error) => Err(error),
    }
}

pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    HOOK_STATE.get_or_try_init(module.py(), || initialize_hook_state(module.py()))?;
    module.add_function(wrap_pyfunction!(_dispatch_hook_trial, module)?)?;
    module.add_function(wrap_pyfunction!(_deregister_hook_trial, module)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{HookAction, HookReply, OriginHookOwner};
    use crate::bridge::WorkerPayload;

    fn assert_worker_payload<T: WorkerPayload>() {}

    trait AmbiguousIfWorkerPayload<A> {
        fn marker() {}
    }

    impl<T: ?Sized> AmbiguousIfWorkerPayload<()> for T {}
    impl<T: ?Sized + WorkerPayload> AmbiguousIfWorkerPayload<u8> for T {}

    #[test]
    fn action_payloads_are_explicitly_worker_safe() {
        assert_worker_payload::<HookAction>();
        assert_worker_payload::<HookReply>();
    }

    #[test]
    fn origin_owner_and_python_handles_are_not_worker_payloads() {
        let _ = <OriginHookOwner as AmbiguousIfWorkerPayload<_>>::marker;
        let _ = <pyo3::Py<pyo3::PyAny> as AmbiguousIfWorkerPayload<_>>::marker;
    }
}
