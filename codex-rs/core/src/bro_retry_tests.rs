use super::BroRetryState;
use super::Failure;
use super::classify;
use super::format_remaining;
use super::retry;
use crate::session::tests::make_session_and_context_with_rx;
use chrono::Utc;
use codex_http_client::RetryAfter;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::RetryLimitReachedError;
use codex_protocol::error::UnexpectedResponseError;
use codex_protocol::error::UsageLimitReachedError;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use http::StatusCode;
use pretty_assertions::assert_eq;
use std::time::Duration;
use tokio::time::Instant;

fn status_error(status: StatusCode, body: &str) -> CodexErr {
    CodexErr::UnexpectedStatus(UnexpectedResponseError {
        status,
        body: body.to_string(),
        user_message: None,
        url: None,
        cf_ray: None,
        request_id: None,
        identity_authorization_error: None,
        identity_error_code: None,
    })
}

fn usage_limit(resets_in: Option<Duration>) -> CodexErr {
    CodexErr::UsageLimitReached(UsageLimitReachedError {
        plan_type: None,
        resets_at: resets_in.map(|delay| {
            Utc::now() + chrono::Duration::from_std(delay).expect("reset delay should fit")
        }),
        limit_window_minutes: None,
        rate_limits: None,
        promo_message: None,
        rate_limit_reached_type: None,
    })
}

fn provider_model_error() -> CodexErr {
    CodexErr::InvalidRequest(
        r#"{"error":{"code":"model_not_found","message":"unknown provider for model gpt-5.5"}}"#
            .to_string(),
    )
}

fn stream_error_messages(rx: &async_channel::Receiver<Event>) -> Vec<String> {
    std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|event| match event.msg {
            EventMsg::StreamError(error) => Some(error.message),
            _ => None,
        })
        .collect()
}

#[test]
fn classifies_waitable_failures() {
    let cases = [
        (CodexErr::ServerOverloaded, Some(Failure::ModelAtCapacity)),
        (
            status_error(StatusCode::SERVICE_UNAVAILABLE, ""),
            Some(Failure::ServerUnavailable),
        ),
        (
            CodexErr::RetryLimit(RetryLimitReachedError {
                status: StatusCode::TOO_MANY_REQUESTS,
                request_id: None,
            }),
            Some(Failure::RateLimited),
        ),
        (
            CodexErr::new(CodexErrorDetails::RateLimitExceeded(
                "Rate limit reached".to_string(),
            )),
            Some(Failure::RateLimited),
        ),
        (
            status_error(
                StatusCode::UNAUTHORIZED,
                "Incorrect API key provided: sk-***",
            ),
            Some(Failure::IncorrectApiKey),
        ),
        (usage_limit(None), Some(Failure::UsageLimit)),
        (provider_model_error(), Some(Failure::ProviderModel)),
        (
            CodexErr::Stream("websocket closed".to_string()),
            Some(Failure::Disconnected),
        ),
        (CodexErr::new(CodexErrorDetails::FlexUnavailable), None),
        (CodexErr::QuotaExceeded, None),
        (CodexErr::UsageNotIncluded, None),
        (
            status_error(StatusCode::UNAUTHORIZED, "token expired"),
            None,
        ),
        (CodexErr::InvalidRequest("bad request".to_string()), None),
        (CodexErr::InternalServerError, None),
    ];
    for (err, expected) in cases {
        assert_eq!(classify(&err), expected, "{err:?}");
    }
}

#[test]
fn overload_backoff_uses_fixed_steps_then_jitter() {
    let mut state = BroRetryState::default();
    let err = CodexErr::ServerOverloaded;
    let now = Instant::now();
    let delays: Vec<_> = (0..12)
        .map(|_| {
            state
                .retry_at(Failure::ModelAtCapacity, &err, now)
                .expect("overloads always retry")
                - now
        })
        .collect();
    assert_eq!(
        delays[..2],
        [Duration::from_secs(5), Duration::from_secs(20)]
    );
    for delay in &delays[2..] {
        assert!(
            (Duration::from_secs(30)..=Duration::from_secs(120)).contains(delay),
            "{delay:?}"
        );
    }
}

#[test]
fn server_advice_overrides_backoff_with_a_floor() {
    let mut state = BroRetryState::default();
    let now = Instant::now();
    let advised = RetryAfter::from_delay(Duration::from_secs(90)).expect("retry deadline");
    let err = CodexErr::ServerOverloaded.with_retry_after(advised);
    assert_eq!(
        state.retry_at(Failure::ModelAtCapacity, &err, now),
        Some(advised.deadline().max(now + Duration::from_secs(1)))
    );

    // CPA advertises a cooling credential's reset, which may be reset by hand sooner.
    let advised = RetryAfter::from_delay(Duration::from_secs(5 * 3600)).expect("retry deadline");
    let err = CodexErr::RetryLimit(RetryLimitReachedError {
        status: StatusCode::TOO_MANY_REQUESTS,
        request_id: None,
    })
    .with_retry_after(advised);
    let now = Instant::now();
    assert_eq!(
        state.retry_at(Failure::RateLimited, &err, now),
        Some(now + Duration::from_secs(120))
    );

    let expired = RetryAfter::from_delay(Duration::ZERO).expect("retry deadline");
    let err = CodexErr::ServerOverloaded.with_retry_after(expired);
    let now = Instant::now();
    assert_eq!(
        state.retry_at(Failure::ModelAtCapacity, &err, now),
        Some(now + Duration::from_secs(1))
    );
}

