//! BROCODEX: Bro's retry policy for Responses requests.
//!
//! Upstream gives up on capacity, rate, usage, and credential failures after a small budget.
//! Bro's sessions run unattended behind CPA, so these failures are waited out instead:
//!
//! - model at capacity, HTTP 503, HTTP 429 or in-stream rate limits, and CPA's incorrect API key
//!   401 retry forever: 5s, 20s, then 30-120s with jitter, unless the server advises a delay;
//! - resettable usage limits retry forever, at the server's advice or `resets_at`, otherwise
//!   5s doubling;
//! - CPA's transient "unknown provider for model" errors retry five times;
//! - stream disconnects keep retrying after upstream's budget and transport fallback are spent,
//!   every 5s doubling to 60s.
//!
//! No wait exceeds two minutes, even when the server advises longer: a limit can be reset by hand
//! ahead of its advertised time, and CPA advertises a credential's reset as Retry-After.
//!
//! Quota and plan errors stay terminal. A wait re-sends its status as a countdown, so clients
//! keep showing the retry for the whole wait instead of only when it starts.
//!
//! Upstream code calls in through [`retry`] from `responses_retry.rs` and `compact.rs`; sampling,
//! remote compaction, and local compaction all share this policy.

use std::time::Duration;

use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::util::backoff;
use chrono::Utc;
use codex_client::RetryOperation;
use codex_http_client::RetryAfter;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::StreamErrorEvent;
use rand::Rng;
use tokio::time::Instant;
use tracing::warn;

const MIN_RETRY_DELAY: Duration = Duration::from_secs(1);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(2 * 60);
const INITIAL_USAGE_LIMIT_RETRY_DELAY: Duration = Duration::from_secs(5);
const INITIAL_DISCONNECT_RETRY_DELAY: Duration = Duration::from_secs(5);
const MAX_DISCONNECT_RETRY_DELAY: Duration = Duration::from_secs(60);
const MAX_PROVIDER_MODEL_RETRIES: u64 = 5;
/// Countdown updates are frequent near the deadline and sparse during long waits.
const SHORT_WAIT: Duration = Duration::from_secs(60);
const SHORT_WAIT_TICK: Duration = Duration::from_secs(1);
const LONG_WAIT_TICK: Duration = Duration::from_secs(10);

/// Attempt counters for one request loop.
#[derive(Debug, Default)]
pub(crate) struct BroRetryState {
    overload_retries: u64,
    usage_limit_retries: u64,
    provider_model_retries: u64,
    disconnect_retries: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure {
    ModelAtCapacity,
    ServerUnavailable,
    RateLimited,
    IncorrectApiKey,
    UsageLimit,
    ProviderModel,
    Disconnected,
}

impl Failure {
    fn label(self) -> &'static str {
        match self {
            Self::ModelAtCapacity => "Model at capacity",
            Self::ServerUnavailable => "Server unavailable",
            Self::RateLimited => "Rate limited",
            Self::IncorrectApiKey => "Incorrect API key",
            Self::UsageLimit => "Usage limit reached",
            Self::ProviderModel => "Model provider unavailable",
            Self::Disconnected => "Stream disconnected",
        }
    }
}

fn classify(err: &CodexErr) -> Option<Failure> {
    match err.details() {
        CodexErrorDetails::UsageLimitReached(_) => return Some(Failure::UsageLimit),
        CodexErrorDetails::ServerOverloaded => return Some(Failure::ModelAtCapacity),
        CodexErrorDetails::RateLimitExceeded(_) => return Some(Failure::RateLimited),
        CodexErrorDetails::FlexUnavailable => return None,
        CodexErrorDetails::UnexpectedStatus(response)
            if response.status.as_u16() == 401
                && response.body.contains("Incorrect API key provided:") =>
        {
            return Some(Failure::IncorrectApiKey);
        }
        CodexErrorDetails::InvalidRequest(message) if is_provider_model_error(message) => {
            return Some(Failure::ProviderModel);
        }
        _ => {}
    }
    match err.http_status_code_value() {
        Some(429) => Some(Failure::RateLimited),
        Some(503) => Some(Failure::ServerUnavailable),
        _ => match err.details() {
            CodexErrorDetails::Stream(_) | CodexErrorDetails::ResponseStreamFailed(_) => {
                Some(Failure::Disconnected)
            }
            _ => None,
        },
    }
}

/// CPA reports a model whose provider registry is still loading as an invalid request.
fn is_provider_model_error(message: &str) -> bool {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(message) else {
        return false;
    };
    let Some(error) = parsed.get("error") else {
        return false;
    };
    error.get("code").and_then(serde_json::Value::as_str) == Some("model_not_found")
        && error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|message| message.starts_with("unknown provider for model"))
}

