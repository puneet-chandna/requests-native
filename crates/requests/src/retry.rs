use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetryCount {
    Unlimited,
    Boolean(bool),
    Limited(u32),
}

impl RetryCount {
    fn decrement(self) -> Option<Self> {
        match self {
            Self::Unlimited => Some(Self::Unlimited),
            Self::Boolean(true) => Some(Self::Limited(0)),
            Self::Boolean(false) | Self::Limited(0) => None,
            Self::Limited(remaining) => Some(Self::Limited(remaining - 1)),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MethodSet(BTreeSet<String>);

impl MethodSet {
    pub fn new(methods: impl IntoIterator<Item = String>) -> Self {
        Self(
            methods
                .into_iter()
                .map(|method| method.to_ascii_uppercase())
                .collect(),
        )
    }

    fn allows(&self, method: &str) -> bool {
        self.0.is_empty() || self.0.contains(&method.to_ascii_uppercase())
    }

    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusSet(BTreeSet<u16>);

impl StatusSet {
    pub fn new(statuses: impl IntoIterator<Item = u16>) -> Self {
        Self(statuses.into_iter().collect())
    }

    fn contains(&self, status: u16) -> bool {
        self.0.contains(&status)
    }

    pub fn iter(&self) -> impl Iterator<Item = u16> + '_ {
        self.0.iter().copied()
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BackoffPolicy {
    pub factor: f64,
    pub maximum: Option<f64>,
    pub jitter: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RetryPolicy {
    pub total: RetryCount,
    pub connect: RetryCount,
    pub read: RetryCount,
    pub status: RetryCount,
    pub redirect: RetryCount,
    pub other: RetryCount,
    pub allowed_methods: Option<MethodSet>,
    pub status_forcelist: StatusSet,
    pub backoff: BackoffPolicy,
    pub respect_retry_after: bool,
    pub raise_on_status: bool,
    pub raise_on_redirect: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetryReason {
    Connect,
    Read,
    Status { status: u16 },
    Redirect { status: u16 },
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetryHistory {
    pub reason: RetryReason,
    pub method: String,
    pub url: String,
    pub redirect_location: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetryFailure {
    Exhausted { reason: RetryReason },
}

#[derive(Clone, Debug, PartialEq)]
pub struct RetryState {
    remaining: RetryPolicy,
    history: Vec<RetryHistory>,
}

impl RetryState {
    pub fn new(policy: RetryPolicy) -> Self {
        Self {
            remaining: policy,
            history: Vec::new(),
        }
    }

    pub fn with_history(policy: RetryPolicy, history: Vec<RetryHistory>) -> Self {
        Self {
            remaining: policy,
            history,
        }
    }

    pub fn remaining(&self) -> &RetryPolicy {
        &self.remaining
    }

    pub fn history(&self) -> &[RetryHistory] {
        &self.history
    }

    pub fn allows_method(&self, method: &str) -> bool {
        self.remaining
            .allowed_methods
            .as_ref()
            .is_none_or(|methods| methods.allows(method))
    }

    pub fn is_retry(&self, method: &str, status: u16, has_retry_after: bool) -> bool {
        self.allows_method(method)
            && (self.remaining.status_forcelist.contains(status)
                || (self.remaining.respect_retry_after
                    && has_retry_after
                    && matches!(status, 413 | 429 | 503)))
    }

    pub fn increment(
        &self,
        reason: RetryReason,
        method: &str,
        url: &str,
        redirect_location: Option<&str>,
    ) -> Result<Self, RetryFailure> {
        let consumed_reason = match (reason, redirect_location) {
            (RetryReason::Status { status }, Some(_)) => RetryReason::Redirect { status },
            _ => reason,
        };
        let mut remaining = self.remaining.clone();
        remaining.total = remaining.total.decrement().ok_or(RetryFailure::Exhausted {
            reason: consumed_reason,
        })?;
        let counter = match consumed_reason {
            RetryReason::Connect => &mut remaining.connect,
            RetryReason::Read => &mut remaining.read,
            RetryReason::Status { .. } => &mut remaining.status,
            RetryReason::Redirect { .. } => &mut remaining.redirect,
            RetryReason::Other => &mut remaining.other,
        };
        *counter = counter.decrement().ok_or(RetryFailure::Exhausted {
            reason: consumed_reason,
        })?;

        let mut history = self.history.clone();
        history.push(RetryHistory {
            reason: consumed_reason,
            method: method.to_owned(),
            url: url.to_owned(),
            redirect_location: redirect_location.map(str::to_owned),
        });
        Ok(Self { remaining, history })
    }

    pub fn backoff(&self, random_unit: f64) -> f64 {
        let consecutive_errors = self
            .history
            .iter()
            .rev()
            .take_while(|item| !matches!(item.reason, RetryReason::Redirect { .. }))
            .count();
        if consecutive_errors <= 1 {
            return 0.0;
        }
        let exponent = i32::try_from(consecutive_errors - 1).unwrap_or(i32::MAX);
        let backoff = self.remaining.backoff.factor * 2_f64.powi(exponent)
            + self.remaining.backoff.jitter * random_unit.clamp(0.0, 1.0);
        self.remaining
            .backoff
            .maximum
            .map_or(backoff, |maximum| backoff.min(maximum))
            .max(0.0)
    }
}