#[test]
fn usage_limits_wait_for_advice_then_reset_then_backoff() {
    let mut state = BroRetryState::default();
    let now = Instant::now();
    let advised = RetryAfter::from_delay(Duration::from_secs(30)).expect("retry deadline");
    let err = usage_limit(Some(Duration::from_secs(3600))).with_retry_after(advised);
    assert_eq!(
        state.retry_at(Failure::UsageLimit, &err, now),
        Some(advised.deadline())
    );

    let reset_delay = state
        .retry_at(
            Failure::UsageLimit,
            &usage_limit(Some(Duration::from_secs(90))),
            now,
        )
        .expect("usage limits always retry")
        - now;
    assert!(
        (Duration::from_secs(80)..=Duration::from_secs(90)).contains(&reset_delay),
        "{reset_delay:?}"
    );

    // A manual reset ahead of the advertised time is noticed within two minutes.
    assert_eq!(
        state.retry_at(
            Failure::UsageLimit,
            &usage_limit(Some(Duration::from_secs(5 * 3600))),
            now,
        ),
        Some(now + Duration::from_secs(120))
    );

    let mut state = BroRetryState::default();
    let delays: Vec<_> = (0..9)
        .map(|_| {
            state
                .retry_at(Failure::UsageLimit, &usage_limit(None), now)
                .expect("usage limits always retry")
                - now
        })
        .collect();
    assert_eq!(
        delays,
        [5, 10, 20, 40, 80, 120, 120, 120, 120].map(Duration::from_secs)
    );
}

#[test]
fn provider_model_errors_retry_five_times() {
    let mut state = BroRetryState::default();
    let err = provider_model_error();
    let now = Instant::now();
    for _ in 0..5 {
        assert!(state.retry_at(Failure::ProviderModel, &err, now).is_some());
    }
    assert_eq!(state.retry_at(Failure::ProviderModel, &err, now), None);
}

#[test]
fn disconnect_backoff_doubles_to_a_minute() {
    let mut state = BroRetryState::default();
    let err = CodexErr::Stream("websocket closed".to_string());
    let now = Instant::now();
    let delays: Vec<_> = (0..7)
        .map(|_| {
            state
                .retry_at(Failure::Disconnected, &err, now)
                .expect("disconnects always retry")
                - now
        })
        .collect();
    assert_eq!(delays, [5, 10, 20, 40, 60, 60, 60].map(Duration::from_secs));
}

#[test]
fn formats_countdown() {
    let cases = [
        (Duration::from_millis(200), "1s"),
        (Duration::from_secs(59), "59s"),
        (Duration::from_millis(59_100), "1m 00s"),
        (Duration::from_secs(605), "10m 05s"),
        (Duration::from_secs(3 * 3600 + 7 * 60 + 20), "3h 07m"),
    ];
    for (remaining, expected) in cases {
        assert_eq!(format_remaining(remaining), expected);
    }
}

/// The status is re-sent while waiting so clients keep showing it for the whole wait.
#[tokio::test]
async fn wait_repeats_countdown_until_the_deadline() {
    let (session, turn_context, rx) = make_session_and_context_with_rx().await;
    while rx.try_recv().is_ok() {}
    tokio::time::pause();
    let advised = RetryAfter::from_delay(Duration::from_secs(3)).expect("retry deadline");
    let err = CodexErr::RetryLimit(RetryLimitReachedError {
        status: StatusCode::TOO_MANY_REQUESTS,
        request_id: None,
    })
    .with_retry_after(advised);
    let mut state = BroRetryState::default();

    let retried = retry(
        &mut state,
        &err,
        &session,
        &turn_context,
        /*operation*/ None,
        /*upstream_exhausted*/ false,
    )
    .await;

    assert!(retried);
    assert!(Instant::now() >= advised.deadline());
    assert_eq!(
        stream_error_messages(&rx),
        [
            "Rate limited. Retrying in 3s...",
            "Rate limited. Retrying in 2s...",
            "Rate limited. Retrying in 1s...",
            "Rate limited. Retrying now...",
        ]
    );
}

#[tokio::test]
async fn leaves_terminal_errors_and_early_disconnects_to_upstream() {
    let (session, turn_context, rx) = make_session_and_context_with_rx().await;
    while rx.try_recv().is_ok() {}
    let mut state = BroRetryState::default();
    for (err, upstream_exhausted) in [
        (CodexErr::QuotaExceeded, true),
        (CodexErr::InternalServerError, true),
        (CodexErr::Stream("websocket closed".to_string()), false),
    ] {
        assert!(
            !retry(
                &mut state,
                &err,
                &session,
                &turn_context,
                /*operation*/ None,
                upstream_exhausted,
            )
            .await
        );
    }
    assert_eq!(stream_error_messages(&rx), Vec::<String>::new());
}

#[tokio::test]
async fn disconnects_retry_after_upstream_budget() {
    let (session, turn_context, rx) = make_session_and_context_with_rx().await;
    while rx.try_recv().is_ok() {}
    tokio::time::pause();
    let mut state = BroRetryState::default();
    let started = Instant::now();

    let retried = retry(
        &mut state,
        &CodexErr::Stream("websocket closed".to_string()),
        &session,
        &turn_context,
        /*operation*/ None,
        /*upstream_exhausted*/ true,
    )
    .await;

    assert!(retried);
    assert!(Instant::now() - started >= Duration::from_secs(5));
    let messages = stream_error_messages(&rx);
    assert_eq!(
        messages.first().map(String::as_str),
        Some("Stream disconnected. Retrying in 5s...")
    );
    assert_eq!(
        messages.last().map(String::as_str),
        Some("Stream disconnected. Retrying now...")
    );
}