impl BroRetryState {
    /// Chooses when to retry `failure`, or `None` when the policy leaves it to upstream.
    fn retry_at(&mut self, failure: Failure, err: &CodexErr, now: Instant) -> Option<Instant> {
        let advised = err.retry_after().map(RetryAfter::deadline);
        let retry_at = match failure {
            Failure::ModelAtCapacity
            | Failure::ServerUnavailable
            | Failure::RateLimited
            | Failure::IncorrectApiKey => {
                self.overload_retries = self.overload_retries.saturating_add(1);
                advised.unwrap_or_else(|| now + overload_delay(self.overload_retries))
            }
            Failure::UsageLimit => {
                self.usage_limit_retries = self.usage_limit_retries.saturating_add(1);
                advised
                    .filter(|deadline| *deadline > now)
                    .or_else(|| usage_limit_reset(err).map(|delay| now + delay))
                    .unwrap_or_else(|| {
                        let exponent = self.usage_limit_retries.saturating_sub(1).min(6);
                        now + INITIAL_USAGE_LIMIT_RETRY_DELAY.saturating_mul(1u32 << exponent)
                    })
            }
            Failure::ProviderModel => {
                if self.provider_model_retries >= MAX_PROVIDER_MODEL_RETRIES {
                    return None;
                }
                self.provider_model_retries += 1;
                now + backoff(self.provider_model_retries)
            }
            Failure::Disconnected => {
                self.disconnect_retries = self.disconnect_retries.saturating_add(1);
                let exponent = self.disconnect_retries.saturating_sub(1).min(4);
                advised.unwrap_or_else(|| {
                    now + INITIAL_DISCONNECT_RETRY_DELAY
                        .saturating_mul(1u32 << exponent)
                        .min(MAX_DISCONNECT_RETRY_DELAY)
                })
            }
        };
        Some(retry_at.clamp(now + MIN_RETRY_DELAY, now + MAX_RETRY_DELAY))
    }

    fn attempts(&self, failure: Failure) -> u64 {
        match failure {
            Failure::ModelAtCapacity
            | Failure::ServerUnavailable
            | Failure::RateLimited
            | Failure::IncorrectApiKey => self.overload_retries,
            Failure::UsageLimit => self.usage_limit_retries,
            Failure::ProviderModel => self.provider_model_retries,
            Failure::Disconnected => self.disconnect_retries,
        }
    }
}

fn overload_delay(attempt: u64) -> Duration {
    match attempt {
        1 => Duration::from_secs(5),
        2 => Duration::from_secs(20),
        _ => Duration::from_secs(rand::rng().random_range(30..=120)),
    }
}

fn usage_limit_reset(err: &CodexErr) -> Option<Duration> {
    let CodexErrorDetails::UsageLimitReached(details) = err.details() else {
        return None;
    };
    (details.resets_at? - Utc::now())
        .to_std()
        .ok()
        .filter(|delay| !delay.is_zero())
}

/// Waits out `err` when Bro's policy retries it, returning whether the caller should retry.
///
/// Disconnects stay with upstream's finite retries and transport fallback until
/// `upstream_exhausted` reports that budget spent.
pub(crate) async fn retry(
    state: &mut BroRetryState,
    err: &CodexErr,
    sess: &Session,
    turn_context: &TurnContext,
    operation: Option<RetryOperation>,
    upstream_exhausted: bool,
) -> bool {
    let Some(failure) = classify(err) else {
        return false;
    };
    if failure == Failure::Disconnected && !upstream_exhausted {
        return false;
    }
    let now = Instant::now();
    let Some(retry_at) = state.retry_at(failure, err, now) else {
        return false;
    };
    let attempt = state.attempts(failure);
    let delay = retry_at.saturating_duration_since(now);
    warn!(
        turn_id = %turn_context.sub_id,
        retries = attempt,
        error = %err,
        ?delay,
        "{}; waiting to retry",
        failure.label()
    );
    if let Some(operation) = operation {
        codex_client::record_retry!(attempt, delay, operation);
    }
    wait_with_countdown(sess, turn_context, err, failure.label(), retry_at).await;
    true
}

async fn wait_with_countdown(
    sess: &Session,
    turn_context: &TurnContext,
    err: &CodexErr,
    label: &str,
    retry_at: Instant,
) {
    loop {
        let remaining = retry_at.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        notify(
            sess,
            turn_context,
            err,
            format!("{label}. Retrying in {}...", format_remaining(remaining)),
        )
        .await;
        let tick = if remaining <= SHORT_WAIT {
            SHORT_WAIT_TICK
        } else {
            LONG_WAIT_TICK
        };
        tokio::time::sleep_until(retry_at.min(Instant::now() + tick)).await;
    }
    notify(sess, turn_context, err, format!("{label}. Retrying now...")).await;
}

/// Mirrors `Session::notify_stream_error` without taking ownership of the error.
async fn notify(sess: &Session, turn_context: &TurnContext, err: &CodexErr, message: String) {
    sess.send_event(
        turn_context,
        EventMsg::StreamError(StreamErrorEvent {
            message,
            codex_error_info: Some(CodexErrorInfo::ResponseStreamDisconnected {
                http_status_code: err.http_status_code_value(),
            }),
            additional_details: Some(err.to_string()),
        }),
    )
    .await;
}

fn format_remaining(remaining: Duration) -> String {
    let seconds = remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0);
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 60 * 60 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60)
    }
}

#[cfg(test)]
#[path = "bro_retry_tests.rs"]
mod tests;
